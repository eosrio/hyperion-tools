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
pub const TABLE_TOKEN_HOLDERS: u32 = 6;
pub const TABLE_PUB_KEYS: u32 = 7;
pub const TABLE_TOP_RAM: u32 = 8;
pub const TABLE_TOP_STAKE: u32 = 9;
pub const TABLE_CODEHASH: u32 = 10;

/// FNV1a-64 of a string (used for the token and pub-key table keys). Matches the Zig `fnv1a64`.
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Segment key for a public key string (EOS… or PUB_K1_…).
pub fn key_hash(pubkey: &str) -> u64 {
    fnv1a64(pubkey)
}

fn anyhow_u16(n: usize) -> std::io::Result<()> {
    if n > u16::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "token id too long",
        ));
    }
    Ok(())
}

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

use crate::binfmt::{self, Perm};

type Resources = (i64, i64, i64); // net, cpu, ram
type Deleg = (String, i64, i64); // peer, cpu, net
                                 // (contract, symbol) -> holders; each holder = (account, sort-units i128, amount string).
type TokenHolders = HashMap<(String, String), Vec<(String, i128, String)>>;
// modern (PUB_K1) key string -> (legacy EOS form, holders[(account, perm, weight)]).
type KeyHolders = HashMap<String, (String, Vec<(String, String, i64)>)>;

#[derive(Default)]
pub struct Builder {
    balances: HashMap<u64, Vec<u8>>,
    /// (contract, symbol) -> holders, for the token_holders table (HTTP topholders/holdercount +
    /// WS get_token_holders). Each entry: (holder account string, sort units i128, amount string).
    token_holders: TokenHolders,
    resources: HashMap<u64, Resources>,
    deleg_to: HashMap<u64, Vec<Deleg>>,
    deleg_from: HashMap<u64, Vec<Deleg>>,
    codehash: HashMap<u64, String>,
    perms: HashMap<u64, Vec<Perm>>,
    /// modern (PUB_K1) key string -> (legacy EOS form, holders[(account, perm, weight)]), for the
    /// pub_keys table (HTTP /key + WS get_accounts_from_keys).
    key_holders: KeyHolders,
    pub rows: u64,
}

/// Sortable integer units from an asset amount string ("123.4567" -> 1234567), sign-aware. i128 so
/// big-supply tokens don't overflow.
fn amount_units(s: &str) -> i128 {
    let mut buf = String::with_capacity(s.len());
    let mut neg = false;
    for (i, c) in s.chars().enumerate() {
        match c {
            '-' if i == 0 => neg = true,
            '.' => {}
            d if d.is_ascii_digit() => buf.push(d),
            _ => {}
        }
    }
    let v: i128 = buf.parse().unwrap_or(0);
    if neg {
        -v
    } else {
        v
    }
}

/// Stable key for a (contract, symbol) token: FNV1a-64 of "contract:symbol". Must match the Zig
/// `core/name.zig` tokenKey so the procedures look up the same entry.
pub fn token_key(contract: &str, symbol: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in contract
        .bytes()
        .chain(std::iter::once(b':'))
        .chain(symbol.bytes())
    {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
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
        let holder = name::encode(scope);
        let buf = self.balances.entry(holder).or_default();
        buf.extend_from_slice(code.as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(symbol.as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(decimals.to_string().as_bytes());
        buf.push(b'\t');
        buf.extend_from_slice(amount.as_bytes());
        buf.push(b'\n');

        // transpose for the token_holders table (topholders / holdercount / get_token_holders)
        let _ = holder;
        self.token_holders
            .entry((code.to_string(), symbol.to_string()))
            .or_default()
            .push((scope.to_string(), amount_units(&amount), amount));
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
        // pub_keys reverse index: key -> holders (account, perm, weight), for /key + WS keys.
        let acct = account.to_string();
        let perm_name = p.perm_name.clone();
        for (legacy, modern, w) in &p.keys {
            if modern.is_empty() {
                continue;
            }
            self.key_holders
                .entry(modern.clone())
                .or_insert_with(|| (legacy.clone(), Vec::new()))
                .1
                .push((acct.clone(), perm_name.clone(), *w));
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
        let mut rec: Vec<u8> = Vec::with_capacity(512);
        let empty: Vec<Perm> = Vec::new();
        for key in accts {
            // Compact binary record (procedure renders it to cc32d9 JSON at request time).
            binfmt::encode(
                &mut rec,
                self.resources.get(&key),
                self.perms.get(&key).unwrap_or(&empty),
                self.deleg_to.get(&key),
                self.deleg_from.get(&key),
                self.codehash.get(&key),
            );
            acc_index.push(IndexEntry {
                key,
                off: acc_arena.len() as u64,
                len: rec.len() as u32,
            });
            acc_arena.extend_from_slice(&rec);
        }
        let accounts = acc_index.len();
        let accinfo_tbl = Table {
            table_id: TABLE_ACCINFO,
            index: acc_index,
            arena: acc_arena,
        };

        // token_holders table: per (contract,symbol), holders sorted by amount desc. Serves HTTP
        // topholders (first N), holdercount (line count), and WS get_token_holders (stream all).
        // Blob: [u16 hdr_len]["contract:symbol"] then "account\tamount\n" lines (amount-desc).
        let mut th_index: Vec<IndexEntry> = Vec::with_capacity(self.token_holders.len());
        let mut th_arena: Vec<u8> = Vec::new();
        for ((contract, symbol), mut holders) in self.token_holders.drain() {
            // sort by units desc, tiebreak by account asc for determinism
            holders.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            let off = th_arena.len();
            let hdr = format!("{contract}:{symbol}");
            anyhow_u16(hdr.len())?;
            th_arena.extend_from_slice(&(hdr.len() as u16).to_le_bytes());
            th_arena.extend_from_slice(hdr.as_bytes());
            // holder count (u32) so /holdercount is O(1) instead of scanning every line.
            th_arena.extend_from_slice(&(holders.len() as u32).to_le_bytes());
            for (acct, _units, amount) in &holders {
                th_arena.extend_from_slice(acct.as_bytes());
                th_arena.push(b'\t');
                th_arena.extend_from_slice(amount.as_bytes());
                th_arena.push(b'\n');
            }
            let len = th_arena.len() - off;
            th_index.push(IndexEntry {
                key: token_key(&contract, &symbol),
                off: off as u64,
                len: len as u32,
            });
        }
        let tokens = th_index.len();
        let token_holders_tbl = Table {
            table_id: TABLE_TOKEN_HOLDERS,
            index: th_index,
            arena: th_arena,
        };

        // pub_keys table: key -> holders. Blob = "account\tperm\tweight\n" lines; indexed under BOTH
        // the EOS and PUB_K1 hashes (two index entries → one blob) so a query in either form matches.
        let mut pk_index: Vec<IndexEntry> = Vec::with_capacity(self.key_holders.len() * 2);
        let mut pk_arena: Vec<u8> = Vec::new();
        for (modern, (legacy, rows)) in self.key_holders.drain() {
            let off = pk_arena.len();
            for (acct, perm, w) in &rows {
                pk_arena.extend_from_slice(acct.as_bytes());
                pk_arena.push(b'\t');
                pk_arena.extend_from_slice(perm.as_bytes());
                pk_arena.push(b'\t');
                pk_arena.extend_from_slice(w.to_string().as_bytes());
                pk_arena.push(b'\n');
            }
            let len = (pk_arena.len() - off) as u32;
            pk_index.push(IndexEntry {
                key: key_hash(&modern),
                off: off as u64,
                len,
            });
            if !legacy.is_empty() && legacy != modern {
                pk_index.push(IndexEntry {
                    key: key_hash(&legacy),
                    off: off as u64,
                    len,
                });
            }
        }
        let keys = pk_index.len();
        let pub_keys_tbl = Table {
            table_id: TABLE_PUB_KEYS,
            index: pk_index,
            arena: pk_arena,
        };

        // top_ram / top_stake tables: account rankings for /topram/N and /topstake/N (N ≤ 1000).
        // One blob per table at sentinel key 0. Capped at TOP_CAP (far above the N ceiling) so the
        // segment never carries a full 21M-row ranking. (Resources tuple is (net, cpu, ram).)
        const TOP_CAP: usize = 50_000;

        // top_ram: [u32 count]["owner\tram_bytes\n"...] sorted by ram desc → cc32d9 [owner, ram].
        let mut ram_rows: Vec<(i64, u64)> = self
            .resources
            .iter()
            .filter_map(|(&k, &(_net, _cpu, ram))| if ram > 0 { Some((ram, k)) } else { None })
            .collect();
        ram_rows.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        ram_rows.truncate(TOP_CAP);
        let mut ram_arena: Vec<u8> = Vec::new();
        ram_arena.extend_from_slice(&(ram_rows.len() as u32).to_le_bytes());
        for (ram, k) in &ram_rows {
            ram_arena.extend_from_slice(name::decode(*k).as_bytes());
            ram_arena.push(b'\t');
            ram_arena.extend_from_slice(ram.to_string().as_bytes());
            ram_arena.push(b'\n');
        }
        let top_ram_tbl = Table {
            table_id: TABLE_TOP_RAM,
            index: vec![IndexEntry {
                key: 0,
                off: 0,
                len: ram_arena.len() as u32,
            }],
            arena: ram_arena,
        };

        // top_stake: [u32 count]["owner\tcpu\tnet\n"...] sorted by (cpu+net) desc. cc32d9 emits cpu
        // and net separately ([owner, cpu, net]); only the ranking uses their sum.
        let mut stake_rows: Vec<(i64, i64, i64, u64)> = self
            .resources
            .iter()
            .filter_map(|(&k, &(net, cpu, _ram))| {
                let s = net.saturating_add(cpu);
                if s > 0 {
                    Some((s, cpu, net, k))
                } else {
                    None
                }
            })
            .collect();
        stake_rows.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.3.cmp(&b.3)));
        stake_rows.truncate(TOP_CAP);
        let mut stake_arena: Vec<u8> = Vec::new();
        stake_arena.extend_from_slice(&(stake_rows.len() as u32).to_le_bytes());
        for (_s, cpu, net, k) in &stake_rows {
            stake_arena.extend_from_slice(name::decode(*k).as_bytes());
            stake_arena.push(b'\t');
            stake_arena.extend_from_slice(cpu.to_string().as_bytes());
            stake_arena.push(b'\t');
            stake_arena.extend_from_slice(net.to_string().as_bytes());
            stake_arena.push(b'\n');
        }
        let top_stake_tbl = Table {
            table_id: TABLE_TOP_STAKE,
            index: vec![IndexEntry {
                key: 0,
                off: 0,
                len: stake_arena.len() as u32,
            }],
            arena: stake_arena,
        };

        // codehash reverse index: code_hash -> accounts, for /codehash/<sha256>. The forward map
        // (account -> hash) already rides in each accinfo record; this inverts it. Blob per hash:
        // [u16 hdr_len][hash_hex]["account\n"...] (accounts asc). Keyed by fnv1a64(hash_hex), with the
        // hash header as a collision guard (mirrors token_holders).
        let mut by_hash: HashMap<&str, Vec<u64>> = HashMap::new();
        for (acct, hash) in &self.codehash {
            by_hash.entry(hash.as_str()).or_default().push(*acct);
        }
        let mut ch_index: Vec<IndexEntry> = Vec::with_capacity(by_hash.len());
        let mut ch_arena: Vec<u8> = Vec::new();
        for (hash, accts) in by_hash {
            let mut names: Vec<String> = accts.into_iter().map(name::decode).collect();
            names.sort();
            let off = ch_arena.len();
            anyhow_u16(hash.len())?;
            ch_arena.extend_from_slice(&(hash.len() as u16).to_le_bytes());
            ch_arena.extend_from_slice(hash.as_bytes());
            for n in &names {
                ch_arena.extend_from_slice(n.as_bytes());
                ch_arena.push(b'\n');
            }
            let len = (ch_arena.len() - off) as u32;
            ch_index.push(IndexEntry {
                key: key_hash(hash),
                off: off as u64,
                len,
            });
        }
        let codehashes = ch_index.len();
        let codehash_tbl = Table {
            table_id: TABLE_CODEHASH,
            index: ch_index,
            arena: ch_arena,
        };

        write_segment(
            out,
            vec![
                balances_tbl,
                accinfo_tbl,
                token_holders_tbl,
                pub_keys_tbl,
                top_ram_tbl,
                top_stake_tbl,
                codehash_tbl,
            ],
        )?;
        eprintln!("[wseg] tokens={tokens} pub_key_index_entries={keys} codehashes={codehashes}");
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
