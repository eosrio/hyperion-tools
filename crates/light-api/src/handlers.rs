//! Axum handlers for every cc32d9 `eosio_light_api` endpoint. Each reads the per-chain Mongo DB and
//! shapes the response to match the live API (JSON or plain text, honoring `?pretty=1`).

use axum::extract::{Path, State};
use axum::response::Response;
use futures::future::join_all;
use mongodb::bson::{doc, Bson, Document};
use serde_json::{json, Value};

use crate::db::{self, bson_i64, bson_string};
use crate::error::ApiError;
use crate::respond::{self, PrettyQ};
use crate::rex::{self, RexInput, RexPool};
use crate::state::AppState;
use crate::{asset, keyfmt};

// ── multi-chain ─────────────────────────────────────────────────────────────────────────────────

/// `GET /api/networks` — one object per configured network with live block info.
pub async fn networks(State(st): State<AppState>, pretty: PrettyQ) -> Result<Response, ApiError> {
    let futs = st.networks.iter().map(|n| {
        let st = st.clone();
        let name = n.name.clone();
        async move { st.chain_block(&name, false).await }
    });
    let blocks = join_all(futs).await;
    let mut out = Vec::new();
    for b in blocks {
        out.push(b?);
    }
    Ok(respond::json(Value::Array(out), pretty.on()))
}

/// `GET /api/status` — `OK` if every configured chain is within its sync threshold, else
/// `OUT_OF_SYNC`.
pub async fn status(State(st): State<AppState>) -> Result<Response, ApiError> {
    let futs = st.networks.iter().map(|n| {
        let st = st.clone();
        let name = n.name.clone();
        async move { (name.clone(), st.chain_meta(&name).await) }
    });
    let results = join_all(futs).await;
    // cc32d9 format: "OK" if all in sync, else "OUT_OF_SYNC <chain>:<delay>;..." per lagging chain.
    let mut lagging = String::new();
    for (name, res) in &results {
        let out_of_sync = !matches!(res, Ok(m) if m.in_sync);
        if out_of_sync {
            let delay = res.as_ref().map(|m| m.sync).unwrap_or(0);
            lagging.push_str(&format!(" {name}:{delay};"));
        }
    }
    let body = if lagging.is_empty() {
        "OK".to_string()
    } else {
        format!("OUT_OF_SYNC{lagging}")
    };
    Ok(respond::text(body))
}

/// `GET /api/sync/CHAIN` — `<delay_seconds> OK|OUT_OF_SYNC`.
pub async fn sync(
    State(st): State<AppState>,
    Path(chain): Path<String>,
) -> Result<Response, ApiError> {
    let m = st.chain_meta(&chain).await?;
    let tag = if m.in_sync { "OK" } else { "OUT_OF_SYNC" };
    Ok(respond::text(format!("{} {}", m.sync, tag)))
}

// ── balances / tokens ─────────────────────────────────────────────────────────────────────────

/// Shape one `accounts` doc into `{contract, currency, decimals, amount}` (decimals/amount as strings,
/// tolerating both the legacy `{amount:f64}` and the new `{amount_str, decimals}` schema).
fn balance_json(d: &Document, net_decimals: u32) -> Value {
    let code = d.get_str("code").unwrap_or("");
    let symbol = d.get_str("symbol").unwrap_or("");
    let decimals = bson_i64(d.get("decimals"))
        .or_else(|| {
            d.get_str("amount_str")
                .ok()
                .and_then(|s| s.split_once('.').map(|(_, f)| f.len() as i64))
        })
        .unwrap_or(net_decimals as i64);
    let amount = d
        .get_str("amount_str")
        .map(|s| s.to_string())
        .ok()
        .or_else(|| {
            db::bson_f64(d.get("amount")).map(|f| format!("{:.*}", decimals.max(0) as usize, f))
        })
        .unwrap_or_else(|| "0".to_string());
    json!({
        "contract": code,
        "currency": symbol,
        "decimals": decimals.to_string(),
        "amount": amount,
    })
}

async fn load_balances(st: &AppState, chain: &str, account: &str) -> Result<Vec<Value>, ApiError> {
    let net_decimals = st.network(chain)?.decimals;
    let db = st.db_for(chain)?;
    let docs = st
        .find(&db, "accounts", doc! { "scope": account }, None, None, None)
        .await?;
    Ok(docs.iter().map(|d| balance_json(d, net_decimals)).collect())
}

/// `GET /api/balances/CHAIN/ACCT`. cc32d9 wraps the array in `{account_name, chain, balances}`.
pub async fn balances(
    State(st): State<AppState>,
    Path((chain, account)): Path<(String, String)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let (bal, chain_block) = tokio::join!(
        load_balances(&st, &chain, &account),
        st.chain_block(&chain, true),
    );
    let out = json!({
        "account_name": account,
        "chain": chain_block?,
        "balances": Value::Array(bal?),
    });
    Ok(respond::json(out, pretty.on()))
}

/// `GET /api/tokenbalance/CHAIN/ACCT/CONTRACT/SYM` — plain-text numeric balance (`0` if none).
pub async fn tokenbalance(
    State(st): State<AppState>,
    Path((chain, account, contract, symbol)): Path<(String, String, String, String)>,
) -> Result<Response, ApiError> {
    let db = st.db_for(&chain)?;
    let net_decimals = st.network(&chain)?.decimals;
    let found = st
        .find_one(
            &db,
            "accounts",
            doc! { "scope": account, "code": contract, "symbol": symbol },
        )
        .await?;
    let body = match found {
        Some(d) => {
            let b = balance_json(&d, net_decimals);
            b.get("amount")
                .and_then(Value::as_str)
                .unwrap_or("0")
                .to_string()
        }
        None => "0".to_string(),
    };
    Ok(respond::text(body))
}

/// `GET /api/holdercount/CHAIN/CONTRACT/SYM` — plain-text holder count.
/// Cached: `count_documents` over a popular token can scan millions of index entries at chain scale.
pub async fn holdercount(
    State(st): State<AppState>,
    Path((chain, contract, symbol)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    st.network(&chain)?;
    let key = format!("hc:{chain}:{contract}:{symbol}");
    let st2 = st.clone();
    let n = st
        .cached_count(key, move || async move {
            let db = st2.db_for(&chain).ok()?;
            st2.count(&db, "accounts", doc! { "code": contract, "symbol": symbol })
                .await
                .ok()
        })
        .await;
    Ok(respond::text(n.to_string()))
}

/// `GET /api/usercount/CHAIN` — plain-text total account count (distinct accounts in `permissions`).
/// Cached + background-refreshed: the `distinct` scan is O(all permissions) — seconds-to-minutes at
/// chain scale, so it never runs inside a request (mirrors cc32d9's count cron).
pub async fn usercount(
    State(st): State<AppState>,
    Path(chain): Path<String>,
) -> Result<Response, ApiError> {
    st.network(&chain)?;
    let key = format!("uc:{chain}");
    let st2 = st.clone();
    let n = st
        .cached_count(key, move || async move {
            let db = st2.db_for(&chain).ok()?;
            st2.distinct_count(&db, "permissions", "account", doc! {})
                .await
                .ok()
        })
        .await;
    Ok(respond::text(n.to_string()))
}

/// Kick off background computation of the expensive `usercount` scan for every chain at startup, so
/// the first user request doesn't see a cold `0` (and the scan never runs inside a request).
pub async fn warm_counts(st: &AppState) {
    for net in st.networks.clone() {
        let chain = net.name;
        let key = format!("uc:{chain}");
        let st2 = st.clone();
        let _ = st
            .cached_count(key, move || async move {
                let db = st2.db_for(&chain).ok()?;
                st2.distinct_count(&db, "permissions", "account", doc! {})
                    .await
                    .ok()
            })
            .await;
    }
}

// ── top-N ─────────────────────────────────────────────────────────────────────────────────────

fn check_count(n: i64) -> Result<i64, ApiError> {
    if !(10..=1000).contains(&n) {
        return Err(ApiError::BadRequest(format!("Invalid count: {n}")));
    }
    Ok(n)
}

/// `GET /api/topholders/CHAIN/CONTRACT/SYM/N` — `[["acct","amount"],…]` sorted desc.
pub async fn topholders(
    State(st): State<AppState>,
    Path((chain, contract, symbol, count)): Path<(String, String, String, i64)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let n = check_count(count)?;
    let net_decimals = st.network(&chain)?.decimals;
    let db = st.db_for(&chain)?;
    // Sort by the always-present numeric `amount` (f64) — exact for ordering; `amount_str` (if
    // present) provides the precise display value.
    let docs = st
        .find(
            &db,
            "accounts",
            doc! { "code": &contract, "symbol": &symbol },
            Some(doc! { "amount": -1 }),
            Some(doc! { "scope": 1, "amount": 1, "amount_str": 1, "decimals": 1 }),
            Some(n),
        )
        .await?;
    let rows: Vec<Value> = docs
        .iter()
        .map(|d| {
            let scope = d.get_str("scope").unwrap_or("");
            let amount = balance_json(d, net_decimals);
            let amt = amount.get("amount").and_then(Value::as_str).unwrap_or("0");
            json!([scope, amt])
        })
        .collect();
    Ok(respond::json(Value::Array(rows), pretty.on()))
}

/// `GET /api/topram/CHAIN/N` — `[["acct",bytes],…]` sorted by `ram_bytes` desc.
pub async fn topram(
    State(st): State<AppState>,
    Path((chain, count)): Path<(String, i64)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let n = check_count(count)?;
    let db = st.db_for(&chain)?;
    let docs = st
        .find(
            &db,
            "eosio-userres",
            doc! {},
            Some(doc! { "ram_bytes": -1 }),
            Some(doc! { "owner": 1, "@scope": 1, "ram_bytes": 1 }),
            Some(n),
        )
        .await?;
    let rows: Vec<Value> = docs
        .iter()
        .map(|d| {
            let owner = d
                .get_str("owner")
                .ok()
                .or_else(|| d.get_str("@scope").ok())
                .unwrap_or("");
            let ram = bson_i64(d.get("ram_bytes")).unwrap_or(0);
            json!([owner, ram])
        })
        .collect();
    Ok(respond::json(Value::Array(rows), pretty.on()))
}

/// `GET /api/topstake/CHAIN/N` — `[["acct",cpu,net],…]` sorted by cpu+net (integer units) desc.
pub async fn topstake(
    State(st): State<AppState>,
    Path((chain, count)): Path<(String, i64)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let n = check_count(count)?;
    let db = st.db_for(&chain)?;
    // Sort on the loader-emitted numeric `stake` (net+cpu base units), which is indexed
    // (`eosio-userres {stake:-1}`). The old approach sorted on a computed `$split` of the asset
    // strings, which no index can serve → a full in-memory sort of every userres row (~33s at WAX
    // scale). The cpu/net legs in the response are still parsed from the asset strings below.
    let docs = st
        .find(
            &db,
            "eosio-userres",
            doc! {},
            Some(doc! { "stake": -1 }),
            Some(doc! { "owner": 1, "@scope": 1, "cpu_weight": 1, "net_weight": 1 }),
            Some(n),
        )
        .await?;
    let rows: Vec<Value> = docs
        .iter()
        .map(|d| {
            let owner = d
                .get_str("owner")
                .ok()
                .or_else(|| d.get_str("@scope").ok())
                .unwrap_or("");
            let cpu = d
                .get_str("cpu_weight")
                .ok()
                .and_then(asset::units)
                .unwrap_or(0);
            let net = d
                .get_str("net_weight")
                .ok()
                .and_then(asset::units)
                .unwrap_or(0);
            json!([owner, cpu as i64, net as i64])
        })
        .collect();
    Ok(respond::json(Value::Array(rows), pretty.on()))
}

// ── REX ─────────────────────────────────────────────────────────────────────────────────────────

/// Extract `(unix_seconds, rex_units)` maturity pairs from a `rex_maturities` BSON array, tolerating
/// `{first,second}` / `{key,value}` shapes and string-or-number timestamps.
fn parse_maturities(d: &Document) -> Vec<(i64, i128)> {
    let Some(Bson::Array(arr)) = d.get("rex_maturities") else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|el| {
            let m = el.as_document()?;
            let time = m
                .get("first")
                .or_else(|| m.get("key"))
                .or_else(|| m.get("0"));
            let units = m
                .get("second")
                .or_else(|| m.get("value"))
                .or_else(|| m.get("1"));
            let secs = match time? {
                Bson::String(s) => crate::timeutil::parse_utc(s)?,
                Bson::Int32(i) => *i as i64,
                Bson::Int64(i) => *i,
                Bson::Double(f) => *f as i64,
                _ => return None,
            };
            Some((secs, bson_i64(units).unwrap_or(0) as i128))
        })
        .collect()
}

async fn load_rex_input(
    st: &AppState,
    db: &mongodb::Database,
    account: &str,
) -> Result<RexInput, ApiError> {
    let rexbal = st
        .find_one(db, "eosio-rexbal", doc! { "owner": account })
        .await?;
    let rexfund = st
        .find_one(db, "eosio-rexfund", doc! { "owner": account })
        .await?;
    let rexpool = st.find_one(db, "eosio-rexpool", doc! {}).await?;

    let mut input = RexInput::default();
    if let Some(b) = &rexbal {
        // matured_rex is an int64 (abieos may render it as a numeric string) — not an asset.
        input.matured_rex = bson_i64(b.get("matured_rex")).unwrap_or(0) as i128;
        input.maturities = parse_maturities(b);
    }
    if let Some(f) = &rexfund {
        input.fund = f.get_str("balance").ok().and_then(asset::parse);
    }
    if let Some(p) = &rexpool {
        if let (Some(lend), Some(rx)) = (
            p.get_str("total_lendable").ok().and_then(asset::parse),
            p.get_str("total_rex").ok().and_then(asset::parse),
        ) {
            input.pool = Some(RexPool {
                total_lendable: lend,
                total_rex: rx,
            });
        }
    }
    Ok(input)
}

async fn rex_value(st: &AppState, chain: &str, account: &str) -> Result<Value, ApiError> {
    let net = st.network(chain)?.clone();
    let db = st.db_for(chain)?;
    let input = load_rex_input(st, &db, account).await?;
    let out = rex::compute(
        &input,
        crate::timeutil::now_secs(),
        &net.systoken,
        net.decimals,
    );
    Ok(json!({
        "fund": out.fund,
        "matured": out.matured,
        "maturing": out.maturing,
        "savings": out.savings,
    }))
}

/// `GET /api/rexbalance/CHAIN/ACCT`. cc32d9 omits `rex` on rex-disabled chains.
pub async fn rexbalance(
    State(st): State<AppState>,
    Path((chain, account)): Path<(String, String)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let net = st.network(&chain)?.clone();
    let chain_block = st.chain_block(&chain, true).await?;
    let mut out = json!({ "account_name": account, "chain": chain_block });
    if net.rex_enabled {
        out["rex"] = rex_value(&st, &chain, &account).await?;
    }
    Ok(respond::json(out, pretty.on()))
}

/// `GET /api/rexraw/CHAIN/ACCT` — raw rexbal/rexfund rows for client-side computation.
pub async fn rexraw(
    State(st): State<AppState>,
    Path((chain, account)): Path<(String, String)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    // cc32d9 returns a plain-text notice on rex-disabled chains.
    if !st.network(&chain)?.rex_enabled {
        return Ok(respond::text(format!("REX is not enabled on {chain}")));
    }
    let db = st.db_for(&chain)?;
    let rexbal = st
        .find_one(&db, "eosio-rexbal", doc! { "owner": &account })
        .await?;
    let rexfund = st
        .find_one(&db, "eosio-rexfund", doc! { "owner": &account })
        .await?;
    let rexpool = st.find_one(&db, "eosio-rexpool", doc! {}).await?;
    let strip = |d: Option<Document>| -> Value { d.map(doc_to_clean_json).unwrap_or(Value::Null) };
    let out = json!({
        "account_name": account,
        "rexbal": strip(rexbal),
        "rexfund": strip(rexfund),
        "rexpool": strip(rexpool),
    });
    Ok(respond::json(out, pretty.on()))
}

// ── account / accinfo ───────────────────────────────────────────────────────────────────────────

/// Map a `permissions` doc to the cc32d9 `permissions[]` entry shape.
fn permission_json(d: &Document) -> Value {
    let ra = d.get_document("required_auth").ok();
    let threshold = ra.and_then(|r| bson_i64(r.get("threshold"))).unwrap_or(1);

    let keys = ra
        .and_then(|r| r.get_array("keys").ok())
        .map(|arr| {
            arr.iter()
                .filter_map(|k| {
                    let k = k.as_document()?;
                    // Prefer explicit dual forms; fall back to whatever single form is stored.
                    let legacy = k
                        .get_str("pubkey")
                        .or_else(|_| k.get_str("key"))
                        .unwrap_or("");
                    let modern = k
                        .get_str("public_key")
                        .or_else(|_| k.get_str("key_pub"))
                        .or_else(|_| k.get_str("key"))
                        .unwrap_or("");
                    let weight = bson_i64(k.get("weight")).unwrap_or(1);
                    Some(json!({ "pubkey": legacy, "public_key": modern, "weight": weight }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let accounts = ra
        .and_then(|r| r.get_array("accounts").ok())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let a = a.as_document()?;
                    let perm = a.get_document("permission").ok();
                    let actor = perm.and_then(|p| p.get_str("actor").ok()).unwrap_or("");
                    let permission = perm
                        .and_then(|p| p.get_str("permission").ok())
                        .unwrap_or("");
                    let weight = bson_i64(a.get("weight")).unwrap_or(1);
                    Some(json!({ "actor": actor, "permission": permission, "weight": weight }))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    json!({
        "perm": d.get_str("perm_name").unwrap_or(""),
        "threshold": threshold,
        "auth": { "keys": keys, "accounts": accounts },
    })
}

/// linkauth entries from a permission doc's `linked_actions`.
fn linkauth_json(d: &Document) -> Vec<Value> {
    let perm = d.get_str("perm_name").unwrap_or("");
    d.get_array("linked_actions")
        .ok()
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let a = a.as_document()?;
                    Some(json!({
                        "code": a.get_str("account").unwrap_or(""),
                        "type": a.get_str("action").unwrap_or(""),
                        "requirement": perm,
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One delegated-bandwidth row → `{del_to|del_from, cpu_weight, net_weight}` (integer units).
fn delband_json(d: &Document, peer_field: &str, peer_key: &str) -> Value {
    let peer = d.get_str(peer_field).unwrap_or("");
    let cpu = d
        .get_str("cpu_weight")
        .ok()
        .and_then(asset::units)
        .unwrap_or(0) as i64;
    let net = d
        .get_str("net_weight")
        .ok()
        .and_then(asset::units)
        .unwrap_or(0) as i64;
    json!({ peer_key: peer, "cpu_weight": cpu, "net_weight": net })
}

/// Assemble the account-info bundle (everything except balances). Matches cc32d9: `resources` is
/// `null` when the account has no resource row, `code: {code_hash}` appears only for accounts with a
/// contract, and `rex` is included only on rex-enabled chains.
async fn accinfo_value(st: &AppState, chain: &str, account: &str) -> Result<Value, ApiError> {
    let net = st.network(chain)?.clone();
    let db = st.db_for(chain)?;

    // Run the independent reads concurrently.
    let (userres, perms, deleg_to, deleg_from, codehash, chain_block) = tokio::join!(
        // userres is scoped *by account* in Hyperion's schema (@scope == owner), and @scope always
        // carries the loader's index. Query by @scope, NOT the unindexed `owner` field — at WAX scale
        // an `owner` match is a 21.75M-doc collection scan (~6s/req), which serializes accinfo under load.
        st.find_one(&db, "eosio-userres", doc! { "@scope": account }),
        st.find(
            &db,
            "permissions",
            doc! { "account": account },
            Some(doc! { "perm_name": 1 }),
            None,
            None
        ),
        st.find(
            &db,
            "eosio-delband",
            doc! { "from": account },
            None,
            None,
            None
        ),
        st.find(
            &db,
            "eosio-delband",
            doc! { "to": account },
            None,
            None,
            None
        ),
        st.find_one(&db, "account_codehash", doc! { "account": account }),
        st.chain_block(chain, true),
    );
    let (userres, perms, deleg_to, deleg_from, codehash, chain_block) = (
        userres?,
        perms?,
        deleg_to?,
        deleg_from?,
        codehash?,
        chain_block?,
    );

    // resources: null when no row (cc32d9 returns null, not zeroed object)
    let resources = match &userres {
        Some(u) => json!({
            "net_weight": u.get_str("net_weight").ok().and_then(asset::units).unwrap_or(0) as i64,
            "cpu_weight": u.get_str("cpu_weight").ok().and_then(asset::units).unwrap_or(0) as i64,
            "ram_bytes": bson_i64(u.get("ram_bytes")).unwrap_or(0),
        }),
        None => Value::Null,
    };

    // permissions + linkauth
    let permissions: Vec<Value> = perms.iter().map(permission_json).collect();
    let linkauth: Vec<Value> = perms.iter().flat_map(linkauth_json).collect();

    let delegated_to: Vec<Value> = deleg_to
        .iter()
        .map(|d| delband_json(d, "to", "del_to"))
        .collect();
    let delegated_from: Vec<Value> = deleg_from
        .iter()
        .map(|d| delband_json(d, "from", "del_from"))
        .collect();

    let mut out = json!({
        "account_name": account,
        "chain": chain_block,
        "resources": resources,
        "permissions": permissions,
        "delegated_to": delegated_to,
        "delegated_from": delegated_from,
        "linkauth": linkauth,
    });

    // code: {code_hash} only for accounts that have a contract.
    if let Some(c) = codehash.as_ref().and_then(|d| d.get_str("code_hash").ok()) {
        out["code"] = json!({ "code_hash": c });
    }
    // rex only on rex-enabled chains (cc32d9 omits it otherwise).
    if net.rex_enabled {
        out["rex"] = rex_value(st, chain, account).await?;
    }

    Ok(out)
}

/// `GET /api/accinfo/CHAIN/ACCT`.
pub async fn accinfo(
    State(st): State<AppState>,
    Path((chain, account)): Path<(String, String)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let v = accinfo_value(&st, &chain, &account).await?;
    Ok(respond::json(v, pretty.on()))
}

/// `GET /api/account/CHAIN/ACCT` — accinfo + balances.
pub async fn account(
    State(st): State<AppState>,
    Path((chain, account)): Path<(String, String)>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let (info, balances) = tokio::join!(
        accinfo_value(&st, &chain, &account),
        load_balances(&st, &chain, &account),
    );
    let mut info = info?;
    info["balances"] = Value::Array(balances?);
    Ok(respond::json(info, pretty.on()))
}

// ── key / codehash (multi-chain) ────────────────────────────────────────────────────────────────

/// `GET /api/key/PUBKEY` — accounts using this key across all networks (max 100 per network).
pub async fn key(
    State(st): State<AppState>,
    Path(pubkey): Path<String>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let key = keyfmt::normalize(&pubkey)
        .ok_or_else(|| ApiError::BadRequest("Invalid key".to_string()))?;

    let futs = st.networks.iter().map(|n| {
        let st = st.clone();
        let name = n.name.clone();
        let key = key.clone();
        async move { (name.clone(), key_for_network(&st, &name, &key).await) }
    });
    let results = join_all(futs).await;

    let mut out = serde_json::Map::new();
    for (name, res) in results {
        if let Some(entry) = res? {
            out.insert(name, entry);
        }
    }
    Ok(respond::json(Value::Object(out), pretty.on()))
}

/// For one network: find accounts whose permission keys match, hydrate their permissions. Returns
/// `None` if the key matches nothing on that chain.
async fn key_for_network(st: &AppState, chain: &str, key: &str) -> Result<Option<Value>, ApiError> {
    let db = st.db_for(chain)?;
    // Match either stored form (legacy EOS… or modern PUB_…).
    let filter = doc! { "$or": [ { "key": key }, { "key_pub": key } ] };
    let hits = st
        .find(
            &db,
            "pub_keys",
            filter,
            None,
            Some(doc! { "account": 1 }),
            Some(100),
        )
        .await?;
    if hits.is_empty() {
        return Ok(None);
    }
    let mut accounts: Vec<String> = hits
        .iter()
        .filter_map(|d| d.get_str("account").ok().map(str::to_string))
        .collect();
    accounts.sort();
    accounts.dedup();

    let mut acct_map = serde_json::Map::new();
    for acct in &accounts {
        let perms = st
            .find(
                &db,
                "permissions",
                doc! { "account": acct },
                Some(doc! { "perm_name": 1 }),
                None,
                None,
            )
            .await?;
        let arr: Vec<Value> = perms.iter().map(permission_json).collect();
        acct_map.insert(acct.clone(), Value::Array(arr));
    }
    let chain_block = st.chain_block(chain, true).await?;
    Ok(Some(json!({ "accounts": acct_map, "chain": chain_block })))
}

/// `GET /api/codehash/SHA256` — accounts with this contract code hash across all networks.
pub async fn codehash(
    State(st): State<AppState>,
    Path(hash): Path<String>,
    pretty: PrettyQ,
) -> Result<Response, ApiError> {
    let hash = hash.trim().to_lowercase();
    if hash.len() != 64 || hex::decode(&hash).is_err() {
        return Err(ApiError::BadRequest("Invalid code hash".to_string()));
    }
    let futs = st.networks.iter().map(|n| {
        let st = st.clone();
        let name = n.name.clone();
        let hash = hash.clone();
        async move { (name.clone(), codehash_for_network(&st, &name, &hash).await) }
    });
    let results = join_all(futs).await;
    let mut out = serde_json::Map::new();
    for (name, res) in results {
        let accounts = res?;
        if !accounts.is_empty() {
            // cc32d9 keys `accounts` by account name → {account_name, code_hash}, plus a chain block.
            let chain = st.chain_block(&name, true).await?;
            out.insert(
                name,
                json!({ "accounts": Value::Object(accounts), "chain": chain }),
            );
        }
    }
    Ok(respond::json(Value::Object(out), pretty.on()))
}

async fn codehash_for_network(
    st: &AppState,
    chain: &str,
    hash: &str,
) -> Result<serde_json::Map<String, Value>, ApiError> {
    let db = st.db_for(chain)?;
    let docs = st
        .find(
            &db,
            "account_codehash",
            doc! { "code_hash": hash },
            None,
            Some(doc! { "account": 1 }),
            Some(1000),
        )
        .await?;
    let mut map = serde_json::Map::new();
    for d in &docs {
        if let Ok(acct) = d.get_str("account") {
            map.insert(
                acct.to_string(),
                json!({ "account_name": acct, "code_hash": hash }),
            );
        }
    }
    Ok(map)
}

// ── helpers ───────────────────────────────────────────────────────────────────────────────────

/// Convert a Mongo doc to JSON, dropping Hyperion/snapshot internal fields (`_id`, `@`-prefixed).
fn doc_to_clean_json(d: Document) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in d {
        if k == "_id" || k.starts_with('@') {
            continue;
        }
        map.insert(k, bson_to_json(v));
    }
    Value::Object(map)
}

/// Minimal BSON→JSON for raw passthrough (rexraw). Numbers/strings/bools/arrays/docs; others stringify.
fn bson_to_json(b: Bson) -> Value {
    match b {
        Bson::Double(f) => json!(f),
        Bson::Int32(i) => json!(i),
        Bson::Int64(i) => json!(i),
        Bson::Boolean(b) => json!(b),
        Bson::String(s) => json!(s),
        Bson::Array(a) => Value::Array(a.into_iter().map(bson_to_json).collect()),
        Bson::Document(d) => doc_to_clean_json(d),
        Bson::Null => Value::Null,
        other => json!(bson_string(Some(&other)).unwrap_or_else(|| other.to_string())),
    }
}
