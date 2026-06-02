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

use crate::model::AbiRegistry;

/// Canonical Mongo collection names (mirrors `manager.class.ts` / the sync modules).
pub const COLL_VOTERS: &str = "voters";
pub const COLL_ACCOUNTS: &str = "accounts";
pub const COLL_PROPOSALS: &str = "proposals";

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
}

/// Route a decoded row to its (collection, doc). Returns `None` when the row cannot be mapped to the
/// requested special shape (e.g. an `accounts` row whose `data.balance` is missing/malformed) — the
/// caller skips it. `approvals2` rows are routed to `COLL_PROPOSALS` as an `__approval` carrier doc
/// that the proposals merge step consumes (they are never written verbatim).
///
/// `reg` is the per-worker ABI registry, needed only to decode `proposal.packed_transaction` actions.
pub fn map_row(
    ctx: &RowCtx,
    data: Value,
    reg: &mut AbiRegistry,
) -> Option<(&'static str, Value)> {
    match (ctx.code, ctx.table) {
        ("eosio", "voters") => map_voter(ctx, data).map(|d| (COLL_VOTERS, d)),
        (_, "accounts") => map_account(ctx, data).map(|d| (COLL_ACCOUNTS, d)),
        ("eosio.msig", "proposal") => Some((COLL_PROPOSALS, map_proposal(ctx, data, reg))),
        ("eosio.msig", "approvals2") => Some((COLL_PROPOSALS, map_approvals2(ctx, data))),
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
/// Skips rows of non-validated token contracts (mirrors sync-accounts `scanABIs`), and rows whose
/// `balance` is missing/malformed (a contract that declares an `accounts` table with a different shape).
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
    Some(json!({
        "code": ctx.code,
        "scope": ctx.scope,
        "symbol": symbol,
        "amount": amount,
        "block_num": ctx.block_num,
    }))
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
