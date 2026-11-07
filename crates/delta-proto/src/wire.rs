//! Wire (sysio) KV deltas, shaped exactly like Hyperion 4.5's Wire delta documents.
//!
//! Wire stores contract state as `contract_row_kv` / `contract_index_kv` rows keyed by a numeric
//! `table_id` instead of Antelope's `contract_row`. This path mirrors Hyperion 4.5
//! (`processDeltas` / `processProtocolRecord` in the deserializer) and the Wire plugin's
//! `normalizeDelta`:
//!
//! - every row of every delta stream in a block advances one `ordinal`, starting at 0;
//! - a KV row resolves its contract ABI from the account's latest block-final ABI at or before
//!   the block (`<block_id>:<account>:<sha256(abi)>` is the deployment id);
//! - `table_id` maps to exactly one ABI table (rows) or one secondary index (index records);
//! - present rows become documents; primary rows decode their value with the Wire codec and run
//!   Hyperion's system table handlers, index records carry `data: {}`;
//! - `primary_key` (`h45k1:`) and `_id` (`h45h1:`) use the length-framed sha256 identities from
//!   Hyperion's `protocol-document-id.ts`.
//!
//! Rows Hyperion would reject (no ABI, unmapped table, undecodable value, failing handler) are
//! counted and reported, never silently dropped.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use hyperion_ship::blocks::BlockLog;
use hyperion_ship::delta::read_varuint;
use hyperion_ship::disk::{canonical_offsets, decode_payload};
use hyperion_ship::provider::TimestampProfile;
use hyperion_ship::trace::name_to_string;
use hyperion_wire_protocol::codec::{CodecProfile, decode_value};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};

/// `protocol.id` of documents written by Hyperion's Wire provider.
pub const PROTOCOL_ID: &str = "@eosrio/hyperion-wire-plugin";

pub struct Config {
    pub log_path: String,
    pub idx_path: String,
    pub first_block: u32,
    pub start: u32,
    pub end: u32,
    pub threads: u32,
    pub abi_catalog: String,
    pub blocks_dir: String,
    pub out: Option<String>,
    pub es: Option<String>,
    pub chain: String,
    pub index_version: String,
    pub partition_size: u64,
    pub es_batch: usize,
    pub es_workers: usize,
    pub profile: CodecProfile,
    pub timestamps: TimestampProfile,
    /// `protocol.profileId` written into documents (the indexer's configured provider profile).
    pub doc_profile_id: String,
    pub chain_id: String,
    pub system_contract: String,
}

// ---------------------------------------------------------------------------------------------
// ABI deployments
// ---------------------------------------------------------------------------------------------

/// One ABI table (or secondary index) a numeric `table_id` maps to.
struct TableMap {
    name: String,
    primary_id: u64,
    value_type: String,
    /// `(secondary index name, key type)` for index records.
    secondary: Option<(String, String)>,
}

struct Deployment {
    block: u32,
    id: String,
    /// `None` when the account had no ABI at this point.
    abi: Option<Value>,
    rows: HashMap<u64, Vec<TableMap>>,
    indexes: HashMap<u64, Vec<TableMap>>,
}

type Catalog = HashMap<u64, Vec<Deployment>>;

/// Standard sysio `name` encoding (the inverse of `name_to_string`).
fn string_to_name(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 13 {
        return None;
    }
    let mut value = 0u64;
    for (i, c) in s.bytes().enumerate() {
        let sym = match c {
            b'.' => 0u64,
            b'1'..=b'5' => (c - b'1' + 1) as u64,
            b'a'..=b'z' => (c - b'a' + 6) as u64,
            _ => return None,
        };
        if i < 12 {
            value |= sym << (64 - 5 * (i + 1));
        } else {
            if sym > 0x0f {
                return None;
            }
            value |= sym;
        }
    }
    Some(value)
}

fn load_catalog(path: &str) -> Result<Catalog> {
    let f = BufReader::new(File::open(path).with_context(|| format!("open {path}"))?);
    let mut catalog: Catalog = HashMap::new();
    for line in f.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line).context("ABI catalog line")?;
        if v.get("phase").and_then(Value::as_str) != Some("block-final") {
            continue;
        }
        let account = v["account"].as_str().context("catalog account")?;
        let code = string_to_name(account).with_context(|| format!("catalog account name {account}"))?;
        let block = u32::try_from(v["blockNum"].as_u64().context("catalog blockNum")?)?;
        let block_id = v["blockId"].as_str().context("catalog blockId")?;
        let abi_hex = v.get("abiHex").and_then(Value::as_str).unwrap_or("");
        let present = v.get("present").and_then(Value::as_bool).unwrap_or(true);
        let (abi, id) = if present && !abi_hex.is_empty() {
            let bytes = hex::decode(abi_hex).context("catalog abiHex")?;
            let hash = hex::encode(Sha256::digest(&bytes));
            (Some(v["abi"].clone()), format!("{block_id}:{account}:{hash}"))
        } else {
            (None, String::new())
        };
        let mut rows: HashMap<u64, Vec<TableMap>> = HashMap::new();
        let mut indexes: HashMap<u64, Vec<TableMap>> = HashMap::new();
        if let Some(tables) = abi.as_ref().and_then(|a| a.get("tables")).and_then(Value::as_array) {
            for t in tables {
                let (Some(name), Some(ty), Some(tid)) = (
                    t.get("name").and_then(Value::as_str),
                    t.get("type").and_then(Value::as_str),
                    t.get("table_id").and_then(Value::as_u64),
                ) else {
                    continue;
                };
                rows.entry(tid).or_default().push(TableMap {
                    name: name.to_owned(),
                    primary_id: tid,
                    value_type: ty.to_owned(),
                    secondary: None,
                });
                for idx in t.get("secondary_indexes").and_then(Value::as_array).into_iter().flatten() {
                    let (Some(iname), Some(kt), Some(iid)) = (
                        idx.get("name").and_then(Value::as_str),
                        idx.get("key_type").and_then(Value::as_str),
                        idx.get("table_id").and_then(Value::as_u64),
                    ) else {
                        continue;
                    };
                    indexes.entry(iid).or_default().push(TableMap {
                        name: name.to_owned(),
                        primary_id: tid,
                        value_type: ty.to_owned(),
                        secondary: Some((iname.to_owned(), kt.to_owned())),
                    });
                }
            }
        }
        let list = catalog.entry(code).or_default();
        list.retain(|d| d.block != block); // a later line for the same block supersedes
        list.push(Deployment { block, id, abi, rows, indexes });
    }
    for list in catalog.values_mut() {
        list.sort_by_key(|d| d.block);
    }
    Ok(catalog)
}

/// The account's latest block-final ABI at or before `block` (it applies from ordinal 0).
fn select(catalog: &Catalog, code: u64, block: u32) -> Option<&Deployment> {
    let list = catalog.get(&code)?;
    let pos = list.partition_point(|d| d.block <= block);
    if pos == 0 {
        return None;
    }
    let d = &list[pos - 1];
    d.abi.as_ref().map(|_| d)
}

// ---------------------------------------------------------------------------------------------
// Identities (protocol-document-id.ts)
// ---------------------------------------------------------------------------------------------

/// sha256 over `[u64 BE byte length][utf8 bytes]` per part.
fn framed_sha256(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p.as_bytes());
    }
    hex::encode(h.finalize())
}

fn state_key(chain_id: &str, identity: &str) -> String {
    format!("h45k1:{}", framed_sha256(&["hyperion-state-key-v1", PROTOCOL_ID, chain_id, identity]))
}

fn document_id(chain_id: &str, identity: &str, block_id: &str, block_num: u32, ordinal: u32) -> String {
    let (b, o) = (block_num.to_string(), ordinal.to_string());
    format!(
        "h45h1:{}",
        framed_sha256(&["hyperion-history-document-v1", PROTOCOL_ID, chain_id, identity, block_id, &b, &o])
    )
}

// ---------------------------------------------------------------------------------------------
// Physical rows
// ---------------------------------------------------------------------------------------------

/// Walk every row of a block's `table_delta[]`, calling `f(stream, present, row_bytes)`.
/// Wire's pinned producer declares one delta fewer than it writes (the resource index stream is
/// counted once and emitted twice), so trailing deltas past the declared count are walked too.
fn walk_deltas(buf: &[u8], mut f: impl FnMut(&[u8], bool, &[u8])) -> Option<()> {
    let (declared, k) = read_varuint(buf)?;
    let mut off = k;
    let mut seen = 0usize;
    while seen < declared || off < buf.len() {
        let (_variant, k) = read_varuint(buf.get(off..)?)?;
        off += k;
        let (name_len, k) = read_varuint(buf.get(off..)?)?;
        off += k;
        let name = buf.get(off..off + name_len)?;
        off += name_len;
        let (rows, k) = read_varuint(buf.get(off..)?)?;
        off += k;
        for _ in 0..rows {
            let present = *buf.get(off)?;
            off += 1;
            let (len, k) = read_varuint(buf.get(off..)?)?;
            off += k;
            let data = buf.get(off..off + len)?;
            off += len;
            f(name, present != 0, data);
        }
        seen += 1;
    }
    Some(())
}

/// `contract_row_kv_v0 {code, payer, table_id u16, key bytes, value bytes}` or
/// `contract_index_kv_v0 {code, payer, table_id u16, sec_key bytes, pri_key bytes}`.
struct Kv<'a> {
    code: u64,
    payer: u64,
    table_id: u64,
    first: &'a [u8],
    second: &'a [u8],
}

fn parse_kv(d: &[u8]) -> Option<Kv<'_>> {
    let (variant, mut off) = read_varuint(d)?;
    if variant != 0 {
        return None;
    }
    let u64_at = |o: usize| d.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    let code = u64_at(off)?;
    let payer = u64_at(off + 8)?;
    off += 16;
    let table_id = u16::from_le_bytes(d.get(off..off + 2)?.try_into().ok()?) as u64;
    off += 2;
    let (l1, k) = read_varuint(d.get(off..)?)?;
    off += k;
    let first = d.get(off..off + l1)?;
    off += l1;
    let (l2, k) = read_varuint(d.get(off..)?)?;
    off += k;
    let second = d.get(off..off + l2)?;
    off += l2;
    (off == d.len()).then_some(Kv { code, payer, table_id, first, second })
}

// ---------------------------------------------------------------------------------------------
// JavaScript number semantics used by Hyperion's table handlers
// ---------------------------------------------------------------------------------------------

/// `JSON.stringify` of a JS number: NaN/Infinity -> null, integral values without `.0`.
fn js_num(f: f64) -> Value {
    if !f.is_finite() {
        Value::Null
    } else if f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        Number::from_f64(f).map_or(Value::Null, Value::Number)
    }
}

/// `String(v)` for the values a handler can see.
fn js_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `parseFloat(v)`: the longest decimal prefix after leading whitespace, else NaN.
fn js_parse_float(v: Option<&Value>) -> f64 {
    let Some(s) = js_string(v) else { return f64::NAN };
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return f64::NAN;
    }
    let mut end = i;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp_start = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_start {
            end = j;
        }
    }
    t[..end].parse::<f64>().unwrap_or(f64::NAN)
}

/// `parseInt(v)` (base 10): leading integer digits after whitespace and sign, else NaN.
fn js_parse_int(v: Option<&Value>) -> f64 {
    let Some(s) = js_string(v) else { return f64::NAN };
    let t = s.trim_start();
    let (sign, rest) = match t.as_bytes().first() {
        Some(b'-') => (-1.0, &t[1..]),
        Some(b'+') => (1.0, &t[1..]),
        _ => (1.0, t),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    sign * digits.parse::<f64>().unwrap_or(f64::NAN)
}

/// JS truthiness of an object field.
fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        _ => true,
    }
}

/// `"<amount> <symbol>".split(" ")[0]` parsed as a float; non-strings throw in Hyperion.
fn asset_amount(v: Option<&Value>) -> Result<f64, String> {
    match v {
        Some(Value::String(s)) => Ok(js_parse_float(Some(&Value::String(s.split(' ').next().unwrap_or("").to_owned())))),
        _ => Err("weight is not a string".into()),
    }
}

/// `parseFloat((net + cpu).toFixed(4))`.
fn fixed4(x: f64) -> Value {
    if !x.is_finite() {
        return Value::Null;
    }
    js_num(format!("{x:.4}").parse::<f64>().unwrap_or(f64::NAN))
}

// ---------------------------------------------------------------------------------------------
// Hyperion's system table handlers (populateTableHandlers), for present primary rows
// ---------------------------------------------------------------------------------------------

fn data_obj(doc: &Map<String, Value>) -> Result<&Map<String, Value>, String> {
    doc.get("data").and_then(Value::as_object).ok_or_else(|| "data is not an object".to_owned())
}

fn apply_handlers(
    doc: &mut Map<String, Value>,
    system: &str,
    code: &str,
    table: &str,
    abi: &Value,
    profile: &CodecProfile,
) -> Result<(), String> {
    if code == system {
        match table {
            "voters" => {
                let data = data_obj(doc)?.clone();
                let mut v = Map::new();
                if let Some(x) = data.get("is_proxy") {
                    v.insert("is_proxy".into(), x.clone());
                }
                if let Some(p) = data.get("proxy").filter(|p| p.as_str() != Some("")) {
                    v.insert("proxy".into(), p.clone());
                }
                let producers = data.get("producers").and_then(Value::as_array).ok_or("voters.producers is not an array")?;
                if !producers.is_empty() {
                    v.insert("producers".into(), Value::Array(producers.clone()));
                }
                for k in ["last_vote_weight", "proxied_vote_weight", "staked"] {
                    v.insert(k.into(), js_num(js_parse_float(data.get(k))));
                }
                let d = doc.get_mut("data").and_then(Value::as_object_mut).unwrap();
                for k in ["is_proxy", "owner", "proxy", "producers", "last_vote_weight", "proxied_vote_weight", "staked"] {
                    d.remove(k);
                }
                doc.insert("@voters".into(), Value::Object(v));
            }
            "global" => {
                let data = doc.remove("data").unwrap_or(Value::Null);
                doc.insert("@global".into(), data);
            }
            "producers" => {
                let data = data_obj(doc)?.clone();
                let mut p = Map::new();
                p.insert("total_votes".into(), js_num(js_parse_float(data.get("total_votes"))));
                match data.get("is_active") {
                    Some(Value::Number(n)) if n.as_f64() == Some(0.0) => p.insert("is_active".into(), Value::Bool(false)),
                    Some(Value::Number(n)) if n.as_f64() == Some(1.0) => p.insert("is_active".into(), Value::Bool(true)),
                    Some(other) => p.insert("is_active".into(), other.clone()),
                    None => None,
                };
                if let Some(u) = data.get("unpaid_blocks") {
                    p.insert("unpaid_blocks".into(), u.clone());
                }
                doc.insert("@producers".into(), Value::Object(p));
                doc.remove("data");
            }
            "userres" | "delband" => {
                let data = data_obj(doc)?.clone();
                if truthy(data.get("net_weight")) && truthy(data.get("cpu_weight")) {
                    let net = asset_amount(data.get("net_weight"))?;
                    let cpu = asset_amount(data.get("cpu_weight"))?;
                    let mut r = Map::new();
                    let fields: &[&str] = if table == "userres" { &["owner"] } else { &["from", "to"] };
                    for k in fields {
                        if let Some(x) = data.get(*k) {
                            r.insert((*k).into(), x.clone());
                        }
                    }
                    r.insert("net_weight".into(), js_num(net));
                    r.insert("cpu_weight".into(), js_num(cpu));
                    r.insert("total_weight".into(), fixed4(net + cpu));
                    if table == "userres" {
                        r.insert("ram_bytes".into(), js_num(js_parse_int(data.get("ram_bytes"))));
                    }
                    doc.insert(format!("@{table}"), Value::Object(r));
                    doc.remove("data");
                }
            }
            _ => {}
        }
    }
    if code == format!("{system}.msig") {
        match table {
            "proposal" => {
                let data = data_obj(doc)?.clone();
                let packed = data
                    .get("packed_transaction")
                    .and_then(Value::as_str)
                    .ok_or("proposal.packed_transaction is not a string")?;
                let bytes = hex::decode(packed).map_err(|e| format!("proposal.packed_transaction: {e}"))?;
                let mut trx = decode_value(abi, "transaction", &bytes, profile).map_err(|e| e.message().to_owned())?;
                // Hyperion re-decodes each proposed action through its abieos context, which holds
                // no Wire contract ABIs, so every action's `data` is null in Wire documents.
                for action in trx.get_mut("actions").and_then(Value::as_array_mut).into_iter().flatten() {
                    if let Some(a) = action.as_object_mut() {
                        a.insert("data".into(), Value::Null);
                    }
                }
                let mut p = Map::new();
                if let Some(name) = data.get("proposal_name") {
                    p.insert("proposal_name".into(), name.clone());
                }
                p.insert("transaction".into(), trx);
                doc.insert("@proposal".into(), Value::Object(p));
                doc.remove("data");
            }
            "approvals" => {
                let data = data_obj(doc)?.clone();
                let mut a = Map::new();
                for k in ["proposal_name", "requested_approvals", "provided_approvals"] {
                    if let Some(x) = data.get(k) {
                        a.insert(k.into(), x.clone());
                    }
                }
                doc.insert("@approvals".into(), Value::Object(a));
                doc.remove("data");
            }
            "approvals2" => {
                let data = data_obj(doc)?.clone();
                let level = |key: &str| -> Result<Value, String> {
                    let list = data.get(key).and_then(Value::as_array).ok_or(format!("approvals2.{key} is not an array"))?;
                    Ok(Value::Array(
                        list.iter()
                            .map(|item| {
                                let mut m = Map::new();
                                for (out, val) in [
                                    ("actor", item.get("level").and_then(|l| l.get("actor"))),
                                    ("permission", item.get("level").and_then(|l| l.get("permission"))),
                                    ("time", item.get("time")),
                                ] {
                                    if let Some(v) = val {
                                        m.insert(out.into(), v.clone());
                                    }
                                }
                                Value::Object(m)
                            })
                            .collect(),
                    ))
                };
                let mut a = Map::new();
                if let Some(x) = data.get("proposal_name") {
                    a.insert("proposal_name".into(), x.clone());
                }
                a.insert("requested_approvals".into(), level("requested_approvals")?);
                a.insert("provided_approvals".into(), level("provided_approvals")?);
                doc.insert("@approvals".into(), Value::Object(a)); // approvals2 keeps `data`
            }
            _ => {}
        }
    }
    if table == "accounts" {
        let balance = doc.get("data").and_then(|d| d.get("balance")).and_then(Value::as_str).map(str::to_owned);
        if let Some(balance) = balance {
            let mut parts = balance.split(' ');
            let amount = parts.next().unwrap_or("").to_owned();
            let mut a = Map::new();
            a.insert("amount".into(), js_num(js_parse_float(Some(&Value::String(amount)))));
            if let Some(symbol) = parts.next() {
                a.insert("symbol".into(), Value::String(symbol.to_owned()));
            }
            doc.insert("@accounts".into(), Value::Object(a));
            if let Some(d) = doc.get_mut("data").and_then(Value::as_object_mut) {
                d.remove("balance");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Stats {
    blocks: AtomicU64,
    rows: AtomicU64,       // every delta row (each advances the ordinal)
    kv_present: AtomicU64, // present contract_row_kv / contract_index_kv rows
    docs: AtomicU64,
    index_docs: AtomicU64,
    no_abi: AtomicU64,
    no_table: AtomicU64,
    decode_fail: AtomicU64,
    handler_fail: AtomicU64,
    bad_row: AtomicU64,
    unreadable: AtomicU64,
    no_header: AtomicU64,
    stale: AtomicU64, // superseded (forked) log entries skipped
}

/// (reason, code, table_id) -> (count, first sample).
type Failures = Mutex<BTreeMap<(&'static str, String, u64), (u64, String)>>;

fn fail(failures: &Failures, reason: &'static str, code: u64, table_id: u64, sample: &str) {
    let mut m = failures.lock().unwrap();
    let e = m.entry((reason, name_to_string(code), table_id)).or_insert((0, String::new()));
    e.0 += 1;
    if e.1.is_empty() {
        e.1 = sample.chars().take(160).collect();
    }
}

struct Doc {
    block_num: u32,
    id: String,
    body: String,
}

#[allow(clippy::too_many_arguments)]
fn worker(
    cfg: &Config,
    catalog: &Catalog,
    cs: u32,
    ce: u32,
    stats: &Stats,
    failures: &Failures,
    unreadable: &Mutex<Vec<u32>>,
    sink: Option<&mpsc::SyncSender<Doc>>,
) -> Result<()> {
    let mut blocklog = BlockLog::open(&cfg.blocks_dir)?;
    let log_len = std::fs::metadata(&cfg.log_path)?.len();
    let (offsets, stop) = canonical_offsets(&cfg.idx_path, cfg.first_block, cs, ce, log_len)?;
    let mut pos = offsets[0];
    let mut log = BufReader::with_capacity(8 << 20, File::open(&cfg.log_path)?);
    log.seek(SeekFrom::Start(pos))?;

    let mut hdr = [0u8; 48];
    loop {
        let entry_pos = pos;
        if entry_pos >= stop || log.read_exact(&mut hdr).is_err() {
            break;
        }
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        let block_id = hex::encode(&hdr[8..40]);
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        if payload_size > log_len {
            bail!("payload_size {payload_size} at block {block_num} exceeds log length {log_len}: format/offset error");
        }
        let mut payload = vec![0u8; payload_size as usize];
        log.read_exact(&mut payload)?;
        let entry_end = pos + 48 + payload_size;
        let mut suf = [0u8; 8];
        pos = if log.read_exact(&mut suf).is_ok() && u64::from_le_bytes(suf) == pos {
            entry_end + 8
        } else {
            log.seek_relative(-(suf.len() as i64)).ok();
            entry_end
        };
        // A fork leaves the superseded entry in the log; only the entry the index points to counts.
        if block_num < cs || block_num > ce || offsets[(block_num - cs) as usize] != entry_pos {
            stats.stale.fetch_add(1, Relaxed);
            continue;
        }
        stats.blocks.fetch_add(1, Relaxed);

        let mark_unreadable = || {
            stats.unreadable.fetch_add(1, Relaxed);
            let mut l = unreadable.lock().unwrap();
            if l.len() < 20 {
                l.push(block_num);
            }
        };
        let deltas = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => continue,
            Err(_) => {
                mark_unreadable();
                continue;
            }
        };
        let Some((_, slot)) = blocklog.header(block_num) else {
            stats.no_header.fetch_add(1, Relaxed);
            continue;
        };
        let timestamp = cfg.timestamps.slot_to_iso(slot);

        let mut ordinal: u32 = 0;
        let walked = walk_deltas(&deltas, |stream, present, raw| {
            let this = ordinal;
            ordinal += 1;
            stats.rows.fetch_add(1, Relaxed);
            let is_index = match stream {
                b"contract_row_kv" => false,
                b"contract_index_kv" => true,
                _ => return,
            };
            if !present {
                return; // deletions are not indexed (features.index_deltas_deletions = false)
            }
            stats.kv_present.fetch_add(1, Relaxed);
            let Some(kv) = parse_kv(raw) else {
                stats.bad_row.fetch_add(1, Relaxed);
                return;
            };
            let Some(dep) = select(catalog, kv.code, block_num) else {
                stats.no_abi.fetch_add(1, Relaxed);
                fail(failures, "no_abi", kv.code, kv.table_id, "");
                return;
            };
            let maps = if is_index { dep.indexes.get(&kv.table_id) } else { dep.rows.get(&kv.table_id) };
            let Some([tm]) = maps.map(Vec::as_slice) else {
                stats.no_table.fetch_add(1, Relaxed);
                fail(failures, "no_table", kv.code, kv.table_id, if maps.is_some() { "ambiguous" } else { "missing" });
                return;
            };
            let abi = dep.abi.as_ref().unwrap();
            let value = if is_index {
                Value::Object(Map::new())
            } else {
                match decode_value(abi, &tm.value_type, kv.second, &cfg.profile) {
                    Ok(v) => v,
                    Err(e) => {
                        stats.decode_fail.fetch_add(1, Relaxed);
                        fail(failures, "decode", kv.code, kv.table_id, e.message());
                        return;
                    }
                }
            };

            let chain_id = cfg.chain_id.as_str();
            let (first_hex, second_hex) = (hex::encode(kv.first), hex::encode(kv.second));
            let identity = if is_index {
                format!("wire-kv-v1:index:{chain_id}:{}:{}:{first_hex}:{second_hex}", kv.code, kv.table_id)
            } else {
                format!("wire-kv-v1:row:{chain_id}:{}:{}:{first_hex}", kv.code, kv.table_id)
            };
            let (code_s, payer_s) = (name_to_string(kv.code), name_to_string(kv.payer));
            let raw_hex = hex::encode(raw);

            let mut record = Map::new();
            record.insert("kind".into(), Value::from(if is_index { "contract_index_kv" } else { "contract_row_kv" }));
            record.insert("present".into(), Value::Bool(true));
            record.insert("code".into(), Value::from(kv.code.to_string()));
            record.insert("payer".into(), Value::from(kv.payer.to_string()));
            record.insert("tableId".into(), Value::from(kv.table_id));
            if is_index {
                record.insert("secondaryKeyHex".into(), Value::from(first_hex));
                record.insert("primaryKeyHex".into(), Value::from(second_hex));
            } else {
                record.insert("keyHex".into(), Value::from(first_hex));
                record.insert("valueHex".into(), Value::from(second_hex));
            }
            let mut source = Map::new();
            source.insert("chainId".into(), Value::from(chain_id));
            source.insert("blockId".into(), Value::from(block_id.as_str()));
            source.insert("blockNum".into(), Value::from(block_num));
            source.insert("ordinal".into(), Value::from(this));
            source.insert("abiDeployment".into(), Value::from(dep.id.as_str()));
            let mut metadata = Map::new();
            metadata.insert("profileId".into(), Value::from(cfg.doc_profile_id.as_str()));
            metadata.insert("recordKind".into(), Value::from(if is_index { "index" } else { "row" }));
            metadata.insert("present".into(), Value::Bool(true));
            metadata.insert("code".into(), Value::from(code_s.as_str()));
            metadata.insert("payer".into(), Value::from(payer_s.as_str()));
            metadata.insert("rawHex".into(), Value::from(raw_hex.as_str()));
            metadata.insert("primaryTableId".into(), Value::from(tm.primary_id));
            metadata.insert("valueType".into(), Value::from(tm.value_type.as_str()));
            if let Some((name, key_type)) = &tm.secondary {
                metadata.insert("secondaryName".into(), Value::from(name.as_str()));
                metadata.insert("secondaryKeyType".into(), Value::from(key_type.as_str()));
            }
            let mut protocol = Map::new();
            protocol.insert("id".into(), Value::from(PROTOCOL_ID));
            protocol.insert("profileId".into(), Value::from(cfg.doc_profile_id.as_str()));
            protocol.insert("logicalIdentity".into(), Value::from(identity.as_str()));
            protocol.insert("record".into(), Value::Object(record));
            protocol.insert("source".into(), Value::Object(source));
            protocol.insert("rawHex".into(), Value::from(raw_hex));
            protocol.insert("metadata".into(), Value::Object(metadata));

            let mut doc = Map::new();
            doc.insert("@timestamp".into(), Value::from(timestamp.as_str()));
            doc.insert("block_num".into(), Value::from(block_num));
            doc.insert("block_id".into(), Value::from(block_id.as_str()));
            doc.insert("code".into(), Value::from(code_s.as_str()));
            doc.insert("scope".into(), Value::from(""));
            doc.insert("table".into(), Value::from(tm.name.as_str()));
            doc.insert("primary_key".into(), Value::from(state_key(chain_id, &identity)));
            doc.insert("payer".into(), Value::from(payer_s));
            doc.insert("present".into(), Value::from(1));
            doc.insert("data".into(), value);
            doc.insert("protocol".into(), Value::Object(protocol));

            if !is_index {
                if let Err(e) = apply_handlers(&mut doc, &cfg.system_contract, &code_s, &tm.name, abi, &cfg.profile) {
                    // Hyperion's handler throws here and the row produces no document.
                    stats.handler_fail.fetch_add(1, Relaxed);
                    fail(failures, "handler", kv.code, kv.table_id, &e);
                    return;
                }
            } else {
                stats.index_docs.fetch_add(1, Relaxed);
            }
            stats.docs.fetch_add(1, Relaxed);
            if let Some(tx) = sink {
                let id = document_id(chain_id, &identity, &block_id, block_num, this);
                let _ = tx.send(Doc { block_num, id, body: Value::Object(doc).to_string() });
            }
        });
        if walked.is_none() {
            mark_unreadable();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Sinks
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct EsStats {
    docs: AtomicU64,
    bytes: AtomicU64,
    requests: AtomicU64,
    errors: AtomicU64,
    first_error: Mutex<Option<String>>,
}

fn es_poster(rx: &Mutex<mpsc::Receiver<Doc>>, cfg: &Config, url: &str, stats: &EsStats) {
    let mut body = String::new();
    loop {
        let mut batch: Vec<Doc> = Vec::with_capacity(cfg.es_batch);
        {
            let guard = rx.lock().unwrap();
            match guard.recv() {
                Ok(d) => batch.push(d),
                Err(_) => return,
            }
            while batch.len() < cfg.es_batch {
                match guard.try_recv() {
                    Ok(d) => batch.push(d),
                    Err(_) => break,
                }
            }
        }
        body.clear();
        for d in &batch {
            let partition = (d.block_num as u64).div_ceil(cfg.partition_size).max(1);
            body.push_str(&format!(
                "{{\"index\":{{\"_index\":\"{}-delta-{}-{partition:06}\",\"_id\":\"{}\"}}}}\n",
                cfg.chain, cfg.index_version, d.id
            ));
            body.push_str(&d.body);
            body.push('\n');
        }
        let result = minreq::post(url).with_header("Content-Type", "application/x-ndjson").with_body(body.as_str()).send();
        let failed = match &result {
            Ok(resp) => {
                let text = resp.as_str().unwrap_or("");
                let bad = text.contains("\"errors\":true") || !(200..300).contains(&resp.status_code);
                if bad {
                    let mut first = stats.first_error.lock().unwrap();
                    if first.is_none() {
                        *first = Some(text.chars().take(600).collect());
                    }
                }
                bad
            }
            Err(e) => {
                let mut first = stats.first_error.lock().unwrap();
                if first.is_none() {
                    *first = Some(e.to_string());
                }
                true
            }
        };
        if failed {
            stats.errors.fetch_add(1, Relaxed);
        }
        stats.docs.fetch_add(batch.len() as u64, Relaxed);
        stats.bytes.fetch_add(body.len() as u64, Relaxed);
        stats.requests.fetch_add(1, Relaxed);
    }
}

pub fn run(cfg: Config) -> Result<()> {
    if cfg.chain_id.len() != 64 || !cfg.chain_id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
        bail!("--chain-id must be 64 lowercase hex characters");
    }
    eprintln!("[delta-proto] loading Wire ABI catalog {} ...", cfg.abi_catalog);
    let catalog = load_catalog(&cfg.abi_catalog)?;
    eprintln!(
        "[delta-proto] {} contracts, {} block-final ABI states",
        catalog.len(),
        catalog.values().map(Vec::len).sum::<usize>()
    );
    let block_log_last = BlockLog::open(&cfg.blocks_dir)?.last_block();
    if cfg.end > block_log_last {
        eprintln!("[delta-proto] block log ends at {block_log_last}; clamping the range so every document has its @timestamp");
    }
    let (start, end, threads) = (cfg.start, cfg.end.min(block_log_last), cfg.threads.max(1));
    if start > end {
        bail!("empty range after clamping to the block log");
    }
    let total = (end - start + 1) as u64;
    let chunk = total.div_ceil(threads as u64) as u32;
    eprintln!("[delta-proto] decoding Wire KV deltas [{start}..{end}] ({total} blocks) with {threads} threads");

    let stats = Stats::default();
    let failures = Failures::default();
    let unreadable = Mutex::new(Vec::new());
    let es_stats = EsStats::default();
    let es_url = cfg.es.as_ref().map(|u| format!("{}/_bulk", u.trim_end_matches('/')));
    let emit = cfg.out.is_some() || es_url.is_some();
    let (tx, rx) = mpsc::sync_channel::<Doc>(65_536);
    let rx = Mutex::new(rx);
    let t0 = Instant::now();

    std::thread::scope(|s| -> Result<()> {
        let mut consumers = Vec::new();
        if let Some(url) = es_url.as_deref() {
            eprintln!(
                "[delta-proto] direct-ES sink -> {url} | index={}-delta-{}-<part> batch={} posters={}",
                cfg.chain, cfg.index_version, cfg.es_batch, cfg.es_workers
            );
            for _ in 0..cfg.es_workers.max(1) {
                let (rx, cfg, es_stats) = (&rx, &cfg, &es_stats);
                consumers.push(s.spawn(move || es_poster(rx, cfg, url, es_stats)));
            }
        } else if let Some(path) = cfg.out.as_deref() {
            let mut w = BufWriter::new(File::create(path)?);
            let rx = &rx;
            consumers.push(s.spawn(move || {
                let guard = rx.lock().unwrap();
                for d in guard.iter() {
                    let _ = writeln!(w, "{}", d.body);
                }
                let _ = w.flush();
            }));
        }
        let mut workers = Vec::new();
        for i in 0..threads {
            let cs = start.saturating_add(i.saturating_mul(chunk));
            if cs > end {
                break;
            }
            let ce = ((cs as u64 + chunk as u64 - 1).min(end as u64)) as u32;
            let txc = emit.then(|| tx.clone());
            let (cfg, catalog, stats, failures, unreadable) = (&cfg, &catalog, &stats, &failures, &unreadable);
            workers.push(s.spawn(move || {
                if let Err(e) = worker(cfg, catalog, cs, ce, stats, failures, unreadable, txc.as_ref()) {
                    eprintln!("[delta-proto] worker {i} [{cs}..{ce}] FAILED: {e:#}");
                }
            }));
        }
        drop(tx);
        for w in workers {
            let _ = w.join();
        }
        for c in consumers {
            let _ = c.join();
        }
        Ok(())
    })?;

    let secs = t0.elapsed().as_secs_f64();
    let b = stats.blocks.load(Relaxed);
    eprintln!(
        "[delta-proto] done: {b} blocks in {secs:.1}s ({:.0} blk/s) | rows={} kv_present={} -> docs={} (index records {}) | no_abi={} no_table={} decode_fail={} handler_fail={} bad_row={} no_header={}",
        b as f64 / secs.max(1e-9),
        stats.rows.load(Relaxed),
        stats.kv_present.load(Relaxed),
        stats.docs.load(Relaxed),
        stats.index_docs.load(Relaxed),
        stats.no_abi.load(Relaxed),
        stats.no_table.load(Relaxed),
        stats.decode_fail.load(Relaxed),
        stats.handler_fail.load(Relaxed),
        stats.bad_row.load(Relaxed),
        stats.no_header.load(Relaxed),
    );
    if es_url.is_some() {
        let docs = es_stats.docs.load(Relaxed);
        let mb = es_stats.bytes.load(Relaxed) as f64 / 1e6;
        eprintln!(
            "[delta-proto] ES sink: posted {docs} docs in {secs:.1}s ({:.0} docs/s) | {mb:.1} MB | {} bulk reqs | errors={}",
            docs as f64 / secs.max(1e-9),
            es_stats.requests.load(Relaxed),
            es_stats.errors.load(Relaxed)
        );
        if let Some(e) = es_stats.first_error.lock().unwrap().as_ref() {
            eprintln!("[delta-proto] WARNING: first bulk error: {e}");
        }
    }
    let stale = stats.stale.load(Relaxed);
    if stale > 0 {
        eprintln!("[delta-proto] skipped {stale} superseded log entries (forked blocks the index no longer points to)");
    }
    let n_unreadable = stats.unreadable.load(Relaxed);
    if n_unreadable > 0 {
        eprintln!(
            "[delta-proto] WARNING: {n_unreadable} state-history entries could not be read; first blocks: {:?}",
            unreadable.lock().unwrap()
        );
    }
    let f = failures.lock().unwrap();
    if !f.is_empty() {
        eprintln!("[delta-proto] rows without a document, by reason/contract/table_id:");
        let mut top: Vec<_> = f.iter().collect();
        top.sort_by_key(|e| std::cmp::Reverse(e.1.0));
        for ((reason, code, tid), (cnt, sample)) in top.into_iter().take(30) {
            eprintln!("  {cnt:>8}  {reason:<16} {code}/{tid}  {sample}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for n in ["sysio", "sysio.token", "eosrio", "a", "zzzzzzzzzzzzj"] {
            assert_eq!(name_to_string(string_to_name(n).unwrap()), n);
        }
        assert_eq!(string_to_name("sysio"), Some(14389258095169634304));
    }

    #[test]
    fn identities_match_hyperion() {
        // Stored 4.5 blockinfo row at block 100000 (ordinal 3).
        let chain = "9ccd94e823fda51363bd4265ad618a909c4985c445078c79f8d030fe9697b71d";
        let identity = format!("wire-kv-v1:row:{chain}:14389258095169634304:21782:000000000001869f");
        assert_eq!(
            state_key(chain, &identity),
            "h45k1:12ecd69ff2417eefb733cf24f244f77b354fb0064c721b4d088478afa6bc0fd7"
        );
        let block_id = "000186a0d140e426ff7499bb4b43dc7db6eb37bfe90dceb48250db8151563351";
        assert_eq!(
            document_id(chain, &identity, block_id, 100000, 3),
            "h45h1:d70712c84c35f40ecc0748ae5de596acef91ab2b70d4012b69357e69533fdc3f"
        );
    }

    #[test]
    fn parse_float_follows_javascript() {
        let s = |x: &str| js_parse_float(Some(&Value::from(x)));
        assert_eq!(s("1.5000 WIRE"), 1.5);
        assert_eq!(s("  -2e3x"), -2000.0);
        assert_eq!(s("7e"), 7.0);
        assert!(s("WIRE").is_nan());
        assert!(js_parse_float(None).is_nan());
        assert_eq!(js_num(f64::NAN), Value::Null);
        assert_eq!(js_num(3.0), Value::from(3));
    }

    #[test]
    fn canonical_offsets_bound_the_walk() {
        let dir = std::env::temp_dir().join(format!("delta-proto-idx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let idx = dir.join("chain_state_history.index");
        let offsets: [u64; 3] = [0, 100, 250];
        std::fs::write(&idx, offsets.iter().flat_map(|o| o.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
        let p = idx.to_str().unwrap();
        // Blocks 10..=11 end where block 12's entry starts; the head range ends at the log's end.
        assert_eq!(canonical_offsets(p, 10, 10, 11, 999).unwrap(), (vec![0, 100], 250));
        assert_eq!(canonical_offsets(p, 10, 12, 12, 999).unwrap(), (vec![250], 999));
        assert!(canonical_offsets(p, 10, 12, 13, 999).is_err());
        assert!(canonical_offsets(p, 10, 9, 10, 999).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn kv_rows_parse_exactly() {
        let raw = hex::decode("000000000000eab0c70000000000eab0c7165508000000000001869f0d009f86010040ced3c1425e0600").unwrap();
        let kv = parse_kv(&raw).unwrap();
        assert_eq!((kv.code, kv.table_id), (14389258095169634304, 21782));
        assert_eq!(hex::encode(kv.first), "000000000001869f");
        assert!(parse_kv(&[raw.as_slice(), &[0]].concat()).is_none());
    }
}
