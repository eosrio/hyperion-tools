//! Push-based segment Builder.
//!
//! Accumulates the Light-API tables from mapped Hyperion docs — the same `(collection, Document)`
//! shape the Mongo sink receives — then writes the `.wseg`. Used by BOTH wseg-build (Mongo source)
//! and snapshot-load (snapshot source), so a segment can be built with or without MongoDB.
//!
//! Permissions are kept as compact per-account structs (NOT bson Documents), so the accumulator holds
//! at chain scale: WAX has 43.6M permissions — holding the bson docs would be tens of GB; the compact
//! form is ~4 GB. Rendering byte-matches light-api's `accinfo_value`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use mongodb::bson::{Bson, Document};

use crate::name;
use crate::wseg::{write_segment, IndexEntry, Table};

pub const TABLE_BALANCES: u32 = 0;
pub const TABLE_ACCINFO: u32 = 5;

/// "113.00000000 WAX" -> 11300000000 (base units, exact).
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
fn bson_i64(b: Option<&Bson>) -> Option<i64> {
    match b? {
        Bson::Int64(v) => Some(*v),
        Bson::Int32(v) => Some(*v as i64),
        Bson::Double(d) => Some(*d as i64),
        Bson::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

type Resources = (i64, i64, i64); // net, cpu, ram
type Deleg = (String, i64, i64); // peer, cpu, net

#[derive(Default, Clone)]
struct Perm {
    perm_name: String,
    threshold: i64,
    keys: Vec<(String, String, i64)>, // pubkey(EOS), public_key(PUB_K1), weight
    accounts: Vec<(String, String, i64)>, // actor, permission, weight
    linked: Vec<(String, String)>,    // code(account), type(action)
}

#[derive(Default)]
pub struct Builder {
    balances: HashMap<u64, Vec<u8>>,
    resources: HashMap<u64, Resources>,
    deleg_to: HashMap<u64, Vec<Deleg>>,
    deleg_from: HashMap<u64, Vec<Deleg>>,
    codehash: HashMap<u64, String>,
    perms: HashMap<u64, Vec<Perm>>,
    pub rows: u64,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Route a mapped Hyperion doc into the segment (same collection names the Mongo sink uses).
    /// Unserved collections (voters, eosio-global, pub_keys, proposals) are ignored.
    pub fn push(&mut self, coll: &str, d: &Document) {
        self.rows += 1;
        match coll {
            "accounts" => self.push_balance(d),
            "permissions" => self.push_perm(d),
            "eosio-userres" => self.push_userres(d),
            "eosio-delband" => self.push_delband(d),
            "account_codehash" => self.push_codehash(d),
            _ => {}
        }
    }

    fn push_balance(&mut self, d: &Document) {
        let Ok(scope) = d.get_str("scope") else {
            return;
        };
        let code = d.get_str("code").unwrap_or("");
        let symbol = d.get_str("symbol").unwrap_or("");
        let decimals = bson_i64(d.get("decimals")).unwrap_or(0);
        let amount = match d.get_str("amount_str") {
            Ok(s) => s.to_string(),
            Err(_) => match d.get_f64("amount") {
                Ok(f) => format!("{:.*}", decimals.max(0) as usize, f),
                Err(_) => "0".to_string(),
            },
        };
        let buf = self.balances.entry(name::encode(scope)).or_default();
        buf.extend_from_slice(code.as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(symbol.as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(decimals.to_string().as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(amount.as_bytes());
        buf.push(b'\n');
    }

    fn push_userres(&mut self, d: &Document) {
        let scope = d
            .get_str("@scope")
            .or_else(|_| d.get_str("owner"))
            .unwrap_or("");
        if scope.is_empty() {
            return;
        }
        let net = asset_units(d.get_str("net_weight").unwrap_or(""));
        let cpu = asset_units(d.get_str("cpu_weight").unwrap_or(""));
        let ram = bson_i64(d.get("ram_bytes")).unwrap_or(0);
        self.resources.insert(name::encode(scope), (net, cpu, ram));
    }

    fn push_delband(&mut self, d: &Document) {
        let from = d.get_str("from").unwrap_or("");
        let to = d.get_str("to").unwrap_or("");
        let cpu = asset_units(d.get_str("cpu_weight").unwrap_or(""));
        let net = asset_units(d.get_str("net_weight").unwrap_or(""));
        if !from.is_empty() {
            self.deleg_to
                .entry(name::encode(from))
                .or_default()
                .push((to.to_string(), cpu, net));
        }
        if !to.is_empty() {
            self.deleg_from
                .entry(name::encode(to))
                .or_default()
                .push((from.to_string(), cpu, net));
        }
    }

    fn push_codehash(&mut self, d: &Document) {
        if let (Ok(a), Ok(h)) = (d.get_str("account"), d.get_str("code_hash")) {
            self.codehash.insert(name::encode(a), h.to_string());
        }
    }

    fn push_perm(&mut self, d: &Document) {
        let Ok(account) = d.get_str("account") else {
            return;
        };
        let ra = d.get_document("required_auth").ok();
        let threshold = ra.and_then(|r| bson_i64(r.get("threshold"))).unwrap_or(1);
        let mut p = Perm {
            perm_name: d.get_str("perm_name").unwrap_or("").to_string(),
            threshold,
            ..Default::default()
        };
        if let Some(r) = ra {
            if let Ok(keys) = r.get_array("keys") {
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
                    let w = bson_i64(kd.get("weight")).unwrap_or(1);
                    p.keys.push((legacy.to_string(), modern.to_string(), w));
                }
            }
            if let Ok(accs) = r.get_array("accounts") {
                for a in accs {
                    let Some(ad) = a.as_document() else { continue };
                    let perm = ad.get_document("permission").ok();
                    let actor = perm.and_then(|p| p.get_str("actor").ok()).unwrap_or("");
                    let permission = perm
                        .and_then(|p| p.get_str("permission").ok())
                        .unwrap_or("");
                    let w = bson_i64(ad.get("weight")).unwrap_or(1);
                    p.accounts
                        .push((actor.to_string(), permission.to_string(), w));
                }
            }
        }
        if let Ok(la) = d.get_array("linked_actions") {
            for a in la {
                let Some(ad) = a.as_document() else { continue };
                let code = ad.get_str("account").unwrap_or("");
                let typ = ad.get_str("action").unwrap_or("");
                p.linked.push((code.to_string(), typ.to_string()));
            }
        }
        self.perms.entry(name::encode(account)).or_default().push(p);
    }

    /// Render one account's accinfo fragment from the accumulated state (for --probe parity checks).
    pub fn render_account(&mut self, account: &str) -> String {
        let key = name::encode(account);
        if let Some(ps) = self.perms.get_mut(&key) {
            ps.sort_by(|a, b| a.perm_name.cmp(&b.perm_name));
        }
        let mut out = String::new();
        render_fragment(
            &mut out,
            self.resources.get(&key),
            self.perms.get(&key).map(|v| v.as_slice()).unwrap_or(&[]),
            self.deleg_to.get(&key),
            self.deleg_from.get(&key),
            self.codehash.get(&key),
        );
        out
    }

    /// Write the segment (balances + accinfo tables). Returns (holders, accounts).
    pub fn finish(mut self, out: &str) -> std::io::Result<(usize, usize)> {
        // balances table
        let mut bal_index: Vec<IndexEntry> = Vec::with_capacity(self.balances.len());
        let mut bal_arena: Vec<u8> = Vec::new();
        for (key, buf) in self.balances.drain() {
            if buf.is_empty() {
                continue;
            }
            bal_index.push(IndexEntry {
                key,
                off: bal_arena.len() as u64,
                len: buf.len() as u32,
            });
            bal_arena.extend_from_slice(&buf);
        }
        let holders = bal_index.len();
        let balances_tbl = Table {
            table_id: TABLE_BALANCES,
            index: bal_index,
            arena: bal_arena,
        };

        // sort each account's perms by perm_name (matches light-api), once, before rendering
        for ps in self.perms.values_mut() {
            ps.sort_by(|a, b| a.perm_name.cmp(&b.perm_name));
        }
        // accinfo table: union of every account seen across perms / resources / delband / codehash
        let mut accts: HashSet<u64> = HashSet::new();
        accts.extend(self.perms.keys());
        accts.extend(self.resources.keys());
        accts.extend(self.deleg_to.keys());
        accts.extend(self.deleg_from.keys());
        accts.extend(self.codehash.keys());

        let mut acc_index: Vec<IndexEntry> = Vec::with_capacity(accts.len());
        let mut acc_arena: Vec<u8> = Vec::new();
        let mut frag = String::with_capacity(2048);
        let empty: Vec<Perm> = Vec::new();
        for key in accts {
            render_fragment(
                &mut frag,
                self.resources.get(&key),
                self.perms.get(&key).unwrap_or(&empty),
                self.deleg_to.get(&key),
                self.deleg_from.get(&key),
                self.codehash.get(&key),
            );
            acc_index.push(IndexEntry {
                key,
                off: acc_arena.len() as u64,
                len: frag.len() as u32,
            });
            acc_arena.extend_from_slice(frag.as_bytes());
        }
        let accounts = acc_index.len();
        let accinfo_tbl = Table {
            table_id: TABLE_ACCINFO,
            index: acc_index,
            arena: acc_arena,
        };

        write_segment(out, vec![balances_tbl, accinfo_tbl])?;
        Ok((holders, accounts))
    }
}

fn render_permission(out: &mut String, p: &Perm) {
    let _ = write!(
        out,
        "{{\"perm\":\"{}\",\"threshold\":{},\"auth\":{{\"keys\":[",
        p.perm_name, p.threshold
    );
    for (i, (legacy, modern, w)) in p.keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"pubkey\":\"{legacy}\",\"public_key\":\"{modern}\",\"weight\":{w}}}"
        );
    }
    out.push_str("],\"accounts\":[");
    for (i, (actor, permission, w)) in p.accounts.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"actor\":\"{actor}\",\"permission\":\"{permission}\",\"weight\":{w}}}"
        );
    }
    out.push_str("]}}");
}

/// Render the cc32d9 accinfo fragment (resources … linkauth [, code] }). `perms` must be sorted.
fn render_fragment(
    out: &mut String,
    userres: Option<&Resources>,
    perms: &[Perm],
    deleg_to: Option<&Vec<Deleg>>,
    deleg_from: Option<&Vec<Deleg>>,
    codehash: Option<&String>,
) {
    out.clear();
    match userres {
        Some((net, cpu, ram)) => {
            let _ = write!(
                out,
                "\"resources\":{{\"net_weight\":{net},\"cpu_weight\":{cpu},\"ram_bytes\":{ram}}}"
            );
        }
        None => out.push_str("\"resources\":null"),
    }
    out.push_str(",\"permissions\":[");
    for (i, p) in perms.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        render_permission(out, p);
    }
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
    out.push_str("],\"linkauth\":[");
    let mut first_la = true;
    for p in perms {
        for (code, typ) in &p.linked {
            if !first_la {
                out.push(',');
            }
            first_la = false;
            let _ = write!(
                out,
                "{{\"code\":\"{code}\",\"type\":\"{typ}\",\"requirement\":\"{}\"}}",
                p.perm_name
            );
        }
    }
    out.push(']');
    if let Some(h) = codehash {
        let _ = write!(out, ",\"code\":{{\"code_hash\":\"{h}\"}}");
    }
    out.push('}');
}
