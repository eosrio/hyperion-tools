//! Decode model: selected rows, target tables, per-worker ABI registry, stats, NDJSON formatting.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use rs_abieos::{AbiHandle, Abieos, AbieosError};

use crate::reader::{Section, Snap};

/// One selected contract-table primary row, owned so it can cross the decode-channel.
pub struct RawRow {
    pub code: u64,
    pub scope: u64,
    pub table: u64,
    pub pk: u64,
    pub payer: u64,
    pub value: Vec<u8>,
}

/// A table selector, with names pre-resolved to `u64` for the hot path.
pub enum Filter {
    All,
    Table(u64),          // any code/scope, this table  (e.g. every token's `accounts`)
    CodeTable(u64, u64), // code + table, any scope      (e.g. eosio.msig `proposal`)
    CodeScopeTable(u64, u64, u64),
}

/// Which tables to emit. A row is selected if it matches any filter.
pub struct Targets {
    pub filters: Vec<Filter>,
}

impl Targets {
    pub fn selected(&self, code: u64, scope: u64, table: u64) -> bool {
        self.filters.iter().any(|f| match f {
            Filter::All => true,
            Filter::Table(t) => table == *t,
            Filter::CodeTable(c, t) => code == *c && table == *t,
            Filter::CodeScopeTable(c, s, t) => code == *c && scope == *s && table == *t,
        })
    }
}

pub enum Dec {
    Ok,
    NoAbi,
    NoTable,
    Err,
}

/// Framing-level stats (single producer).
#[derive(Default, Debug)]
pub struct ProducerStats {
    pub tables: u64,
    pub kv_rows: u64,
    pub count_mismatches: u64,
    pub emitted: u64,
    pub limited: bool,
}

/// Decode-level stats (summed across workers).
#[derive(Default, Debug)]
pub struct WorkerStats {
    pub decoded: u64,
    pub fb_no_abi: u64,
    pub fb_no_table: u64,
    pub fb_err: u64,
    pub abis_parsed: u64,
}

impl WorkerStats {
    pub fn tally(&mut self, d: &Dec) {
        match d {
            Dec::Ok => self.decoded += 1,
            Dec::NoAbi => self.fb_no_abi += 1,
            Dec::NoTable => self.fb_no_table += 1,
            Dec::Err => self.fb_err += 1,
        }
    }
    pub fn merge(&mut self, o: WorkerStats) {
        self.decoded += o.decoded;
        self.fb_no_abi += o.fb_no_abi;
        self.fb_no_table += o.fb_no_table;
        self.fb_err += o.fb_err;
        self.abis_parsed += o.abis_parsed;
    }
}

/// Per-worker ABI registry: shared raw `abi_def` bytes (read-only) + a local, lazily-parsed
/// `AbiHandle` cache. `AbiHandle` is `Send` but not `Sync`, so each worker owns its own — never shared.
/// Carries its own `Abieos` for name<->u64 used by the action-decode path (proposals).
pub struct AbiRegistry {
    raw: Arc<HashMap<u64, Vec<u8>>>,
    cache: HashMap<u64, Option<AbiHandle>>,
    /// Cached "is this a standard token contract" verdict per code (mirrors sync-accounts `scanABIs`).
    token_cache: HashMap<u64, bool>,
    names: Abieos,
    accounts_name: u64,
    stat_name: u64,
    transfer_name: u64,
}

impl AbiRegistry {
    pub fn new(raw: Arc<HashMap<u64, Vec<u8>>>) -> Self {
        let names = Abieos::new();
        let accounts_name = names.string_to_name("accounts").unwrap_or(0);
        let stat_name = names.string_to_name("stat").unwrap_or(0);
        let transfer_name = names.string_to_name("transfer").unwrap_or(0);
        Self {
            raw,
            cache: HashMap::new(),
            token_cache: HashMap::new(),
            names,
            accounts_name,
            stat_name,
            transfer_name,
        }
    }

    /// Standard-token-contract check (replicates `sync-accounts.ts::scanABIs`): the ABI must declare
    /// both an `accounts` and a `stat` table and a `transfer` action. Filters out non-token contracts
    /// (e.g. `key.chain`) whose `accounts` table has a different row model and would otherwise create
    /// duplicate `(code, scope, symbol)` keys. Cached per code.
    pub fn is_token_contract(&mut self, code: u64) -> bool {
        if let Some(v) = self.token_cache.get(&code) {
            return *v;
        }
        let (acc, stat, xfer) = (self.accounts_name, self.stat_name, self.transfer_name);
        let verdict = self
            .handle(code)
            .map(|h| {
                h.type_for_table(acc).is_some()
                    && h.type_for_table(stat).is_some()
                    && h.type_for_action(xfer).is_some()
            })
            .unwrap_or(false);
        self.token_cache.insert(code, verdict);
        verdict
    }
    pub fn decode(&mut self, code: u64, table: u64, value: &[u8], out: &mut String) -> Dec {
        let raw = &self.raw;
        let entry = self
            .cache
            .entry(code)
            .or_insert_with(|| raw.get(&code).and_then(|b| AbiHandle::from_bin(b).ok()));
        match entry {
            Some(h) => match h.decode_table_row_into(table, value, out) {
                Ok(()) => Dec::Ok,
                Err(AbieosError::GetTypeForTable(_)) => Dec::NoTable,
                Err(_) => Dec::Err,
            },
            None => Dec::NoAbi,
        }
    }

    /// Lazily parse + cache the `AbiHandle` for `code`.
    fn handle(&mut self, code: u64) -> Option<&mut AbiHandle> {
        let raw = &self.raw;
        self.cache
            .entry(code)
            .or_insert_with(|| raw.get(&code).and_then(|b| AbiHandle::from_bin(b).ok()))
            .as_mut()
    }

    /// Decode an action's `data` (hex) against the contract's ABI, returning the decoded JSON as a
    /// `serde_json::Value`. `None` on any failure (unknown account/action ABI, bad hex, decode error) —
    /// the caller then keeps the raw hex (mirrors `sync-proposals.ts`'s per-action fallback).
    pub fn decode_action_json(
        &mut self,
        account: &str,
        action: &str,
        data_hex: &str,
    ) -> Option<serde_json::Value> {
        let code = self.names.string_to_name(account).ok()?;
        let act = self.names.string_to_name(action).ok()?;
        let bin = hex::decode(data_hex).ok()?;
        let h = self.handle(code)?;
        let ty = h.type_for_action(act)?.to_owned();
        let json = h.bin_to_json(&ty, &bin).ok()?;
        serde_json::from_str(&json).ok()
    }

    pub fn abis_parsed(&self) -> u64 {
        self.cache.values().filter(|v| v.is_some()).count() as u64
    }
}

/// Parse the `account_object` section into `code(u64) -> packed abi_def bytes`.
/// Row = name(u64) | creation_date(u32) | varuint abi_len | abi_len bytes. The varuint length is
/// consumed here, so the stored bytes are exactly what `AbiHandle::from_bin` expects. Empty ABIs skipped.
pub fn load_abis(s: &mut Snap, sec: &Section) -> Result<HashMap<u64, Vec<u8>>> {
    s.seek_to(sec.payload_off)?;
    let end = sec.payload_off + sec.payload_len;
    let mut map = HashMap::new();
    for _ in 0..sec.rows {
        let name = s.u64()?;
        let _creation_date = s.u32()?;
        let abi_len = s.varuint()? as usize;
        if abi_len == 0 {
            continue;
        }
        let mut abi = vec![0u8; abi_len];
        s.read_buf(&mut abi)?;
        map.insert(name, abi);
    }
    if s.pos != end {
        bail!(
            "account_object walk desync: consumed to {} but section ends at {}",
            s.pos,
            end
        );
    }
    Ok(map)
}

/// Format one row as a Hyperion-shaped NDJSON line (trailing newline included). On `Dec::Ok` the
/// decoded JSON is embedded under `data`; otherwise the raw value is emitted as hex under `value`.
pub fn format_line(names: &Abieos, row: &RawRow, block: u32, dec: &Dec, decoded: &str) -> String {
    use std::fmt::Write as _;
    let n = |v: u64| names.name_to_string(v).unwrap_or_else(|_| v.to_string());
    let mut s = String::with_capacity(decoded.len() + 160);
    let _ = write!(
        s,
        "{{\"code\":\"{}\",\"scope\":\"{}\",\"table\":\"{}\",\"primary_key\":\"{}\",\"payer\":\"{}\",\"block_num\":{},\"present\":true,",
        n(row.code), n(row.scope), n(row.table), row.pk, n(row.payer), block
    );
    match dec {
        Dec::Ok => {
            let _ = write!(s, "\"data\":{decoded}");
        }
        _ => {
            let _ = write!(s, "\"value\":\"{}\"", hex::encode(&row.value));
        }
    }
    s.push_str("}\n");
    s
}
