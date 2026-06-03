//! Permissions decode — the one `hyp-control sync` target that is NOT a contract table.
//!
//! Section `eosio::chain::permission_object` rows are `snapshot_permission_object`
//! (FC_REFLECT `(parent)(owner)(name)(last_updated)(last_used)(auth)`):
//!   parent(name u64) | owner(name u64) | name(name u64) | last_updated(i64 µs) | last_used(i64 µs) | authority
//! authority = threshold(u32) | varuint Nk ×(public_key + weight u16)
//!           | varuint Na ×(permission_level{actor u64, perm u64} + weight u16 = 18B)
//!           | varuint Nw ×(wait_sec u32 + weight u16 = 6B)
//! Section `eosio::chain::permission_link_object` rows = account|code|message_type|required_permission (4 names, 32B).
//!
//! `required_auth` and `last_updated` are rendered via the eosio ABI's `authority` / built-in `time_point`
//! types (abieos yields `PUB_K1_…` key strings + ISO timestamps); falls back to hex/µs if unavailable.
//! Emits Hyperion `IPermission`-shaped docs. Decoupled from the contract-table pipeline (different decode).

use std::collections::HashMap;
use std::io::Write;

use anyhow::{anyhow, bail, Result};
use rs_abieos::{AbiHandle, Abieos};
use serde_json::{json, Value};

use crate::map::{COLL_PERMISSIONS, COLL_PUB_KEYS};
use crate::mongo::SinkItem;
use crate::reader::{find, Section, SnapRead};

#[derive(Default, Debug)]
pub struct PermStats {
    pub permissions: u64,
    pub links: u64,
    pub auth_decoded: u64,
    pub auth_fallback: u64,
}

fn read_varuint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    let mut shift = 0u32;
    let mut i = 0usize;
    loop {
        let byte = *b.get(i)?;
        i += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((v, i));
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Byte length of a serialized fc `public_key` (variant `[u8 type][data]`). All offset arithmetic
/// is checked — `slen` is an untrusted varuint and could otherwise wrap `usize`.
fn public_key_len(b: &[u8]) -> Option<usize> {
    match b.first()? {
        0 | 1 => Some(1 + 33), // K1 / R1: 33-byte compressed point
        2 => {
            // WebAuthn: 33-byte key + u8 user_presence + string rpid
            let o = 1 + 33 + 1;
            let (slen, k) = read_varuint(b.get(o..)?)?;
            // o + k + slen, all checked.
            o.checked_add(k)?.checked_add(usize::try_from(slen).ok()?)
        }
        _ => None,
    }
}

/// Byte length of a serialized `authority`. `nk`/`na`/`nw` are untrusted varuint counts, so all
/// offset math uses checked add/mul (returning None on overflow — the caller already bails).
fn authority_len(b: &[u8]) -> Option<usize> {
    let mut o = 4usize; // threshold u32
    let (nk, k) = read_varuint(b.get(o..)?)?;
    o = o.checked_add(k)?;
    for _ in 0..nk {
        o = o.checked_add(public_key_len(b.get(o..)?)?)?;
        o = o.checked_add(2)?; // weight u16
    }
    let (na, k) = read_varuint(b.get(o..)?)?;
    // permission_level(16) + weight(2) = 18 bytes per entry.
    let na_bytes = usize::try_from(na).ok()?.checked_mul(18)?;
    o = o.checked_add(k)?.checked_add(na_bytes)?;
    let (nw, k) = read_varuint(b.get(o..)?)?;
    // wait_sec(4) + weight(2) = 6 bytes per entry.
    let nw_bytes = usize::try_from(nw).ok()?.checked_mul(6)?;
    o = o.checked_add(k)?.checked_add(nw_bytes)?;
    Some(o)
}

fn read_section_bytes(s: &mut impl SnapRead, sec: &Section) -> Result<Vec<u8>> {
    // NOTE: reads the whole section into memory (Telos ~188 MB, FIO ~406 MB — fine; for EOS-scale a
    // streaming parse would be better, but the permission sections are far smaller than contract_tables).
    s.seek_to(sec.payload_off)?;
    let mut buf = vec![0u8; sec.payload_len as usize];
    s.read_buf(&mut buf)?;
    Ok(buf)
}

/// Seek path: read both native sections fully into `Vec`s (`permission_link_object` first, then
/// `permission_object` — they are read in either order on a seekable file), then delegate to the
/// shared buffer-driven decoder. Byte-identical to the previous inline body (pure extraction).
#[allow(clippy::too_many_arguments)]
pub fn decode_permissions(
    s: &mut impl SnapRead,
    secs: &[Section],
    abi_raw: &HashMap<u64, Vec<u8>>,
    names: &Abieos,
    eosio: u64,
    block_num: u32,
    out: &mut dyn Write,
    limit: Option<u64>,
    stats_only: bool,
) -> Result<PermStats> {
    let perm_sec = find(secs, "eosio::chain::permission_object")
        .ok_or_else(|| anyhow!("no permission_object section"))?;
    let link_sec = find(secs, "eosio::chain::permission_link_object")
        .ok_or_else(|| anyhow!("no permission_link_object section"))?;

    let lbuf = read_section_bytes(s, link_sec)?;
    let pbuf = read_section_bytes(s, perm_sec)?;
    decode_permissions_bufs(
        &lbuf,
        link_sec.rows,
        &pbuf,
        perm_sec.rows,
        abi_raw,
        names,
        eosio,
        block_num,
        out,
        limit,
        stats_only,
    )
}

/// Buffer-driven permissions decoder shared by the seek path and the streaming forward pass.
/// Both native sections are already fully materialised into `lbuf` / `pbuf` before this is called.
/// In writer order `permission_object` precedes `permission_link_object`, but the link map must be
/// built first; the stream driver buffers the permission_object payload until the link section
/// arrives, then calls this with both buffers in hand (see `main.rs::run_stream`).
#[allow(clippy::too_many_arguments)]
pub fn decode_permissions_bufs(
    lbuf: &[u8],
    link_rows: u64,
    pbuf: &[u8],
    perm_rows: u64,
    abi_raw: &HashMap<u64, Vec<u8>>,
    names: &Abieos,
    eosio: u64,
    block_num: u32,
    out: &mut dyn Write,
    limit: Option<u64>,
    stats_only: bool,
) -> Result<PermStats> {
    let mut st = PermStats::default();

    // 1. permission_link_object (4 names, 32B/row) -> (account, required_permission) -> [(code, action)]
    if lbuf.len() as u64 != link_rows * 32 {
        bail!(
            "permission_link_object: expected rows*32 = {} bytes but section payload is {}",
            link_rows * 32,
            lbuf.len()
        );
    }
    let mut links: HashMap<(u64, u64), Vec<(u64, u64)>> = HashMap::new();
    for i in 0..link_rows as usize {
        let o = i * 32;
        let f = |j: usize| u64::from_le_bytes(lbuf[o + j..o + j + 8].try_into().unwrap());
        let (account, code, message_type, required_permission) = (f(0), f(8), f(16), f(24));
        links
            .entry((account, required_permission))
            .or_default()
            .push((code, message_type));
        st.links += 1;
    }

    // 2. eosio ABI for rendering authority + time_point (fallback to hex/µs if unavailable)
    let mut eosio_abi: Option<AbiHandle> = abi_raw
        .get(&eosio)
        .and_then(|b| AbiHandle::from_bin(b).ok());

    // 3. permission_object rows = snapshot_permission_object
    let n = |v: u64| names.name_to_string(v).unwrap_or_else(|_| v.to_string());
    let mut o = 0usize;
    let mut auth_json = String::new();
    let mut ts_json = String::new();
    let mut broke_early = false;

    for _ in 0..perm_rows {
        // overflow-safe form of `o + 40 > pbuf.len()` (o is an accumulated, untrusted offset).
        if pbuf.len().saturating_sub(o) < 40 {
            bail!("permission_object: truncated fixed fields at offset {o}");
        }
        let g = |j: usize| u64::from_le_bytes(pbuf[o + j..o + j + 8].try_into().unwrap());
        let (parent, owner, name) = (g(0), g(8), g(16));
        let last_updated = &pbuf[o + 24..o + 32]; // time_point i64 µs LE  (last_used at +32..+40, unused)
        let auth_off = o + 40;
        let alen = authority_len(
            pbuf.get(auth_off..)
                .ok_or_else(|| anyhow!("authority oob"))?,
        )
        .ok_or_else(|| anyhow!("bad authority at offset {auth_off}"))?;
        let auth_bytes = pbuf
            .get(auth_off..auth_off + alen)
            .ok_or_else(|| anyhow!("authority slice oob at {auth_off}"))?;
        o = auth_off + alen;
        // skip the chainbase null/sentinel permission (id 0: empty owner/name, threshold 0)
        if owner == 0 && name == 0 {
            continue;
        }
        st.permissions += 1;

        // decode authority (always — feeds both stats and the doc)
        let auth_ok = eosio_abi.as_mut().is_some_and(|h| {
            h.bin_to_json_into("authority", auth_bytes, &mut auth_json)
                .is_ok()
        });
        if auth_ok {
            st.auth_decoded += 1;
        } else {
            st.auth_fallback += 1;
        }

        if !stats_only {
            let ts_ok = eosio_abi.as_mut().is_some_and(|h| {
                h.bin_to_json_into("time_point", last_updated, &mut ts_json)
                    .is_ok()
            });
            write!(out, "{{\"block_num\":{block_num},\"last_updated\":")?;
            if ts_ok {
                write!(out, "{ts_json}")?;
            } else {
                write!(
                    out,
                    "{}",
                    i64::from_le_bytes(last_updated.try_into().unwrap())
                )?;
            }
            write!(
                out,
                ",\"account\":\"{}\",\"perm_name\":\"{}\",\"parent\":\"{}\",\"required_auth\":",
                n(owner),
                n(name),
                n(parent)
            )?;
            if auth_ok {
                write!(out, "{auth_json}")?;
            } else {
                write!(out, "\"{}\"", hex::encode(auth_bytes))?;
            }
            write!(out, ",\"linked_actions\":[")?;
            if let Some(la) = links.get(&(owner, name)) {
                for (i, (code, action)) in la.iter().enumerate() {
                    if i > 0 {
                        write!(out, ",")?;
                    }
                    write!(
                        out,
                        "{{\"account\":\"{}\",\"action\":\"{}\"}}",
                        n(*code),
                        n(*action)
                    )?;
                }
            }
            writeln!(out, "],\"present\":true}}")?;
        }

        if matches!(limit, Some(l) if st.permissions >= l) {
            broke_early = true;
            break;
        }
    }

    if !broke_early && o != pbuf.len() {
        bail!(
            "permission_object walk DESYNC: consumed {} of {} bytes",
            o,
            pbuf.len()
        );
    }
    Ok(st)
}

/// Mongo variant of [`decode_permissions`]: emits structured `permissions` docs plus the `pub_keys`
/// reverse index into the shared sink channel. Reuses the same low-level walk (parse helpers above);
/// the NDJSON path is left byte-for-byte untouched. Each authority key entry carries BOTH the modern
/// `public_key` (`PUB_…`) and the legacy `pubkey` (`EOS…`) forms so cc32d9 `/key` matches either.
#[allow(clippy::too_many_arguments)]
pub fn decode_permissions_mongo(
    s: &mut impl SnapRead,
    secs: &[Section],
    abi_raw: &HashMap<u64, Vec<u8>>,
    names: &Abieos,
    eosio: u64,
    block_num: u32,
    tx: &crossbeam_channel::Sender<SinkItem>,
    limit: Option<u64>,
) -> Result<PermStats> {
    let perm_sec = find(secs, "eosio::chain::permission_object")
        .ok_or_else(|| anyhow!("no permission_object section"))?;
    let link_sec = find(secs, "eosio::chain::permission_link_object")
        .ok_or_else(|| anyhow!("no permission_link_object section"))?;
    let lbuf = read_section_bytes(s, link_sec)?;
    let pbuf = read_section_bytes(s, perm_sec)?;

    let mut st = PermStats::default();

    // 1. links: (account, required_permission) -> [(code, action)]
    if lbuf.len() as u64 != link_sec.rows * 32 {
        bail!(
            "permission_link_object: expected rows*32 = {} bytes but section payload is {}",
            link_sec.rows * 32,
            lbuf.len()
        );
    }
    let mut links: HashMap<(u64, u64), Vec<(u64, u64)>> = HashMap::new();
    for i in 0..link_sec.rows as usize {
        let o = i * 32;
        let f = |j: usize| u64::from_le_bytes(lbuf[o + j..o + j + 8].try_into().unwrap());
        let (account, _code, _msg, required_permission) = (f(0), f(8), f(16), f(24));
        links
            .entry((account, required_permission))
            .or_default()
            .push((f(8), f(16)));
        st.links += 1;
    }

    let mut eosio_abi: Option<AbiHandle> = abi_raw
        .get(&eosio)
        .and_then(|b| AbiHandle::from_bin(b).ok());
    let n = |v: u64| names.name_to_string(v).unwrap_or_else(|_| v.to_string());

    let mut o = 0usize;
    let mut auth_json = String::new();
    let mut ts_json = String::new();
    let mut broke_early = false;

    for _ in 0..perm_sec.rows {
        if pbuf.len().saturating_sub(o) < 40 {
            bail!("permission_object: truncated fixed fields at offset {o}");
        }
        let g = |j: usize| u64::from_le_bytes(pbuf[o + j..o + j + 8].try_into().unwrap());
        let (parent, owner, name) = (g(0), g(8), g(16));
        let last_updated = &pbuf[o + 24..o + 32];
        let auth_off = o + 40;
        let alen = authority_len(
            pbuf.get(auth_off..)
                .ok_or_else(|| anyhow!("authority oob"))?,
        )
        .ok_or_else(|| anyhow!("bad authority at offset {auth_off}"))?;
        let auth_bytes = pbuf
            .get(auth_off..auth_off + alen)
            .ok_or_else(|| anyhow!("authority slice oob at {auth_off}"))?;
        o = auth_off + alen;
        if owner == 0 && name == 0 {
            continue;
        }
        st.permissions += 1;

        // required_auth: decoded authority Value, or hex string on failure.
        let auth_ok = eosio_abi.as_mut().is_some_and(|h| {
            h.bin_to_json_into("authority", auth_bytes, &mut auth_json)
                .is_ok()
        });
        let mut required_auth: Value = if auth_ok {
            st.auth_decoded += 1;
            serde_json::from_str(&auth_json).unwrap_or_else(|_| json!(hex::encode(auth_bytes)))
        } else {
            st.auth_fallback += 1;
            json!(hex::encode(auth_bytes))
        };

        let account = n(owner);
        let perm_name = n(name);

        // Enrich each key with dual forms and emit pub_keys docs.
        if let Some(keys) = required_auth.get_mut("keys").and_then(Value::as_array_mut) {
            for k in keys.iter_mut() {
                let modern = k
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let legacy = crate::keyfmt::k1_to_legacy(&modern);
                let stored = legacy.clone().unwrap_or_else(|| modern.clone());
                if let Value::Object(m) = k {
                    m.insert("public_key".into(), json!(modern));
                    m.insert("pubkey".into(), json!(stored));
                }
                let pk = json!({
                    "account": account,
                    "perm": perm_name,
                    "block_num": block_num,
                    "present": true,
                    "key": stored,
                    "key_pub": modern,
                });
                if let Ok(d) = mongodb::bson::to_document(&pk) {
                    let _ = tx.send((COLL_PUB_KEYS, d));
                }
            }
        }

        // last_updated: ISO string via the eosio ABI, else raw µs i64.
        let ts_ok = eosio_abi.as_mut().is_some_and(|h| {
            h.bin_to_json_into("time_point", last_updated, &mut ts_json)
                .is_ok()
        });
        let last_updated_val: Value = if ts_ok {
            serde_json::from_str(&ts_json)
                .unwrap_or_else(|_| json!(i64::from_le_bytes(last_updated.try_into().unwrap())))
        } else {
            json!(i64::from_le_bytes(last_updated.try_into().unwrap()))
        };

        let linked: Vec<Value> = links
            .get(&(owner, name))
            .map(|la| {
                la.iter()
                    .map(|(code, action)| json!({"account": n(*code), "action": n(*action)}))
                    .collect()
            })
            .unwrap_or_default();

        let doc = json!({
            "block_num": block_num,
            "last_updated": last_updated_val,
            "account": account,
            "perm_name": perm_name,
            "parent": n(parent),
            "required_auth": required_auth,
            "linked_actions": linked,
            "present": true,
        });
        if let Ok(d) = mongodb::bson::to_document(&doc) {
            let _ = tx.send((COLL_PERMISSIONS, d));
        }

        if matches!(limit, Some(l) if st.permissions >= l) {
            broke_early = true;
            break;
        }
    }

    if !broke_early && o != pbuf.len() {
        bail!(
            "permission_object walk DESYNC: consumed {} of {} bytes",
            o,
            pbuf.len()
        );
    }
    Ok(st)
}
