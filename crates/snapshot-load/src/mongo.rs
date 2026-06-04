//! High-throughput MongoDB sink — the parallel "saturate the sink" writer (mirrors es-load).
//!
//! Decode workers send typed `(collection, serde_json::Value)` docs over a crossbeam channel. A single
//! **bridge thread** owns a multi-thread tokio runtime (`run_sink`), accumulates per-collection batches
//! of `--mongo-batch` docs, and drives `--mongo-writers` concurrent `insert_many(ordered(false))`
//! futures over one pooled `Client` via `buffer_unordered`. Write concern is `w:1, journal:false`
//! (bulk-load speed). Indexes are built AFTER the load (mirroring the sync modules), skippable for
//! benchmarking.
//!
//! `proposals` are a special case: they require a `(proposer, proposal_name)` join against `approvals2`
//! carrier docs, so they are buffered in the bridge thread and merged + written at end-of-stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, Result};
use futures::stream::StreamExt;
use mongodb::bson::{doc, Bson, DateTime, Document};
use mongodb::error::{Error as MongoError, ErrorKind};
use mongodb::options::{Acknowledgment, ClientOptions, IndexOptions, WriteConcern};
use mongodb::{Client, IndexModel};

use crate::atomicassets::{
    COLL_AA_ASSETS, COLL_AA_COLLECTIONS, COLL_AA_CONFIG, COLL_AA_OFFERS, COLL_AA_SCHEMAS,
    COLL_AA_TEMPLATES,
};
use crate::atomicmarket::{
    COLL_AM_AUCTIONS, COLL_AM_BUYOFFERS, COLL_AM_CONFIG, COLL_AM_MARKETPLACES, COLL_AM_SALES,
    COLL_AM_TEMPLATE_BUYOFFERS,
};
use crate::map::{
    COLL_ACCOUNTS, COLL_CODEHASH, COLL_PERMISSIONS, COLL_PROPOSALS, COLL_PUB_KEYS, COLL_VOTERS,
};

/// Sink configuration assembled from CLI args.
pub struct MongoCfg {
    pub uri: String,
    pub db_name: String,
    /// Optional auth database, applied via the typed credential `source` (NOT string-appended to the
    /// URI). `None` leaves whatever the URI specifies (or the driver default).
    pub auth_source: Option<String>,
    pub writers: usize,
    pub batch: usize,
    pub pool: u32,
    pub drop: bool,
    pub no_index: bool,
    /// Build only the indexes the Light-API serving path needs (drops the `permissions` unique
    /// `{account,perm_name}` + `last_updated`/`linked_actions` and the `pub_keys` `account` indexes).
    /// At chain scale (WAX: 43M permissions, 32M pub_keys) the post-load index build dominates total
    /// time; the dropped indexes aren't on any read path.
    pub lean_index: bool,
    /// Non-dynamic collections to drop up front (when `drop`). Lets a permissions-only pass drop just
    /// `permissions`/`pub_keys` without wiping the `voters`/`accounts`/`proposals` from a prior pass.
    pub special_drops: Vec<&'static str>,
}

/// Final per-phase metrics, reported by the caller.
#[derive(Debug, Default)]
pub struct MongoStats {
    pub docs: u64,
    pub batches: u64,
    pub errors: u64,
    pub write_secs: f64,
    pub index_secs: f64,
    pub per_coll: Vec<(String, u64)>,
}

/// Item on the typed sink channel: a BSON doc destined for `coll`. The expensive
/// `serde_json::Value -> bson::Document` encode is done by the parallel decode workers (not the
/// single accumulator), so the write side actually saturates.
pub type SinkItem = (&'static str, Document);

/// Run the Mongo sink on the current thread: build a tokio runtime, drain `rx`, write in parallel,
/// then build indexes. Blocks until done. Returns metrics.
pub fn run_sink(cfg: MongoCfg, rx: crossbeam_channel::Receiver<SinkItem>) -> Result<MongoStats> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads((cfg.writers + 2).max(2))
        .enable_all()
        .build()?;
    rt.block_on(async move { run_writer(cfg, rx).await })
}

async fn run_writer(
    cfg: MongoCfg,
    rx: crossbeam_channel::Receiver<SinkItem>,
) -> Result<MongoStats> {
    let mut opts = ClientOptions::parse(&cfg.uri).await?;
    opts.max_pool_size = Some(cfg.pool);
    opts.min_pool_size = Some(cfg.writers as u32);
    // Apply --mongo-auth-source via the typed credential `source` rather than string-appending
    // `?authSource=` to the URI (which mishandles URIs that already carry query params / options).
    // Only set it when the URI ALREADY carries a credential: `get_or_insert_with(Credential::default)`
    // would fabricate a username-less credential on a credential-less URI, which the driver rejects at
    // connect time. authSource without auth is meaningless anyway.
    if let Some(src) = &cfg.auth_source {
        if let Some(cred) = &mut opts.credential {
            cred.source = Some(src.clone());
        }
    }
    opts.write_concern = Some(
        WriteConcern::builder()
            .w(Acknowledgment::Nodes(1))
            .journal(false)
            .build(),
    );
    let client = Client::with_options(opts)?;
    let db = Arc::new(client.database(&cfg.db_name));

    // Optionally drop the special + any seen dynamic collections before load (idempotent re-runs).
    // We only know the special names up front; dynamic ones are dropped lazily on first batch below.
    if cfg.drop {
        for c in &cfg.special_drops {
            if let Err(e) = db.collection::<Document>(c).drop().await {
                eprintln!("[snapshot-load][mongo] drop {c} failed: {e}");
            }
        }
    }

    let docs = Arc::new(AtomicU64::new(0));
    let batches = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    // Bounded async channel of full batches; a dedicated std thread accumulates from the crossbeam
    // (sync) receiver and ships `(coll, Vec<Document>)` here. Proposals/approvals2 are buffered and
    // emitted at the very end (after the join). Dropped collections set is tracked for --mongo-drop.
    let (batch_tx, batch_rx) =
        tokio::sync::mpsc::channel::<(&'static str, Vec<Document>)>(cfg.writers * 2 + 4);

    let per_coll = Arc::new(std::sync::Mutex::new(HashMap::<&'static str, u64>::new()));
    let drop_dynamic = cfg.drop;
    let db_for_drop = db.clone();
    // tokio RwLock (NOT std, NOT a plain Mutex): once a dynamic collection has been dropped, every
    // subsequent batch only takes a *concurrent read* lock to confirm it (the common case — so the
    // parallel writers don't serialize on a global lock). Only the FIRST batch for a newly-seen
    // collection upgrades to the write lock, which is held ACROSS the async `drop().await` so a
    // concurrent first-batch for the same collection waits for the drop to complete before inserting
    // (else its inserts could land and then be deleted by the drop) — preserving the race fix.
    let dropped: Arc<tokio::sync::RwLock<std::collections::HashSet<&'static str>>> =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()));

    // Accumulator thread: groups docs per collection into batches, buffers proposals for the join.
    let acc_batch_tx = batch_tx.clone();
    let batch_size = cfg.batch;
    let per_coll_acc = per_coll.clone();
    let acc = std::thread::spawn(move || {
        let mut pending: HashMap<&'static str, Vec<Document>> = HashMap::new();
        // proposals join buffers, keyed by (proposer, proposal_name)
        let mut proposals: HashMap<(String, String), Document> = HashMap::new();
        let mut approvals: HashMap<(String, String), Document> = HashMap::new();

        for (coll, doc) in rx.iter() {
            if coll == COLL_PROPOSALS {
                buffer_proposal(doc, &mut proposals, &mut approvals);
                continue;
            }
            *per_coll_acc.lock().unwrap().entry(coll).or_insert(0) += 1;
            let buf = pending.entry(coll).or_default();
            buf.push(doc);
            if buf.len() >= batch_size {
                let full = std::mem::take(buf);
                if acc_batch_tx.blocking_send((coll, full)).is_err() {
                    return;
                }
            }
        }
        // flush partial batches
        for (coll, buf) in pending {
            if !buf.is_empty() && acc_batch_tx.blocking_send((coll, buf)).is_err() {
                return;
            }
        }
        // merge + flush proposals
        let merged = merge_proposals(proposals, approvals);
        if !merged.is_empty() {
            *per_coll_acc
                .lock()
                .unwrap()
                .entry(COLL_PROPOSALS)
                .or_insert(0) += merged.len() as u64;
            for chunk in merged.chunks(batch_size.max(1)) {
                if acc_batch_tx
                    .blocking_send((COLL_PROPOSALS, chunk.to_vec()))
                    .is_err()
                {
                    return;
                }
            }
        }
    });
    drop(batch_tx); // accumulator holds the only remaining sender

    // Writer side: buffer_unordered of insert_many futures over the pooled client.
    let write_t0 = Instant::now();
    let stream = futures::stream::unfold(batch_rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    stream
        .map(|(coll, docs_vec)| {
            let db = db.clone();
            let docs = docs.clone();
            let batches = batches.clone();
            let errors = errors.clone();
            let dropped = dropped.clone();
            let db_for_drop = db_for_drop.clone();
            async move {
                if drop_dynamic && !is_named_collection(coll) {
                    // Fast path: already dropped → a concurrent READ lock only (no serialization).
                    if !dropped.read().await.contains(coll) {
                        // First sight: upgrade to the WRITE lock and hold it across the drop await, so
                        // the first batch for `coll` performs the drop while later batches block here
                        // until it returns — guaranteeing no insert races ahead of (and is then wiped
                        // by) the collection drop. Re-check under the write lock to settle the race
                        // between a concurrent first-batch's read and this write.
                        let mut w = dropped.write().await;
                        if w.insert(coll) {
                            if let Err(e) = db_for_drop.collection::<Document>(coll).drop().await {
                                eprintln!("[snapshot-load][mongo] drop {coll} failed: {e}");
                            }
                        }
                    }
                }
                let attempted = docs_vec.len();
                match db
                    .collection::<Document>(coll)
                    .insert_many(docs_vec)
                    .ordered(false)
                    .await
                {
                    Ok(r) => {
                        docs.fetch_add(r.inserted_ids.len() as u64, Relaxed);
                    }
                    Err(e) => {
                        // Count ONLY docs that actually landed. For an unordered insert the driver
                        // surfaces per-document failures in `write_errors`, so inserted = attempted -
                        // failures. For any other error (connection drop, etc.) we cannot know, so add
                        // 0 rather than inflating the counter with the full attempted batch.
                        let inserted = inserted_from_err(&e, attempted);
                        docs.fetch_add(inserted, Relaxed);
                        errors.fetch_add(1, Relaxed);
                        eprintln!("[snapshot-load][mongo] insert_many error in {coll}: {e}");
                    }
                }
                batches.fetch_add(1, Relaxed);
            }
        })
        .buffer_unordered(cfg.writers.max(1))
        .collect::<Vec<()>>()
        .await;

    let write_secs = write_t0.elapsed().as_secs_f64();
    acc.join()
        .map_err(|_| anyhow!("accumulator thread panicked"))?;

    let coll_counts: Vec<(String, u64)> = per_coll
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();

    let mut stats = MongoStats {
        docs: docs.load(Relaxed),
        batches: batches.load(Relaxed),
        errors: errors.load(Relaxed),
        write_secs,
        index_secs: 0.0,
        per_coll: coll_counts.clone(),
    };

    // ── indexes (AFTER load) ─────────────────────────────────────────────────────────────────────
    if !cfg.no_index {
        let t = Instant::now();
        for (coll, count) in &coll_counts {
            if *count == 0 {
                continue;
            }
            build_indexes(&db, coll, cfg.lean_index).await?;
        }
        stats.index_secs = t.elapsed().as_secs_f64();
    }

    Ok(stats)
}

/// Best-effort count of docs that actually landed when an unordered `insert_many` errored. The
/// driver reports per-document failures in `write_errors`, so `attempted - write_errors` is the
/// number inserted server-side. Any non-`InsertMany` error (connection drop, write-concern failure
/// with no per-doc detail, …) is treated as 0 inserted so the counter is never inflated.
fn inserted_from_err(e: &MongoError, attempted: usize) -> u64 {
    if let ErrorKind::InsertMany(ime) = e.kind.as_ref() {
        let failed = ime.write_errors.as_ref().map_or(0, Vec::len);
        return attempted.saturating_sub(failed) as u64;
    }
    0
}

/// Buffer a proposals-channel BSON doc into either the proposal map or the approvals map (carrier
/// docs). The doc was already BSON-encoded by the worker; we just key it and (for proposals) fix up
/// the expiration Date.
fn buffer_proposal(
    mut doc: Document,
    proposals: &mut HashMap<(String, String), Document>,
    approvals: &mut HashMap<(String, String), Document>,
) {
    if doc.get_bool("__approval").unwrap_or(false) {
        let proposer = doc.get_str("__proposer").unwrap_or("").to_string();
        let name = doc.get_str("__proposal_name").unwrap_or("").to_string();
        approvals.insert((proposer, name), doc);
    } else {
        let proposer = doc.get_str("proposer").unwrap_or("").to_string();
        let name = doc.get_str("proposal_name").unwrap_or("").to_string();
        fixup_expiration(&mut doc);
        proposals.insert((proposer, name), doc);
    }
}

/// `IProposal.expiration` is a `Date`. abieos emits the trx expiration as a naive ISO string and the
/// mapper wraps it as `{ "$date": "..." }`; bson-2's serde `to_document` does NOT auto-interpret that
/// extended-JSON tag, so convert the field to a real BSON `DateTime` here (UTC; abieos has no offset).
fn fixup_expiration(doc: &mut Document) {
    let s = match doc.get("expiration") {
        Some(Bson::Document(d)) => d.get_str("$date").ok().map(str::to_string),
        Some(Bson::String(s)) => Some(s.clone()),
        _ => None,
    };
    if let Some(s) = s {
        // abieos renders "YYYY-MM-DDTHH:MM:SS.sss" (no zone). Append Z to parse as UTC — but only if
        // the string does not ALREADY carry a zone. A zone is a trailing 'Z' or a +HH:MM / -HH:MM
        // offset AFTER the date's 'T'. (The earlier `contains('+')` check missed negative offsets,
        // and a plain date contains '-' separators, so scan only the time part for the offset sign.)
        let has_zone = s.ends_with('Z')
            || s.rsplit_once('T')
                .is_some_and(|(_, time)| time.contains('+') || time.contains('-'));
        let rfc = if has_zone { s.clone() } else { format!("{s}Z") };
        if let Ok(dt) = DateTime::parse_rfc3339_str(&rfc) {
            doc.insert("expiration", Bson::DateTime(dt));
        }
    }
}

/// Merge approvals carrier docs into their proposals by `(proposer, proposal_name)`. A proposal with
/// no matching approvals2 row gets no version/requested/provided fields (mirrors sync-proposals.ts).
fn merge_proposals(
    proposals: HashMap<(String, String), Document>,
    approvals: HashMap<(String, String), Document>,
) -> Vec<Document> {
    let mut out = Vec::with_capacity(proposals.len());
    for (key, mut prop) in proposals {
        if let Some(ap) = approvals.get(&key) {
            if let Ok(v) = ap.get_i32("version") {
                prop.insert("version", v);
            } else if let Ok(v) = ap.get_i64("version") {
                prop.insert("version", v);
            }
            if let Ok(arr) = ap.get_array("requested_approvals") {
                prop.insert("requested_approvals", arr.clone());
            }
            if let Ok(arr) = ap.get_array("provided_approvals") {
                prop.insert("provided_approvals", arr.clone());
            }
        }
        out.push(prop);
    }
    out
}

/// Named collections — dropped up front via `special_drops` and given dedicated `build_indexes`
/// arms — so the per-batch lazy-drop path skips them (else a first batch would re-drop, racing the
/// up-front drop). Covers the eosio/Light-API collections plus every AtomicAssets/AtomicMarket
/// collection (by `atomicassets-`/`atomicmarket-` prefix).
fn is_named_collection(coll: &str) -> bool {
    matches!(
        coll,
        COLL_VOTERS
            | COLL_ACCOUNTS
            | COLL_PROPOSALS
            | COLL_PERMISSIONS
            | COLL_PUB_KEYS
            | COLL_CODEHASH
    ) || coll.starts_with("atomicassets-")
        || coll.starts_with("atomicmarket-")
}

/// Create the post-load indexes for a collection, mirroring the sync modules. Unknown (dynamic)
/// collections get the 5 `@`-field contract-state indexes.
async fn build_indexes(db: &mongodb::Database, coll: &str, lean: bool) -> Result<()> {
    let c = db.collection::<Document>(coll);
    let unique = || IndexOptions::builder().unique(true).build();
    let mk = |keys: Document| IndexModel::builder().keys(keys).build();
    let mk_u = |keys: Document| IndexModel::builder().keys(keys).options(unique()).build();

    // Lean mode keeps only the read-path indexes for the two huge collections — at WAX scale this
    // turns four foreground builds over 43M/32M docs into one or two.
    if lean {
        match coll {
            COLL_PERMISSIONS => {
                // {account} for reads + {block_num} for the live feed's "changed since N" poll.
                c.create_indexes(vec![mk(doc! { "account": 1 }), mk(doc! { "block_num": 1 })])
                    .await?;
                return Ok(());
            }
            COLL_PUB_KEYS => {
                c.create_indexes(vec![mk(doc! { "key": 1 }), mk(doc! { "key_pub": 1 })])
                    .await?;
                return Ok(());
            }
            _ => {}
        }
    }

    let models: Vec<IndexModel> = match coll {
        COLL_VOTERS => vec![
            mk_u(doc! { "voter": 1 }),
            mk(doc! { "producers": 1 }),
            mk(doc! { "is_proxy": 1 }),
        ],
        COLL_ACCOUNTS => vec![
            mk(doc! { "code": 1 }),
            mk(doc! { "scope": 1 }),
            mk(doc! { "symbol": 1 }),
            mk_u(doc! { "code": 1, "scope": 1, "symbol": 1 }),
            // Light-API /topholders: top balances of a (contract, symbol) by amount desc.
            mk(doc! { "code": 1, "symbol": 1, "amount": -1 }),
            // Live feed: "changed since block N" poll.
            mk(doc! { "block_num": 1 }),
        ],
        COLL_PERMISSIONS => vec![
            mk(doc! { "account": 1 }),
            mk_u(doc! { "account": 1, "perm_name": 1 }),
            mk(doc! { "last_updated": -1 }),
            mk(doc! { "linked_actions.account": 1, "linked_actions.action": 1 }),
            // Live feed: "changed since block N" poll.
            mk(doc! { "block_num": 1 }),
        ],
        COLL_PUB_KEYS => vec![
            mk(doc! { "key": 1 }),
            mk(doc! { "key_pub": 1 }),
            mk(doc! { "account": 1 }),
            mk(doc! { "account": 1, "perm": 1 }),
        ],
        COLL_CODEHASH => vec![mk_u(doc! { "account": 1 }), mk(doc! { "code_hash": 1 })],
        COLL_PROPOSALS => vec![
            mk(doc! { "proposal_name": 1 }),
            mk(doc! { "proposer": 1 }),
            mk(doc! { "expiration": -1 }),
            mk(doc! { "provided_approvals.actor": 1 }),
            mk(doc! { "requested_approvals.actor": 1 }),
        ],
        // Light-API read-path indexes on the eosio system tables (dynamic collections, matched by name).
        // userres carries /topram (ram_bytes) + /topstake (numeric `stake`, emitted by map_userres);
        // delband carries accinfo's delegated_to (`from`) + delegated_from (`to`) joins.
        "eosio-userres" => vec![
            mk(doc! { "@pk": -1 }),
            mk(doc! { "@scope": 1 }),
            mk(doc! { "@block_num": -1 }),
            mk(doc! { "@block_time": -1 }),
            mk(doc! { "@payer": 1 }),
            mk(doc! { "ram_bytes": -1 }),
            mk(doc! { "stake": -1 }),
        ],
        "eosio-delband" => vec![
            mk(doc! { "@pk": -1 }),
            mk(doc! { "@scope": 1 }),
            mk(doc! { "@block_num": -1 }),
            mk(doc! { "@block_time": -1 }),
            mk(doc! { "@payer": 1 }),
            mk(doc! { "to": 1 }),
            mk(doc! { "from": 1 }),
        ],
        // ── AtomicAssets state (the `atomicassets`/`atomic` preset) ──────────────────────────────
        // The faceted query surface: filter by owner/collection/schema/template + decoded `data.*`
        // attributes (a single wildcard index serves arbitrary `data:key=value` filters, mirroring
        // eosio-contract-api's combined-data GIN). `block_num` powers the live "changed since" feed.
        COLL_AA_ASSETS => vec![
            mk_u(doc! { "asset_id": 1 }),
            mk(doc! { "owner": 1 }),
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "schema_name": 1 }),
            mk(doc! { "template_id": 1 }),
            mk(doc! { "collection_name": 1, "template_id": 1 }),
            mk(doc! { "data.$**": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AA_TEMPLATES => vec![
            mk_u(doc! { "template_id": 1 }),
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "schema_name": 1 }),
            mk(doc! { "data.$**": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AA_SCHEMAS => vec![
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "schema_name": 1 }),
            mk_u(doc! { "collection_name": 1, "schema_name": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AA_COLLECTIONS => vec![
            mk_u(doc! { "collection_name": 1 }),
            mk(doc! { "author": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AA_OFFERS => vec![
            mk_u(doc! { "offer_id": 1 }),
            mk(doc! { "sender": 1 }),
            mk(doc! { "recipient": 1 }),
            mk(doc! { "sender_asset_ids": 1 }),
            mk(doc! { "recipient_asset_ids": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AA_CONFIG => vec![mk_u(doc! { "contract": 1 })],
        // ── AtomicMarket state (the `atomicmarket`/`atomic` preset) ──────────────────────────────
        COLL_AM_SALES => vec![
            mk_u(doc! { "sale_id": 1 }),
            mk(doc! { "seller": 1 }),
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "state": 1 }),
            mk(doc! { "asset_ids": 1 }),
            mk(doc! { "offer_id": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AM_AUCTIONS => vec![
            mk_u(doc! { "auction_id": 1 }),
            mk(doc! { "seller": 1 }),
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "state": 1 }),
            mk(doc! { "end_time": 1 }),
            mk(doc! { "asset_ids": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AM_BUYOFFERS => vec![
            mk_u(doc! { "buyoffer_id": 1 }),
            mk(doc! { "buyer": 1 }),
            mk(doc! { "seller": 1 }),
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "asset_ids": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AM_TEMPLATE_BUYOFFERS => vec![
            mk_u(doc! { "buyoffer_id": 1 }),
            mk(doc! { "buyer": 1 }),
            mk(doc! { "template_id": 1 }),
            mk(doc! { "collection_name": 1 }),
            mk(doc! { "block_num": 1 }),
        ],
        COLL_AM_MARKETPLACES => vec![
            mk_u(doc! { "marketplace_name": 1 }),
            mk(doc! { "creator": 1 }),
        ],
        COLL_AM_CONFIG => vec![mk_u(doc! { "market_contract": 1 })],
        _ => vec![
            mk(doc! { "@pk": -1 }),
            mk(doc! { "@scope": 1 }),
            mk(doc! { "@block_num": -1 }),
            mk(doc! { "@block_time": -1 }),
            mk(doc! { "@payer": 1 }),
        ],
    };
    c.create_indexes(models).await?;
    Ok(())
}
