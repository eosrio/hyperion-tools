//! accinfo fragment builder.
//!
//! Renders, per account, the cc32d9 accinfo body *minus* the `{account_name, chain}`
//! prefix — i.e. the fragment `"resources":…,"permissions":…,…,"linkauth":[…][,"code":…]}`
//! that WormDB's lightapi_accinfo/account procedures wrap with the (shared) chain block.
//! Byte-matches light-api's `accinfo_value` rendering (handlers.rs) so parity holds.
//!
//! Strategy: `permissions` is the account universe (every account has an owner perm), so we
//! stream it sorted by account and group; `userres` (resources), `delband` (delegations) and
//! `account_codehash` are loaded into RAM maps first (all small enough: ~2 GB total). One cursor,
//! three lookups — no k-way cursor merge.

use anyhow::{Context, Result};
use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::Database;
use std::collections::HashMap;
use std::fmt::Write as _;

use crate::name;
use crate::wseg::{IndexEntry, Table};

/// WormDB segment TableId.accinfo (must match src/storage/segment.zig).
pub const TABLE_ACCINFO: u32 = 5;

/// "113.00000000 WAX" -> 11300000000 (base units, exact). Matches light-api asset::units.
fn asset_units(s: &str) -> i64 {
    let num = s.split(' ').next().unwrap_or("");
    let mut buf = String::with_capacity(num.len());
    let mut neg = false;
    for (i, c) in num.chars().enumerate() {
        match c {
            '-' if i == 0 => neg = true,
            '.' => {}
            d if d.is_ascii_digit() => buf.push(d),
            _ => return 0,
        }
    }
    let v: i64 = buf.parse().unwrap_or(0);
    if neg {
        -v
    } else {
        v
    }
}

/// i64 from Int64/Int32/Double or a numeric string (ram_bytes is stored as a string).
fn bson_i64_any(b: Option<&Bson>) -> Option<i64> {
    match b? {
        Bson::Int64(v) => Some(*v),
        Bson::Int32(v) => Some(*v as i64),
        Bson::Double(d) => Some(*d as i64),
        Bson::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

type Resources = (i64, i64, i64); // (net_weight, cpu_weight, ram_bytes)
type Deleg = (String, i64, i64); // (peer, cpu_weight, net_weight)

/// Render one permission doc into the cc32d9 `permissions[]` entry shape.
fn render_permission(out: &mut String, perm: &Document) {
    let perm_name = perm.get_str("perm_name").unwrap_or("");
    let ra = perm.get_document("required_auth").ok();
    let threshold = ra
        .and_then(|r| bson_i64_any(r.get("threshold")))
        .unwrap_or(1);

    let _ = write!(
        out,
        "{{\"perm\":\"{perm_name}\",\"threshold\":{threshold},\"auth\":{{\"keys\":["
    );
    if let Some(r) = ra {
        if let Ok(keys) = r.get_array("keys") {
            let mut first = true;
            for k in keys {
                let Some(kd) = k.as_document() else { continue };
                let legacy = kd
                    .get_str("pubkey")
                    .or_else(|_| kd.get_str("key"))
                    .unwrap_or("");
                let modern = kd
                    .get_str("public_key")
                    .or_else(|_| kd.get_str("key_pub"))
                    .or_else(|_| kd.get_str("key"))
                    .unwrap_or("");
                let weight = bson_i64_any(kd.get("weight")).unwrap_or(1);
                if !first {
                    out.push(',');
                }
                first = false;
                let _ = write!(
                    out,
                    "{{\"pubkey\":\"{legacy}\",\"public_key\":\"{modern}\",\"weight\":{weight}}}"
                );
            }
        }
    }
    out.push_str("],\"accounts\":[");
    if let Some(r) = ra {
        if let Ok(accounts) = r.get_array("accounts") {
            let mut first = true;
            for a in accounts {
                let Some(ad) = a.as_document() else { continue };
                let p = ad.get_document("permission").ok();
                let actor = p.and_then(|p| p.get_str("actor").ok()).unwrap_or("");
                let permission = p.and_then(|p| p.get_str("permission").ok()).unwrap_or("");
                let weight = bson_i64_any(ad.get("weight")).unwrap_or(1);
                if !first {
                    out.push(',');
                }
                first = false;
                let _ = write!(
                    out,
                    "{{\"actor\":\"{actor}\",\"permission\":\"{permission}\",\"weight\":{weight}}}"
                );
            }
        }
    }
    out.push_str("]}}");
}

/// Render the full accinfo fragment for one account (resources … linkauth [, code] }).
/// `perms` must be sorted by perm_name (matches light-api's `find().sort({perm_name:1})`).
fn render_fragment(
    out: &mut String,
    userres: Option<&Resources>,
    perms: &[Document],
    deleg_to: Option<&Vec<Deleg>>,
    deleg_from: Option<&Vec<Deleg>>,
    codehash: Option<&String>,
) {
    out.clear();
    // resources (null when no row, like cc32d9)
    match userres {
        Some((net, cpu, ram)) => {
            let _ = write!(
                out,
                "\"resources\":{{\"net_weight\":{net},\"cpu_weight\":{cpu},\"ram_bytes\":{ram}}}"
            );
        }
        None => out.push_str("\"resources\":null"),
    }
    // permissions
    out.push_str(",\"permissions\":[");
    for (i, p) in perms.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        render_permission(out, p);
    }
    // delegated_to / delegated_from
    out.push_str("],\"delegated_to\":[");
    if let Some(v) = deleg_to {
        for (i, (to, cpu, net)) in v.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"del_to\":\"{to}\",\"cpu_weight\":{cpu},\"net_weight\":{net}}}"
            );
        }
    }
    out.push_str("],\"delegated_from\":[");
    if let Some(v) = deleg_from {
        for (i, (from, cpu, net)) in v.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            let _ = write!(
                out,
                "{{\"del_from\":\"{from}\",\"cpu_weight\":{cpu},\"net_weight\":{net}}}"
            );
        }
    }
    // linkauth (flattened across perms, in perm order)
    out.push_str("],\"linkauth\":[");
    let mut first_la = true;
    for p in perms {
        let req = p.get_str("perm_name").unwrap_or("");
        if let Ok(la) = p.get_array("linked_actions") {
            for a in la {
                let Some(ad) = a.as_document() else { continue };
                let code = ad.get_str("account").unwrap_or("");
                let typ = ad.get_str("action").unwrap_or("");
                if !first_la {
                    out.push(',');
                }
                first_la = false;
                let _ = write!(
                    out,
                    "{{\"code\":\"{code}\",\"type\":\"{typ}\",\"requirement\":\"{req}\"}}"
                );
            }
        }
    }
    out.push(']');
    // code (only when the account has a contract)
    if let Some(h) = codehash {
        let _ = write!(out, ",\"code\":{{\"code_hash\":\"{h}\"}}");
    }
    out.push('}');
}

/// Load userres -> {account_u64: (net, cpu, ram)}.
async fn load_resources(db: &Database) -> Result<HashMap<u64, Resources>> {
    let coll = db.collection::<Document>("eosio-userres");
    let mut cur = coll
        .find(doc! {})
        .projection(
            doc! { "_id": 0, "@scope": 1, "net_weight": 1, "cpu_weight": 1, "ram_bytes": 1 },
        )
        .batch_size(20_000)
        .await
        .context("find userres")?;
    let mut map: HashMap<u64, Resources> = HashMap::with_capacity(22_000_000);
    while let Some(d) = cur.try_next().await? {
        let Ok(scope) = d.get_str("@scope") else {
            continue;
        };
        let net = asset_units(d.get_str("net_weight").unwrap_or(""));
        let cpu = asset_units(d.get_str("cpu_weight").unwrap_or(""));
        let ram = bson_i64_any(d.get("ram_bytes")).unwrap_or(0);
        map.insert(name::encode(scope), (net, cpu, ram));
    }
    Ok(map)
}

/// Load delband -> (by_from {from: [(to,cpu,net)]}, by_to {to: [(from,cpu,net)]}).
async fn load_delband(
    db: &Database,
) -> Result<(HashMap<u64, Vec<Deleg>>, HashMap<u64, Vec<Deleg>>)> {
    let coll = db.collection::<Document>("eosio-delband");
    let mut cur = coll
        .find(doc! {})
        .projection(doc! { "_id": 0, "from": 1, "to": 1, "cpu_weight": 1, "net_weight": 1 })
        .batch_size(20_000)
        .await
        .context("find delband")?;
    let mut by_from: HashMap<u64, Vec<Deleg>> = HashMap::new();
    let mut by_to: HashMap<u64, Vec<Deleg>> = HashMap::new();
    while let Some(d) = cur.try_next().await? {
        let from = d.get_str("from").unwrap_or("");
        let to = d.get_str("to").unwrap_or("");
        let cpu = asset_units(d.get_str("cpu_weight").unwrap_or(""));
        let net = asset_units(d.get_str("net_weight").unwrap_or(""));
        if !from.is_empty() {
            by_from
                .entry(name::encode(from))
                .or_default()
                .push((to.to_string(), cpu, net));
        }
        if !to.is_empty() {
            by_to
                .entry(name::encode(to))
                .or_default()
                .push((from.to_string(), cpu, net));
        }
    }
    Ok((by_from, by_to))
}

/// Load account_codehash -> {account_u64: code_hash}.
async fn load_codehash(db: &Database) -> Result<HashMap<u64, String>> {
    let coll = db.collection::<Document>("account_codehash");
    let mut cur = coll
        .find(doc! {})
        .projection(doc! { "_id": 0, "account": 1, "code_hash": 1 })
        .await
        .context("find codehash")?;
    let mut map = HashMap::new();
    while let Some(d) = cur.try_next().await? {
        if let (Ok(a), Ok(h)) = (d.get_str("account"), d.get_str("code_hash")) {
            map.insert(name::encode(a), h.to_string());
        }
    }
    Ok(map)
}

/// Build the accinfo table: stream permissions grouped by account, render each fragment.
pub async fn build_accinfo(db: &Database) -> Result<Table> {
    let t0 = std::time::Instant::now();
    eprintln!("[accinfo] loading resources/delband/codehash into RAM maps…");
    let userres = load_resources(db).await?;
    let (by_from, by_to) = load_delband(db).await?;
    let codehash = load_codehash(db).await?;
    eprintln!(
        "[accinfo] maps loaded: {} userres, {} deleg-from / {} deleg-to, {} codehash ({:.0}s)",
        userres.len(),
        by_from.len(),
        by_to.len(),
        codehash.len(),
        t0.elapsed().as_secs_f64()
    );

    let coll = db.collection::<Document>("permissions");
    let mut cur = coll
        .find(doc! {})
        .sort(doc! { "account": 1 }) // index-ordered stream, groups perms by account
        .projection(doc! { "_id": 0, "account": 1, "perm_name": 1, "required_auth": 1, "linked_actions": 1 })
        .batch_size(20_000)
        .await
        .context("find permissions")?;

    let mut arena: Vec<u8> = Vec::with_capacity(14usize << 30); // up to ~14 GiB
    let mut index: Vec<IndexEntry> = Vec::with_capacity(22_000_000);
    let mut frag = String::with_capacity(4096);

    let mut cur_acct: Option<String> = None;
    let mut group: Vec<Document> = Vec::new();
    let mut perms_seen: u64 = 0;

    // Flush the accumulated perm group for one account into the arena.
    macro_rules! flush_group {
        ($acct:expr, $group:expr) => {{
            let key = name::encode($acct);
            $group.sort_by(|a: &Document, b: &Document| {
                a.get_str("perm_name")
                    .unwrap_or("")
                    .cmp(b.get_str("perm_name").unwrap_or(""))
            });
            render_fragment(
                &mut frag,
                userres.get(&key),
                &$group,
                by_from.get(&key),
                by_to.get(&key),
                codehash.get(&key),
            );
            let off = arena.len();
            anyhow::ensure!(frag.len() <= u32::MAX as usize, "fragment too large");
            arena.extend_from_slice(frag.as_bytes());
            index.push(IndexEntry {
                key,
                off: off as u64,
                len: frag.len() as u32,
            });
        }};
    }

    while let Some(d) = cur.try_next().await? {
        let acct = d.get_str("account").unwrap_or("").to_string();
        if cur_acct.as_deref() != Some(acct.as_str()) {
            if let Some(prev) = cur_acct.take() {
                flush_group!(&prev, group);
                group.clear();
            }
            cur_acct = Some(acct);
        }
        group.push(d);
        perms_seen += 1;
        if perms_seen % 4_000_000 == 0 {
            eprintln!(
                "[accinfo] {} perms, {} accounts, {} MiB arena, {:.0}s",
                perms_seen,
                index.len(),
                arena.len() >> 20,
                t0.elapsed().as_secs_f64()
            );
        }
    }
    if let Some(prev) = cur_acct.take() {
        flush_group!(&prev, group);
    }

    eprintln!(
        "[accinfo] {} accounts, {} perms -> {} MiB arena in {:.0}s",
        index.len(),
        perms_seen,
        arena.len() >> 20,
        t0.elapsed().as_secs_f64()
    );
    Ok(Table {
        table_id: TABLE_ACCINFO,
        index,
        arena,
    })
}

/// Probe one account: render + print its fragment (fast parity check, no full build).
pub async fn probe_accinfo(db: &Database, account: &str) -> Result<()> {
    let key = name::encode(account);

    let perms_coll = db.collection::<Document>("permissions");
    let mut pc = perms_coll.find(doc! { "account": account }).await?;
    let mut perms: Vec<Document> = Vec::new();
    while let Some(d) = pc.try_next().await? {
        perms.push(d);
    }
    perms.sort_by(|a, b| {
        a.get_str("perm_name")
            .unwrap_or("")
            .cmp(b.get_str("perm_name").unwrap_or(""))
    });

    let ur = db
        .collection::<Document>("eosio-userres")
        .find_one(doc! { "@scope": account })
        .await?;
    let resources: Option<Resources> = ur.map(|u| {
        (
            asset_units(u.get_str("net_weight").unwrap_or("")),
            asset_units(u.get_str("cpu_weight").unwrap_or("")),
            bson_i64_any(u.get("ram_bytes")).unwrap_or(0),
        )
    });

    let dbnd = db.collection::<Document>("eosio-delband");
    let mut dt: Vec<Deleg> = Vec::new();
    let mut fc = dbnd.find(doc! { "from": account }).await?;
    while let Some(d) = fc.try_next().await? {
        dt.push((
            d.get_str("to").unwrap_or("").to_string(),
            asset_units(d.get_str("cpu_weight").unwrap_or("")),
            asset_units(d.get_str("net_weight").unwrap_or("")),
        ));
    }
    let mut df: Vec<Deleg> = Vec::new();
    let mut tc = dbnd.find(doc! { "to": account }).await?;
    while let Some(d) = tc.try_next().await? {
        df.push((
            d.get_str("from").unwrap_or("").to_string(),
            asset_units(d.get_str("cpu_weight").unwrap_or("")),
            asset_units(d.get_str("net_weight").unwrap_or("")),
        ));
    }

    let ch = db
        .collection::<Document>("account_codehash")
        .find_one(doc! { "account": account })
        .await?
        .and_then(|d| d.get_str("code_hash").ok().map(|s| s.to_string()));

    let _ = key;
    let mut frag = String::new();
    render_fragment(
        &mut frag,
        resources.as_ref(),
        &perms,
        Some(&dt),
        Some(&df),
        ch.as_ref(),
    );
    println!("{frag}");
    Ok(())
}
