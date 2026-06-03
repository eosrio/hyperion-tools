//! Decode model: selected rows, target tables, per-worker ABI registry, stats, NDJSON formatting.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{bail, Result};
use rs_abieos::{AbiHandle, Abieos, AbieosError};

use crate::reader::{Section, SnapRead};

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

    /// Standard-token-contract check — mirrors `sync-accounts.ts::scanABIs`' contract-eligibility
    /// test. The ABI must declare both an `accounts` and a `stat` table, a `transfer` action, AND the
    /// `transfer` struct's first four fields must be `from:name, to:name, quantity:asset, memo:string`
    /// (accepting the `account_name` alias for `from`/`to`). The field check is what rejects non-token contracts that
    /// merely *look* token-shaped — e.g. WAX `simpleassets` (an NFT contract whose `transfer` takes
    /// `assetids`, not `quantity:asset`), whose `accounts` rows otherwise collide on the unique
    /// `(code, scope, symbol)` index. Cached per code.
    pub fn is_token_contract(&mut self, code: u64) -> bool {
        if let Some(v) = self.token_cache.get(&code) {
            return *v;
        }
        let verdict = self.compute_token_verdict(code);
        self.token_cache.insert(code, verdict);
        verdict
    }

    fn compute_token_verdict(&mut self, code: u64) -> bool {
        // Fast pre-filter via the already-parsed AbiHandle: needs accounts + stat tables + a transfer
        // action. Avoids the abi_bin_to_json parse below for the common non-token case.
        let (acc, stat, xfer) = (self.accounts_name, self.stat_name, self.transfer_name);
        let basic = self
            .handle(code)
            .map(|h| {
                h.type_for_table(acc).is_some()
                    && h.type_for_table(stat).is_some()
                    && h.type_for_action(xfer).is_some()
            })
            .unwrap_or(false);
        if !basic {
            return false;
        }
        // scanABIs field check: render the packed ABI to JSON and validate the `transfer` struct.
        // (The prefilter above already parsed these bytes via `AbiHandle`; this re-render is the only
        // way to reach the struct fields, and the `Err` arm is therefore defensive.)
        let Some(raw) = self.raw.get(&code) else {
            return false;
        };
        match self.names.abi_bin_to_json(raw) {
            Ok(json) => transfer_fields_match(&json),
            Err(_) => false, // can't validate the ABI -> exclude (scanABIs likewise skips on ABI error)
        }
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

/// `true` iff the ABI JSON's `transfer` struct's first four fields are
/// `from:name, to:name, quantity:asset, memo:string` — the eosio.token signature — accepting the
/// legacy `account_name` alias for `from`/`to` at their expected positions. Mirrors the candidate
/// selection in `sync-accounts.ts::scanABIs`.
///
/// Note: this is the *contract-eligibility* test. The `(code, scope, symbol)` uniqueness that the
/// `accounts` index relies on is upheld downstream by `map::map_account`, which derives `symbol` from
/// each row's decoded `balance` asset (a standard token has one row per symbol per scope) and drops
/// rows with no parseable balance — not by this function.
fn transfer_fields_match(abi_json: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(abi_json) else {
        return false;
    };
    let Some(structs) = v.get("structs").and_then(|s| s.as_array()) else {
        return false;
    };
    let Some(transfer) = structs
        .iter()
        .find(|s| s.get("name").and_then(|n| n.as_str()) == Some("transfer"))
    else {
        return false;
    };
    let Some(fields) = transfer.get("fields").and_then(|f| f.as_array()) else {
        return false;
    };
    const EXPECT: [(&str, &str); 4] = [
        ("from", "name"),
        ("to", "name"),
        ("quantity", "asset"),
        ("memo", "string"),
    ];
    if fields.len() < EXPECT.len() {
        return false;
    }
    for (i, (ename, etype)) in EXPECT.iter().enumerate() {
        let fname = fields[i].get("name").and_then(|x| x.as_str()).unwrap_or("");
        let ftype = fields[i].get("type").and_then(|x| x.as_str()).unwrap_or("");
        if fname != *ename {
            return false;
        }
        // `account_name` is the legacy alias for `name`, accepted only on from/to and only at their
        // expected position. (scanABIs accepts it position-blind, but a working chain never swaps
        // from/to, so this is behavior-identical on every real token ABI while being strictly tighter.)
        let type_ok =
            ftype == *etype || ((*ename == "from" || *ename == "to") && ftype == "account_name");
        if !type_ok {
            return false;
        }
    }
    true
}

/// Parse the `account_object` section into `code(u64) -> packed abi_def bytes`.
/// Row = name(u64) | creation_date(u32) | varuint abi_len | abi_len bytes. The varuint length is
/// consumed here, so the stored bytes are exactly what `AbiHandle::from_bin` expects. Empty ABIs skipped.
pub fn load_abis(s: &mut impl SnapRead, sec: &Section) -> Result<HashMap<u64, Vec<u8>>> {
    s.seek_to(sec.payload_off)?;
    let end = sec.payload_off + sec.payload_len;
    let mut map = HashMap::new();
    for _ in 0..sec.rows {
        let name = s.u64()?;
        let _creation_date = s.u32()?;
        let abi_len_u = s.varuint()?;
        // `abi_len` is an untrusted varuint driving `vec![0u8; abi_len]`; reject a length that cannot
        // fit in the bytes remaining before the section end (prevents a huge alloc on malformed input).
        let remaining = end.saturating_sub(s.pos());
        if abi_len_u > remaining {
            bail!(
                "account_object: abi length {abi_len_u} exceeds {remaining} bytes remaining in section at offset {}",
                s.pos()
            );
        }
        // 64-bit on disk; `try_from` (not `as`) so a >usize::MAX length bails instead of truncating
        // (a no-op on 64-bit; correct on a 32-bit target).
        let abi_len = usize::try_from(abi_len_u).map_err(|_| {
            anyhow::anyhow!("account_object: abi length {abi_len_u} overflows usize")
        })?;
        if abi_len == 0 {
            continue;
        }
        let mut abi = vec![0u8; abi_len];
        s.read_buf(&mut abi)?;
        map.insert(name, abi);
    }
    if s.pos() != end {
        bail!(
            "account_object walk desync: consumed to {} but section ends at {}",
            s.pos(),
            end
        );
    }
    Ok(map)
}

/// Parse the `account_metadata_object` section → `(account_name u64, code_hash [u8;32])` for every
/// account that actually has a contract (non-zero hash). The snapshot row is a fixed 86 bytes in
/// Spring's `FC_REFLECT` order:
///   name(u64) | recv/auth/code/abi_sequence(4×u64) | code_hash(32) | last_code_update(i64)
///   | flags(u32) | vm_type(u8) | vm_version(u8)
/// so `code_hash` is at offset 40. Powers cc32d9 `/codehash` (account ↔ contract hash).
pub fn load_codehashes(s: &mut impl SnapRead, sec: &Section) -> Result<Vec<(u64, [u8; 32])>> {
    s.seek_to(sec.payload_off)?;
    let end = sec.payload_off + sec.payload_len;
    let mut out = Vec::new();
    let mut skip4 = [0u8; 32]; // recv/auth/code/abi sequences
    let mut tail = [0u8; 14]; // last_code_update(8) + flags(4) + vm_type(1) + vm_version(1)
    for _ in 0..sec.rows {
        let name = s.u64()?;
        s.read_buf(&mut skip4)?;
        let mut hash = [0u8; 32];
        s.read_buf(&mut hash)?;
        s.read_buf(&mut tail)?;
        if hash != [0u8; 32] {
            out.push((name, hash));
        }
    }
    if s.pos() != end {
        bail!(
            "account_metadata_object walk desync: consumed to {} but section ends at {}",
            s.pos(),
            end
        );
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::transfer_fields_match;

    fn abi_with_transfer(fields: &str) -> String {
        format!(
            r#"{{"version":"eosio::abi/1.2","structs":[{{"name":"transfer","base":"","fields":[{fields}]}}]}}"#
        )
    }

    #[test]
    fn standard_eosio_token_transfer_matches() {
        let abi = abi_with_transfer(
            r#"{"name":"from","type":"name"},{"name":"to","type":"name"},{"name":"quantity","type":"asset"},{"name":"memo","type":"string"}"#,
        );
        assert!(transfer_fields_match(&abi));
    }

    #[test]
    fn account_name_alias_for_from_to_matches() {
        let abi = abi_with_transfer(
            r#"{"name":"from","type":"account_name"},{"name":"to","type":"account_name"},{"name":"quantity","type":"asset"},{"name":"memo","type":"string"}"#,
        );
        assert!(transfer_fields_match(&abi));
    }

    #[test]
    fn extra_trailing_fields_are_ignored() {
        // scanABIs only validates the first four fields; a 5th is fine.
        let abi = abi_with_transfer(
            r#"{"name":"from","type":"name"},{"name":"to","type":"name"},{"name":"quantity","type":"asset"},{"name":"memo","type":"string"},{"name":"extra","type":"uint64"}"#,
        );
        assert!(transfer_fields_match(&abi));
    }

    #[test]
    fn simpleassets_style_nft_transfer_rejected() {
        // simpleassets: transfer(from, to, assetids[], memo) — field[2] is not quantity:asset.
        let abi = abi_with_transfer(
            r#"{"name":"from","type":"name"},{"name":"to","type":"name"},{"name":"assetids","type":"uint64[]"},{"name":"memo","type":"string"}"#,
        );
        assert!(!transfer_fields_match(&abi));
    }

    #[test]
    fn too_few_fields_rejected() {
        let abi = abi_with_transfer(r#"{"name":"from","type":"name"},{"name":"to","type":"name"}"#);
        assert!(!transfer_fields_match(&abi));
    }

    #[test]
    fn no_transfer_struct_rejected() {
        let abi =
            r#"{"version":"eosio::abi/1.2","structs":[{"name":"issue","base":"","fields":[]}]}"#;
        assert!(!transfer_fields_match(abi));
    }

    #[test]
    fn malformed_json_rejected() {
        assert!(!transfer_fields_match("not json"));
    }

    #[test]
    fn swapped_from_to_account_name_rejected() {
        // The alias must be position-correct: from/to physically swapped (both account_name) must NOT
        // pass, even though each named field uses the legacy alias. (Bot-flagged regression.)
        let abi = abi_with_transfer(
            r#"{"name":"to","type":"account_name"},{"name":"from","type":"account_name"},{"name":"quantity","type":"asset"},{"name":"memo","type":"string"}"#,
        );
        assert!(!transfer_fields_match(&abi));
    }

    #[test]
    fn account_name_alias_on_wrong_field_rejected() {
        // account_name is accepted only for from/to, not for a differently-named first field.
        let abi = abi_with_transfer(
            r#"{"name":"sender","type":"account_name"},{"name":"to","type":"name"},{"name":"quantity","type":"asset"},{"name":"memo","type":"string"}"#,
        );
        assert!(!transfer_fields_match(&abi));
    }

    /// Integration test for the whole verdict path: pack real ABIs with `abi_json_to_bin`, then drive
    /// `AbiRegistry::is_token_contract` so the `AbiHandle` prefilter, the `abi_bin_to_json` re-render,
    /// `transfer_fields_match`, and the per-code cache all run together.
    #[test]
    fn compute_token_verdict_accepts_token_rejects_nft() {
        use super::AbiRegistry;
        use rs_abieos::Abieos;
        use std::collections::HashMap;
        use std::sync::Arc;

        let mk_abi = |transfer_fields: &str| -> String {
            let mut s = String::new();
            s.push_str(r#"{"version":"eosio::abi/1.1","types":[],"structs":["#);
            s.push_str(
                r#"{"name":"account","base":"","fields":[{"name":"balance","type":"asset"}]},"#,
            );
            s.push_str(r#"{"name":"currency_stats","base":"","fields":[{"name":"supply","type":"asset"},{"name":"max_supply","type":"asset"},{"name":"issuer","type":"name"}]},"#);
            s.push_str(r#"{"name":"transfer","base":"","fields":["#);
            s.push_str(transfer_fields);
            s.push_str(r#"]}],"#);
            s.push_str(
                r#""actions":[{"name":"transfer","type":"transfer","ricardian_contract":""}],"#,
            );
            s.push_str(r#""tables":[{"name":"accounts","index_type":"i64","key_names":[],"key_types":[],"type":"account"},{"name":"stat","index_type":"i64","key_names":[],"key_types":[],"type":"currency_stats"}],"#);
            s.push_str(
                r#""ricardian_clauses":[],"error_messages":[],"abi_extensions":[],"variants":[]}"#,
            );
            s
        };
        let token_abi = mk_abi(
            r#"{"name":"from","type":"name"},{"name":"to","type":"name"},{"name":"quantity","type":"asset"},{"name":"memo","type":"string"}"#,
        );
        let nft_abi = mk_abi(
            r#"{"name":"from","type":"name"},{"name":"to","type":"name"},{"name":"assetids","type":"uint64[]"},{"name":"memo","type":"string"}"#,
        );

        let abieos = Abieos::new();
        let tkn = abieos.string_to_name("tkn").unwrap();
        let nft = abieos.string_to_name("nft").unwrap();
        let mut map: HashMap<u64, Vec<u8>> = HashMap::new();
        map.insert(
            tkn,
            abieos.abi_json_to_bin(&token_abi).expect("pack token abi"),
        );
        map.insert(nft, abieos.abi_json_to_bin(&nft_abi).expect("pack nft abi"));

        let mut reg = AbiRegistry::new(Arc::new(map));
        assert!(
            reg.is_token_contract(tkn),
            "standard eosio.token-shaped contract should qualify"
        );
        assert!(
            !reg.is_token_contract(nft),
            "simpleassets-style NFT (transfer takes assetids) should be rejected"
        );
        // second call hits the cache and yields the same verdict
        assert!(reg.is_token_contract(tkn));
        assert!(!reg.is_token_contract(nft));
    }
}
