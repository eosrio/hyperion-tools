//! High-throughput MongoDB sink — the parallel "saturate the sink" writer (mirrors es-load).
//!
//! Decode workers send typed `(collection, serde_json::Value)` docs over a crossbeam channel. A single
//! **bridge thread** owns a current-thread tokio runtime, accumulates per-collection batches of
//! `--mongo-batch` docs, and drives `--mongo-writers` concurrent `insert_many(ordered(false))` futures
//! over one pooled `Client` via `buffer_unordered`. Write concern is `w:1, journal:false` (bulk-load
//! speed). Indexes are built AFTER the load (mirroring the sync modules), skippable for benchmarking.
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
use mongodb::options::{Acknowledgment, ClientOptions, IndexOptions, WriteConcern};
use mongodb::{Client, IndexModel};

use crate::map::{COLL_ACCOUNTS, COLL_PROPOSALS, COLL_VOTERS};

/// Sink configuration assembled from CLI args.
pub struct MongoCfg {
    pub uri: String,
    pub db_name: String,
    pub writers: usize,
    pub batch: usize,
    pub pool: u32,
    pub drop: bool,
    pub no_index: bool,
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

async fn run_writer(cfg: MongoCfg, rx: crossbeam_channel::Receiver<SinkItem>) -> Result<MongoStats> {
    let mut opts = ClientOptions::parse(&cfg.uri).await?;
    opts.max_pool_size = Some(cfg.pool);
    opts.min_pool_size = Some(cfg.writers as u32);
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
        for c in [COLL_VOTERS, COLL_ACCOUNTS, COLL_PROPOSALS] {
            db.collection::<Document>(c).drop().await.ok();
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
    let dropped: Arc<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

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
            *per_coll_acc.lock().unwrap().entry(COLL_PROPOSALS).or_insert(0) += merged.len() as u64;
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
                if drop_dynamic && coll != COLL_VOTERS && coll != COLL_ACCOUNTS && coll != COLL_PROPOSALS
                {
                    let need = { dropped.lock().unwrap().insert(coll) };
                    if need {
                        db_for_drop.collection::<Document>(coll).drop().await.ok();
                    }
                }
                let n = docs_vec.len() as u64;
                match db
                    .collection::<Document>(coll)
                    .insert_many(docs_vec)
                    .ordered(false)
                    .await
                {
                    Ok(r) => {
                        docs.fetch_add(r.inserted_ids.len() as u64, Relaxed);
                    }
                    Err(_) => {
                        // Unordered insert: count attempted; partial successes still landed server-side.
                        docs.fetch_add(n, Relaxed);
                        errors.fetch_add(1, Relaxed);
                    }
                }
                batches.fetch_add(1, Relaxed);
            }
        })
        .buffer_unordered(cfg.writers.max(1))
        .collect::<Vec<()>>()
        .await;

    let write_secs = write_t0.elapsed().as_secs_f64();
    acc.join().map_err(|_| anyhow!("accumulator thread panicked"))?;

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
            build_indexes(&db, coll).await?;
        }
        stats.index_secs = t.elapsed().as_secs_f64();
    }

    Ok(stats)
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
        // abieos renders "YYYY-MM-DDTHH:MM:SS.sss" (no zone). Append Z to parse as UTC.
        let rfc = if s.ends_with('Z') || s.contains('+') {
            s.clone()
        } else {
            format!("{s}Z")
        };
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

/// Create the post-load indexes for a collection, mirroring the sync modules. Unknown (dynamic)
/// collections get the 5 `@`-field contract-state indexes.
async fn build_indexes(db: &mongodb::Database, coll: &str) -> Result<()> {
    let c = db.collection::<Document>(coll);
    let unique = || IndexOptions::builder().unique(true).build();
    let mk = |keys: Document| IndexModel::builder().keys(keys).build();
    let mk_u = |keys: Document| {
        IndexModel::builder()
            .keys(keys)
            .options(unique())
            .build()
    };

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
        ],
        COLL_PROPOSALS => vec![
            mk(doc! { "proposal_name": 1 }),
            mk(doc! { "proposer": 1 }),
            mk(doc! { "expiration": -1 }),
            mk(doc! { "provided_approvals.actor": 1 }),
            mk(doc! { "requested_approvals.actor": 1 }),
        ],
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
