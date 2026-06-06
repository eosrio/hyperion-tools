//! Map decoded snapshot rows → exact Hyperion MongoDB doc shapes (`IVoter` / `IAccount` /
//! `IProposal`), plus the generic dynamic contract-state doc for every other table.
//!
//! Each mapper takes the already-decoded `data` JSON (a `serde_json::Value`, produced by the worker
//! from `AbiHandle::decode_table_row`) and the row's `code/scope/table/pk/payer` names, and returns a
//! `(collection_name, serde_json::Value)` ready for the sink (NDJSON or Mongo).
//!
//! Doc-shape parity is verified against the live indexer's Mongo writes (`mongo-routes.ts`) and the
//! `hyp-control sync` modules (`sync-{voters,accounts,proposals,contract-state}.ts`). See README.

use serde_json::{json, Map, Value};

use crate::atomicassets::{self, SchemaRegistry};
use crate::atomicmarket;
use crate::model::AbiRegistry;

/// Canonical Mongo collection names (mirrors `manager.class.ts` / the sync modules).
pub const COLL_VOTERS: &str = "voters";
pub const COLL_ACCOUNTS: &str = "accounts";
pub const COLL_PROPOSALS: &str = "proposals";
/// Light-API native collections (permissions are decoded off a separate native path; see `perms.rs`).
pub const COLL_PERMISSIONS: &str = "permissions";
pub const COLL_PUB_KEYS: &str = "pub_keys";
/// Per-account contract code hash, for cc32d9 `/codehash` (from `account_metadata_object`).
pub const COLL_CODEHASH: &str = "account_codehash";

/// Decoded-row context handed to the mappers — names already rendered to strings.
pub struct RowCtx<'a> {
    pub code: &'a str,
    pub scope: &'a str,
    pub table: &'a str,
    pub primary_key: String, // raw u64 pk as decimal string (matches snapshot NDJSON)
    pub payer: &'a str,
    pub block_num: u32,
    /// Whether `code` is a validated standard token contract (only meaningful for `accounts` rows).
    pub token_ok: bool,
    /// Emit the additive Light-API fields (e.g. `accounts.decimals`/`amount_str`/`amount_num`). Gated
    /// ON for the Mongo sink / `--tables lightapi`, OFF for plain NDJSON so the existing byte-exact
    /// `accounts` line is unchanged.
    pub lightapi_fields: bool,
}

/// Route a decoded row to its (collection, doc). Returns `None` when the row cannot be mapped to the
/// requested special shape (e.g. an `accounts` row whose `data.balance` is missing/malformed) — the
/// caller skips it. `approvals2` rows are routed to `COLL_PROPOSALS` as an `__approval` carrier doc
/// that the proposals merge step consumes (they are never written verbatim).
///
/// `reg` is the per-worker ABI registry, needed only to decode `proposal.packed_transaction` actions.
/// `schema_reg` is the (read-only, shared) AtomicAssets schema-format registry — empty for every
/// non-`atomic` run, so its lookups are never reached off the atomicassets arms.
pub fn map_row(
    ctx: &RowCtx,
    data: Value,
    reg: &mut AbiRegistry,
    schema_reg: &SchemaRegistry,
) -> Option<(&'static str, Value)> {
    match (ctx.code, ctx.table) {
        ("eosio", "voters") => map_voter(ctx, data).map(|d| (COLL_VOTERS, d)),
        (_, "accounts") => map_account(ctx, data).map(|d| (COLL_ACCOUNTS, d)),
        ("eosio.msig", "proposal") => Some((COLL_PROPOSALS, map_proposal(ctx, data, reg))),
        ("eosio.msig", "approvals2") => Some((COLL_PROPOSALS, map_approvals2(ctx, data))),
        ("eosio", "userres") => Some((dynamic_collection(ctx), map_userres(ctx, data))),
        // AtomicAssets state (the `atomicassets`/`atomic` preset). `assets`/`templates`/`collections`
        // decode their `serialized_data` against `schema_reg`.
        ("atomicassets", "schemas") => Some((
            atomicassets::COLL_AA_SCHEMAS,
            atomicassets::map_schema(ctx, data),
        )),
        ("atomicassets", "collections") => Some((
            atomicassets::COLL_AA_COLLECTIONS,
            atomicassets::map_collection(ctx, data, schema_reg),
        )),
        ("atomicassets", "templates") => Some((
            atomicassets::COLL_AA_TEMPLATES,
            atomicassets::map_template(ctx, data, schema_reg),
        )),
        ("atomicassets", "assets") => Some((
            atomicassets::COLL_AA_ASSETS,
            atomicassets::map_asset(ctx, data, schema_reg),
        )),
        ("atomicassets", "offers") => Some((
            atomicassets::COLL_AA_OFFERS,
            atomicassets::map_offer(ctx, data),
        )),
        ("atomicassets", "config") => Some((
            atomicassets::COLL_AA_CONFIG,
            atomicassets::map_config(ctx, data, schema_reg),
        )),
        // AtomicMarket state (the `atomicmarket`/`atomic` preset).
        ("atomicmarket", "sales") => Some((
            atomicmarket::COLL_AM_SALES,
            atomicmarket::map_sale(ctx, data),
        )),
        ("atomicmarket", "auctions") => Some((
            atomicmarket::COLL_AM_AUCTIONS,
            atomicmarket::map_auction(ctx, data),
        )),
        ("atomicmarket", "buyoffers") => Some((
            atomicmarket::COLL_AM_BUYOFFERS,
            atomicmarket::map_buyoffer(ctx, data),
        )),
        ("atomicmarket", "tbuyoffers") => Some((
            atomicmarket::COLL_AM_TEMPLATE_BUYOFFERS,
            atomicmarket::map_template_buyoffer(ctx, data),
        )),
        ("atomicmarket", "marketplaces") => Some((
            atomicmarket::COLL_AM_MARKETPLACES,
            atomicmarket::map_marketplace(ctx, data),
        )),
        ("atomicmarket", "config") => Some((
            atomicmarket::COLL_AM_CONFIG,
            atomicmarket::map_config(ctx, data),
        )),
        _ => Some((dynamic_collection(ctx), map_dynamic(ctx, data))),
    }
}

/// Dynamic contract-state collection name: `${code}-${table}` (mirrors `sync-contract-state.ts`).
/// Leaked to `'static` so it can ride the typed sink channel without per-doc allocation churn — there
/// are only a handful of distinct dynamic collections per load, so the leak is bounded and tiny.
fn dynamic_collection(ctx: &RowCtx) -> &'static str {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::OnceLock;
    static INTERN: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let map = INTERN.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!("{}-{}", ctx.code, ctx.table);
    let mut g = map.lock().unwrap();
    if let Some(s) = g.get(&key) {
        return s;
    }
    let leaked: &'static str = Box::leak(key.clone().into_boxed_str());
    g.insert(key, leaked);
    leaked
}

// ── VOTERS ─────────────────────────────────────────────────────────────────────────────────────

/// `voter_info` → `IVoter`. `is_proxy` 0/1 → bool; weights stay strings; `staked` stays a number;
/// `primary_key` is the row's pk (already `Name(owner).value` as decimal string).
fn map_voter(ctx: &RowCtx, data: Value) -> Option<Value> {
    let owner = data.get("owner").and_then(Value::as_str).unwrap_or("");
    let is_proxy = match data.get("is_proxy") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) != 0,
        _ => false,
    };
    let producers = data
        .get("producers")
        .cloned()
        .unwrap_or_else(|| Value::Array(vec![]));
    Some(json!({
        "voter": owner,
        "primary_key": ctx.primary_key,
        "is_proxy": is_proxy,
        "last_vote_weight": value_to_str(data.get("last_vote_weight")),
        "proxied_vote_weight": value_to_str(data.get("proxied_vote_weight")),
        "staked": value_to_num(data.get("staked")),
        "producers": producers,
        "proxy": data.get("proxy").and_then(Value::as_str).unwrap_or(""),
        "block_num": ctx.block_num,
    }))
}

// ── ACCOUNTS ───────────────────────────────────────────────────────────────────────────────────

/// `{balance:"<amount> <SYM>"}` → `IAccount`. `code` = token contract, `scope` = holder.
/// Skips rows of non-validated token contracts (the `scanABIs` contract-eligibility filter in
/// `model::transfer_fields_match`), and rows whose `balance` is missing/malformed (a contract that
/// declares an `accounts` table with a different shape).
///
/// This `balance`-derived `symbol` is what actually upholds the unique `(code, scope, symbol)` index:
/// a standard eosio.token `accounts` table has one row per symbol per scope, so the validated set
/// cannot produce a duplicate key. The contract-level `scanABIs` filter is the first gate; this
/// per-row balance parse is the second.
fn map_account(ctx: &RowCtx, data: Value) -> Option<Value> {
    if !ctx.token_ok {
        return None;
    }
    let balance = data.get("balance").and_then(Value::as_str)?;
    let (amount_s, symbol) = balance.split_once(' ')?;
    if symbol.is_empty() || symbol.len() > 7 || !symbol.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    let amount: f64 = amount_s.parse().ok()?;
    let mut doc = Map::new();
    doc.insert("code".into(), json!(ctx.code));
    doc.insert("scope".into(), json!(ctx.scope));
    doc.insert("symbol".into(), json!(symbol));
    // `amount` stays an f64 for Hyperion parity (and is the always-present sort key for /topholders).
    doc.insert("amount".into(), json!(amount));
    doc.insert("block_num".into(), json!(ctx.block_num));
    if ctx.lightapi_fields {
        // Additive Light-API fields, computed from the raw asset string before the lossy f64 parse:
        //   decimals   = fractional-digit count        (Light API returns it as a string)
        //   amount_str = exact decimal string           (preserves trailing zeros + big-supply precision)
        //   amount_num = same f64 (explicit numeric sort companion, mirrors `amount`)
        let decimals = amount_s.split_once('.').map(|(_, f)| f.len()).unwrap_or(0);
        doc.insert("decimals".into(), json!(decimals as i32));
        doc.insert("amount_str".into(), json!(amount_s));
        doc.insert("amount_num".into(), json!(amount));
    }
    Some(Value::Object(doc))
}

// ── PROPOSALS ──────────────────────────────────────────────────────────────────────────────────

/// `proposal` struct → partial `IProposal` (proposer, proposal_name, trx, expiration). `version` and
/// the approval arrays are merged later from the `approvals2` carrier docs (see [`map_approvals2`]).
/// `packed_transaction` is unpacked + per-action decoded via the embedded `transaction` ABI.
fn map_proposal(ctx: &RowCtx, data: Value, reg: &mut AbiRegistry) -> Value {
    let proposal_name = data
        .get("proposal_name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut doc = Map::new();
    doc.insert("proposal_name".into(), json!(proposal_name));
    doc.insert("proposer".into(), json!(ctx.scope));

    let packed_hex = data.get("packed_transaction").and_then(Value::as_str);
    match packed_hex.and_then(|h| decode_packed_transaction(h, reg)) {
        Some(trx) => {
            if let Some(exp) = trx.get("expiration").and_then(Value::as_str) {
                // BSON extended-JSON Date so the sink stores a real Date (mirrors `new Date(...)`).
                doc.insert("expiration".into(), json!({ "$date": exp }));
            }
            doc.insert("trx".into(), trx);
        }
        None => {
            // Whole-transaction fallback: keep packed_transaction as hex, omit trx/expiration.
            if let Some(h) = packed_hex {
                doc.insert("packed_transaction".into(), json!(h));
            }
        }
    }
    Value::Object(doc)
}

/// `approvals2` row → carrier doc consumed by the proposals merge. Flattens `level` into
/// `{actor, permission, time}` and tags with `__approval` + `__proposal_name` so the merge can join
/// on `(proposer, proposal_name)` and never write this doc verbatim.
fn map_approvals2(ctx: &RowCtx, data: Value) -> Value {
    let flatten = |arr: Option<&Value>| -> Value {
        let mut out = Vec::new();
        if let Some(Value::Array(items)) = arr {
            for it in items {
                let level = it.get("level");
                out.push(json!({
                    "actor": level.and_then(|l| l.get("actor")).and_then(Value::as_str).unwrap_or(""),
                    "permission": level.and_then(|l| l.get("permission")).and_then(Value::as_str).unwrap_or(""),
                    "time": it.get("time").cloned().unwrap_or(Value::Null),
                }));
            }
        }
        Value::Array(out)
    };
    json!({
        "__approval": true,
        "__proposer": ctx.scope,
        "__proposal_name": data.get("proposal_name").and_then(Value::as_str).unwrap_or(""),
        "version": data.get("version").cloned().unwrap_or(Value::Null),
        "requested_approvals": flatten(data.get("requested_approvals")),
        "provided_approvals": flatten(data.get("provided_approvals")),
    })
}

/// Embedded `transaction` ABI (copied verbatim from `rs-abieos/abis/transaction.abi.json`). Lets one
/// `bin_to_json("transaction", …)` call walk the whole header + actions + extensions.
const TRANSACTION_ABI: &str = include_str!("../abis/transaction.abi.json");

thread_local! {
    static TRX_ABI: std::cell::RefCell<Option<rs_abieos::AbiHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// Unpack an eosio.msig `proposal.packed_transaction` (a bare serialized `transaction`, compression
/// none) into the objectified `trx` shape, decoding each action's `data` against its contract ABI.
/// Per-action fallback: keep `data` as hex when the contract ABI / action type / decode is missing.
/// Returns `None` on whole-transaction failure (caller keeps the hex).
fn decode_packed_transaction(packed_hex: &str, reg: &mut AbiRegistry) -> Option<Value> {
    let bin = hex::decode(packed_hex).ok()?;
    let trx_json = TRX_ABI.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = rs_abieos::AbiHandle::from_json(TRANSACTION_ABI).ok();
        }
        slot.as_mut()?.bin_to_json("transaction", &bin).ok()
    })?;
    let mut trx: Value = serde_json::from_str(&trx_json).ok()?;

    // Re-decode each action's hex `data` against the action's contract ABI, in place.
    for key in ["context_free_actions", "actions"] {
        if let Some(Value::Array(actions)) = trx.get_mut(key) {
            for action in actions.iter_mut() {
                decode_action_data_inplace(action, reg);
            }
        }
    }
    Some(trx)
}

/// Replace one action's hex `data` with the decoded object, in place. Leaves the hex on any failure
/// (missing ABI / unknown action / decode error) — mirrors `sync-proposals.ts`'s per-action `catch`.
fn decode_action_data_inplace(action: &mut Value, reg: &mut AbiRegistry) {
    let (Some(account), Some(name), Some(data_hex)) = (
        action.get("account").and_then(Value::as_str),
        action.get("name").and_then(Value::as_str),
        action.get("data").and_then(Value::as_str),
    ) else {
        return;
    };
    if let Some(decoded) = reg.decode_action_json(account, name, data_hex) {
        if let Value::Object(map) = action {
            map.insert("data".into(), decoded);
        }
    }
}

// ── USERRES (eosio.userresources) ────────────────────────────────────────────────────────────────

/// `eosio:userres` rows, as the dynamic doc PLUS a numeric `stake` companion (Light-API only).
///
/// `/topstake` ranks accounts by `net_weight + cpu_weight`, but those are asset *strings*
/// ("x.xxxx SYM"). Without a numeric field the server can only sort on a computed `$split` expression
/// — which no index can serve, so at WAX scale (21.75M rows) it degrades to a full in-memory sort
/// (~33s/request). Emitting an integer `stake` (sum of both legs in base units) lets the server sort
/// on an indexed field, exactly as `amount_num` does for `/topholders`. net/cpu share the system
/// token's precision, so summing base units is exact and monotonic.
fn map_userres(ctx: &RowCtx, data: Value) -> Value {
    let stake = ctx.lightapi_fields.then(|| {
        let leg = |f: &str| {
            data.get(f)
                .and_then(Value::as_str)
                .and_then(asset_units)
                .unwrap_or(0)
        };
        leg("net_weight").saturating_add(leg("cpu_weight"))
    });
    let mut doc = map_dynamic(ctx, data);
    if let (Some(s), Value::Object(map)) = (stake, &mut doc) {
        map.insert("stake".into(), json!(s));
    }
    doc
}

// ── DYNAMIC CONTRACT-STATE ───────────────────────────────────────────────────────────────────────

/// Every non-special table → the dynamic contract-state doc: `@`-prefixed system fields spread with
/// the decoded `data` (mirrors `sync-contract-state.ts`). The snapshot has no block_id/time, so those
/// are emitted empty (the live indexer fills them; the API does not require them for reads).
fn map_dynamic(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = Map::new();
    doc.insert("@scope".into(), json!(ctx.scope));
    doc.insert("@pk".into(), json!(ctx.primary_key));
    doc.insert("@payer".into(), json!(ctx.payer));
    doc.insert("@block_num".into(), json!(ctx.block_num));
    doc.insert("@block_id".into(), json!(""));
    doc.insert("@block_time".into(), json!(""));
    if let Value::Object(fields) = data {
        for (k, v) in fields {
            doc.insert(k, v);
        }
    }
    Value::Object(doc)
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

/// Parse an asset string ("11.30000000 WAX") to its integer base units (1130000000), ignoring the
/// symbol. Decimal point dropped, digits concatenated — exact for sorting/summing same-precision legs.
fn asset_units(s: &str) -> Option<i64> {
    let num = s.split(' ').next()?;
    let (int_part, frac) = num.split_once('.').unwrap_or((num, ""));
    format!("{int_part}{frac}").parse::<i64>().ok()
}

/// abieos renders float64 as a JSON string already; keep strings as-is, stringify numbers, default "".
fn value_to_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

/// `IVoter.staked` is a `number`, but abieos renders int64 as a JSON string. Parse it back to a number
/// (falls through to the original value, then 0). Matches the live indexer's numeric `staked`.
fn value_to_num(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Number(n)) => Value::Number(n.clone()),
        Some(Value::String(s)) => s
            .parse::<i64>()
            .ok()
            .map(|i| Value::Number(i.into()))
            .or_else(|| {
                s.parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
            })
            .unwrap_or(Value::Number(0.into())),
        _ => Value::Number(0.into()),
    }
}

#[cfg(test)]
mod asset_units_tests {
    use super::asset_units;

    #[test]
    fn parses_assets_to_base_units() {
        assert_eq!(asset_units("11.30000000 WAX"), Some(1_130_000_000));
        assert_eq!(asset_units("0.00000000 WAX"), Some(0));
        assert_eq!(asset_units("1.0000 EOS"), Some(10_000));
        assert_eq!(asset_units("100 WAX"), Some(100)); // integer asset, no fractional part
        assert_eq!(asset_units("garbage"), None);
    }

    #[test]
    fn sum_is_monotonic_for_same_precision_legs() {
        // net + cpu (same symbol) → exact integer sum used as the /topstake sort key.
        let net = asset_units("5.00000000 WAX").unwrap();
        let cpu = asset_units("6.30000000 WAX").unwrap();
        assert_eq!(net + cpu, 1_130_000_000);
    }
}
