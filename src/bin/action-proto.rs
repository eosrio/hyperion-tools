//! action-proto — PROTOTYPE: a direct-from-disk Hyperion *action* indexer.
//!
//! The sibling of `delta-proto`, pointed at `trace_history.{log,index}` instead of
//! `chain_state_history.*`. It reads the append-only trace log off disk in parallel (no nodeos,
//! no SHiP websocket), decodes each block's `transaction_trace[]`, walks the nested
//! `action_traces`, decodes every action's `act.data` against the contract ABI active *at that
//! block*, groups notification receipts à la Hyperion, and emits `<chain>-action-v1`-shaped
//! NDJSON. It is the next slice after deltas in the "delete the ship-0 bottleneck" direction.
//!
//! TWO-LEVEL DECODE:
//!   L1 (trace skeleton): one `AbiHandle::bin_to_json_into("transaction_trace[]", payload, buf)`
//!      against the embedded SHiP protocol ABI (rust-backend handles the top-level `[]`, the
//!      action_trace v0/v1 variant, optionals, and the recursive failed_dtrx_trace). This yields
//!      `act.data` as a raw hex string (the SHiP ABI types it as `bytes`).
//!   L2 (act.data): per action, look up the contract `AbiHandle` active at the block in the
//!      per-worker `Registry` (reused verbatim from delta-proto), resolve `type_for_action`, and
//!      `bin_to_json_into`. On failure retry at block-1 (same-block setabi boundary); if still
//!      undecodable, preserve the raw hex (`ds_error`) — every action still emits a doc.
//!
//! PHASE A scope: emits every field EXCEPT the block-header fields `@timestamp` and `producer`
//! (those live only in the signed_block, not in any trace — Phase B reads the block log), and
//! defers the computed `@transfer`/`@newaccount`/... handlers to a later slice. Uses a static
//! chunk split (delta-proto style); work-stealing + checkpoint is a later productionization step.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::Sender;
use std::sync::{mpsc, Arc};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rs_abieos::{AbiHandle, Abieos};
use serde_json::{Map, Value};

use abi_scanner::disk::{decode_payload, is_ship_magic};

/// The state-history PROTOCOL ABI (transaction_trace / action_trace / action_receipt / ...),
/// captured once from a WAX SHiP handshake. nodeos emits it only over the websocket, so for
/// offline disk decoding we embed it. It is stable per nodeos protocol family (eosio::abi/1.1).
const SHIP_ABI_JSON: &str = include_str!("../../abis/ship.abi.json");

#[derive(Parser, Debug)]
#[command(about = "PROTOTYPE: decode action_traces directly from the trace_history log.")]
struct Args {
    /// nodeos state-history dir (trace_history.{log,index})
    #[arg(long)]
    from_disk: String,
    /// abi-index NDJSON produced by abi-scanner ({account, block, abi_hex, ...})
    #[arg(long)]
    abi_index: String,
    #[arg(long)]
    start: u32,
    #[arg(long)]
    end: u32,
    #[arg(long, default_value_t = 8)]
    threads: u32,
    /// output NDJSON (omit to measure pure decode throughput)
    #[arg(long)]
    out: Option<String>,
}

/// account (u64 name) -> versions sorted by the block the ABI took effect (valid_from).
type AbiIndex = HashMap<u64, Vec<(u32, String)>>;

fn load_abi_index(path: &str) -> Result<AbiIndex> {
    let names = Abieos::new();
    let f = BufReader::new(File::open(path).with_context(|| format!("open {path}"))?);
    let mut idx: AbiIndex = HashMap::new();
    let mut skipped = 0u64;
    for line in f.lines() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            skipped += 1;
            continue;
        };
        let (Some(acct), Some(block), Some(hex)) = (
            v.get("account").and_then(|x| x.as_str()),
            v.get("block").and_then(|x| x.as_u64()),
            v.get("abi_hex").and_then(|x| x.as_str()),
        ) else {
            continue;
        };
        if hex.is_empty() {
            continue;
        }
        let Ok(code) = names.string_to_name(acct) else {
            skipped += 1;
            continue;
        };
        idx.entry(code)
            .or_default()
            .push((block as u32, hex.to_string()));
    }
    for versions in idx.values_mut() {
        versions.sort_by_key(|(b, _)| *b);
    }
    if skipped > 0 {
        eprintln!("[action-proto] skipped {skipped} malformed ABI-index line(s)");
    }
    Ok(idx)
}

/// Per-worker cache of parsed contract ABIs, backed by the shared (immutable) version index.
/// (Identical to delta-proto's registry — the level-2 act.data decode is the same lookup.)
struct Registry<'a> {
    idx: &'a AbiIndex,
    handles: HashMap<(u64, u32), Option<AbiHandle>>,
}

impl<'a> Registry<'a> {
    fn new(idx: &'a AbiIndex) -> Self {
        Self {
            idx,
            handles: HashMap::new(),
        }
    }

    fn active(&mut self, code: u64, block: u32) -> Option<&mut AbiHandle> {
        let Registry { idx, handles } = self;
        let versions = idx.get(&code)?;
        let pos = versions.partition_point(|(vf, _)| *vf <= block);
        if pos == 0 {
            return None;
        }
        let valid_from = versions[pos - 1].0;
        handles
            .entry((code, valid_from))
            .or_insert_with(|| AbiHandle::from_hex(&versions[pos - 1].1).ok())
            .as_mut()
    }
}

#[derive(Default)]
struct Stats {
    blocks: AtomicU64,
    txs: AtomicU64,       // transaction_trace seen
    actions: AtomicU64,   // action_trace processed (except==null, has receipt)
    decoded: AtomicU64,   // act.data -> JSON ok
    raw: AtomicU64,       // act.data undecodable -> raw hex preserved (ds_error)
    no_abi: AtomicU64,    // (subset of raw) no ABI version for (code, block)
    recovered: AtomicU64, // decoded only after retrying against block-1's ABI
    docs: AtomicU64,      // grouped docs emitted
}

/// Why an act.data failed to decode at a given block.
enum Fail {
    NoAbi,
    NoType,
    Decode(String),
}

type Failures =
    std::sync::Mutex<std::collections::BTreeMap<(&'static str, String, String), (u64, String)>>;

fn record_failure(failures: &Failures, reason: &'static str, code: &str, action: &str, sample: &str) {
    let mut m = failures.lock().unwrap();
    let e = m
        .entry((reason, code.to_string(), action.to_string()))
        .or_insert((0, String::new()));
    e.0 += 1;
    if e.1.is_empty() && !sample.is_empty() {
        e.1 = sample.chars().take(140).collect();
    }
}

/// One level-2 decode attempt: deserialize `data` for `action` (u64 name) against the contract
/// ABI version active at `block`, writing JSON into `out`. Pure Rust — no abieos context.
fn decode_action(
    reg: &mut Registry,
    out: &mut String,
    code: u64,
    action: u64,
    data: &[u8],
    block: u32,
) -> std::result::Result<(), Fail> {
    let Some(handle) = reg.active(code, block) else {
        return Err(Fail::NoAbi);
    };
    let ty = match handle.type_for_action(action) {
        Some(t) => t.to_owned(),
        None => return Err(Fail::NoType),
    };
    handle
        .bin_to_json_into(&ty, data, out)
        .map_err(|e| Fail::Decode(format!("{e:?}")))
}

/// A processed action_trace, held until the whole transaction is grouped.
struct Proc {
    action_ordinal: u64,
    creator_action_ordinal: u64,
    act_digest: String,
    receipt: Value, // action_receipt_v0 object
    doc: Map<String, Value>,
}

/// `delta !== '0'` for an account_ram_deltas entry's delta (int64 may be number or string).
fn nonzero_delta(v: &Value) -> bool {
    match v.get("delta") {
        Some(Value::String(s)) => s != "0",
        Some(Value::Number(n)) => n.as_i64() != Some(0),
        _ => true,
    }
}

/// Group notification receipts and clean each resulting doc, then send. Faithful port of
/// Hyperion `groupActionTraces` (action-dedup.ts) + `cleanActionTrace` (ds-pool.ts).
fn finalize_and_emit(procs: Vec<Proc>, sink: Option<&Sender<String>>, stats: &Stats) {
    if procs.is_empty() {
        return;
    }
    if procs.len() == 1 {
        let mut p = procs.into_iter().next().unwrap();
        let mut receipt = p.receipt;
        if let Value::Object(m) = &mut receipt {
            if let Some(cs) = m.remove("code_sequence") {
                p.doc.insert("code_sequence".into(), cs);
            }
            if let Some(asq) = m.remove("abi_sequence") {
                p.doc.insert("abi_sequence".into(), asq);
            }
        }
        p.doc.insert("receipts".into(), Value::Array(vec![receipt]));
        clean_and_send(p.doc, sink, stats);
        return;
    }

    // Pass 1: action_ordinal -> act_digest, to tell notifications (same digest as creator)
    // from inline actions (different digest).
    let digest_by_ordinal: HashMap<u64, String> = procs
        .iter()
        .map(|p| (p.action_ordinal, p.act_digest.clone()))
        .collect();
    let canonical = |p: &Proc| -> u64 {
        if p.creator_action_ordinal > 0
            && digest_by_ordinal.get(&p.creator_action_ordinal) == Some(&p.act_digest)
        {
            p.creator_action_ordinal
        } else {
            p.action_ordinal
        }
    };

    let keys: Vec<String> = procs
        .iter()
        .map(|p| format!("{}:{}", p.act_digest, canonical(p)))
        .collect();
    let mut group_receipts: HashMap<String, Vec<Value>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        group_receipts
            .entry(keys[i].clone())
            .or_default()
            .push(p.receipt.clone());
    }

    // The first proc of each group becomes the head doc; merge the group's receipts into it.
    let mut emitted: HashSet<String> = HashSet::new();
    for (i, p) in procs.into_iter().enumerate() {
        let key = &keys[i];
        if emitted.contains(key) {
            continue;
        }
        let Some(receipts) = group_receipts.remove(key) else {
            continue;
        };
        emitted.insert(key.clone());
        let mut doc = p.doc;
        let mut out_receipts = Vec::with_capacity(receipts.len());
        for mut r in receipts {
            if let Value::Object(m) = &mut r {
                if let Some(cs) = m.remove("code_sequence") {
                    doc.insert("code_sequence".into(), cs);
                }
                if let Some(asq) = m.remove("abi_sequence") {
                    doc.insert("abi_sequence".into(), asq);
                }
            }
            out_receipts.push(r);
        }
        doc.insert("receipts".into(), Value::Array(out_receipts));
        clean_and_send(doc, sink, stats);
    }
}

/// Port of Hyperion `cleanActionTrace`: prune empties, hoist act_digest from receipts[0] to the
/// doc and strip it from each receipt, drop the singular receiver.
fn clean_and_send(mut t: Map<String, Value>, sink: Option<&Sender<String>>, stats: &Stats) {
    if t.get("return_value").and_then(|v| v.as_str()) == Some("") {
        t.remove("return_value");
    }
    if t.get("context_free") == Some(&Value::Bool(false)) {
        t.remove("context_free");
    }
    if matches!(t.get("elapsed"), Some(Value::String(s)) if s == "0")
        || t.get("elapsed").and_then(|v| v.as_i64()) == Some(0)
    {
        t.remove("elapsed");
    }
    let has_receipts = t
        .get("receipts")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if has_receipts {
        let ad = t["receipts"][0].get("act_digest").cloned();
        if let Some(ad) = ad {
            t.insert("act_digest".into(), ad);
        }
        if let Some(Value::Array(arr)) = t.get_mut("receipts") {
            for r in arr.iter_mut() {
                if let Value::Object(m) = r {
                    m.remove("act_digest");
                }
            }
        }
    } else {
        t.remove("receipts");
    }
    t.remove("receiver");
    if t.get("net_usage_words").and_then(|v| v.as_i64()) == Some(0) {
        t.remove("net_usage_words");
    }
    stats.docs.fetch_add(1, Relaxed);
    if let Some(tx) = sink {
        let _ = tx.send(Value::Object(t).to_string());
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    log_path: &str,
    idx_path: &str,
    first_block: u32,
    cs: u32,
    ce: u32,
    abi_index: &AbiIndex,
    stats: &Stats,
    failures: &Failures,
    sink: Option<&Sender<String>>,
) -> Result<()> {
    let names = Abieos::new(); // string_to_name only (act.account/act.name -> u64)
    let mut h_ship = AbiHandle::from_json(SHIP_ABI_JSON)
        .map_err(|e| anyhow!("embedded SHiP ABI failed to parse: {e:?}"))?;
    let mut reg = Registry::new(abi_index);
    let mut trace_buf = String::new();
    let mut data_buf = String::new();

    let mut idx = File::open(idx_path)?;
    idx.seek(SeekFrom::Start((cs - first_block) as u64 * 8))?;
    let mut ob = [0u8; 8];
    idx.read_exact(&mut ob)?;
    let mut pos = u64::from_le_bytes(ob);
    let mut log = BufReader::with_capacity(8 << 20, File::open(log_path)?);
    log.seek(SeekFrom::Start(pos))?;

    let mut hdr = [0u8; 48];
    loop {
        if log.read_exact(&mut hdr).is_err() {
            break;
        }
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        if block_num > ce {
            break;
        }
        let block_id = hex::encode(&hdr[8..40]);
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        let mut payload = vec![0u8; payload_size as usize];
        log.read_exact(&mut payload)?;
        let entry_end = pos + 48 + payload_size;
        let mut suf = [0u8; 8];
        pos = if log.read_exact(&mut suf).is_ok() && u64::from_le_bytes(suf) == pos {
            entry_end + 8
        } else {
            log.seek_relative(-(suf.len() as i64)).ok();
            entry_end
        };
        stats.blocks.fetch_add(1, Relaxed);

        let inflated = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            _ => continue,
        };
        // L1: decode the whole block's traces against the SHiP ABI in one call.
        if h_ship
            .bin_to_json_into("transaction_trace[]", &inflated, &mut trace_buf)
            .is_err()
        {
            continue;
        }
        let Ok(Value::Array(txs)) = serde_json::from_str::<Value>(&trace_buf) else {
            continue;
        };
        for tx in &txs {
            // tx is ["transaction_trace_v0", {..}]
            let Some(txv) = tx.get(1) else { continue };
            stats.txs.fetch_add(1, Relaxed);
            let trx_id = txv
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_lowercase();
            let cpu = txv.get("cpu_usage_us").cloned();
            let net = txv.get("net_usage_words").cloned();
            let Some(action_traces) = txv.get("action_traces").and_then(|v| v.as_array()) else {
                continue;
            };

            let mut procs: Vec<Proc> = Vec::new();
            let mut usage_included = false;
            for at in action_traces {
                let tag = at.get(0).and_then(|v| v.as_str()).unwrap_or_default();
                if !tag.starts_with("action_trace_") {
                    continue;
                }
                let Some(av) = at.get(1) else { continue };
                // Hyperion only indexes actions with except===null; receipt must be present.
                if !av.get("except").map(|v| v.is_null()).unwrap_or(true) {
                    continue;
                }
                let Some(receipt_obj) = av
                    .get("receipt")
                    .and_then(|r| r.get(1))
                    .filter(|r| r.is_object())
                    .cloned()
                else {
                    continue;
                };
                let Some(act) = av.get("act") else { continue };
                let account = act.get("account").and_then(|v| v.as_str()).unwrap_or_default();
                let name = act.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                let data_hex = act.get("data").and_then(|v| v.as_str()).unwrap_or_default();
                let (Ok(code), Ok(action_u64)) =
                    (names.string_to_name(account), names.string_to_name(name))
                else {
                    continue;
                };
                stats.actions.fetch_add(1, Relaxed);

                // L2: decode act.data against the contract ABI active at the block; retry at
                // block-1 (same-block setabi boundary); else preserve raw hex (ds_error).
                let mut data_value: Option<Value> = None;
                if let Ok(bin) = hex::decode(data_hex) {
                    let r1 = decode_action(&mut reg, &mut data_buf, code, action_u64, &bin, block_num);
                    let outcome = match r1 {
                        Ok(()) => Ok(false),
                        Err(first) => {
                            let retry = if !matches!(first, Fail::NoAbi) && block_num > 1 {
                                decode_action(&mut reg, &mut data_buf, code, action_u64, &bin, block_num - 1)
                            } else {
                                Err(first)
                            };
                            match retry {
                                Ok(()) => Ok(true),
                                Err(f) => Err(f),
                            }
                        }
                    };
                    match outcome {
                        Ok(recovered) => {
                            if recovered {
                                stats.recovered.fetch_add(1, Relaxed);
                            }
                            if let Ok(v) = serde_json::from_str::<Value>(&data_buf) {
                                data_value = Some(v);
                            }
                        }
                        Err(f) => {
                            let (reason, sample) = match f {
                                Fail::NoType => ("no_type", String::new()),
                                Fail::Decode(e) => ("decode", e),
                                Fail::NoAbi => ("no_abi", String::new()),
                            };
                            if reason == "no_abi" {
                                stats.no_abi.fetch_add(1, Relaxed);
                            }
                            record_failure(failures, reason, account, name, &sample);
                        }
                    }
                }

                // Build the action doc (block-context fields, flattened act, ordinals).
                let mut doc = Map::new();
                doc.insert("block_num".into(), Value::from(block_num));
                doc.insert("block_id".into(), Value::from(block_id.clone()));
                doc.insert("trx_id".into(), Value::from(trx_id.clone()));
                if let Some(gs) = receipt_obj
                    .get("global_sequence")
                    .and_then(|v| v.as_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    doc.insert("global_sequence".into(), Value::from(gs));
                }
                if let Some(o) = av.get("action_ordinal").cloned() {
                    doc.insert("action_ordinal".into(), o);
                }
                if let Some(o) = av.get("creator_action_ordinal").cloned() {
                    doc.insert("creator_action_ordinal".into(), o);
                }
                if let Some(cf) = av.get("context_free").cloned() {
                    doc.insert("context_free".into(), cf);
                }
                if let Some(el) = av.get("elapsed").cloned() {
                    doc.insert("elapsed".into(), el);
                }
                if let Some(rv) = av.get("return_value").cloned() {
                    doc.insert("return_value".into(), rv);
                }
                if let Some(deltas) = av.get("account_ram_deltas").and_then(|v| v.as_array()) {
                    let filtered: Vec<Value> =
                        deltas.iter().filter(|d| nonzero_delta(d)).cloned().collect();
                    if !filtered.is_empty() {
                        doc.insert("account_ram_deltas".into(), Value::Array(filtered));
                    }
                }
                // flattened act
                let mut act_out = Map::new();
                act_out.insert("account".into(), Value::from(account));
                act_out.insert("name".into(), Value::from(name));
                if let Some(auth) = act.get("authorization").cloned() {
                    act_out.insert("authorization".into(), auth);
                }
                match data_value {
                    Some(v) => {
                        act_out.insert("data".into(), v);
                        stats.decoded.fetch_add(1, Relaxed);
                    }
                    None => {
                        act_out.insert("data".into(), Value::from(data_hex.to_lowercase()));
                        doc.insert("ds_error".into(), Value::Bool(true));
                        stats.raw.fetch_add(1, Relaxed);
                    }
                }
                doc.insert("act".into(), Value::Object(act_out));
                // usage on the first action of the transaction (extendFirstAction)
                if !usage_included {
                    if let Some(c) = cpu.clone() {
                        doc.insert("cpu_usage_us".into(), c);
                    }
                    if let Some(n) = net.clone() {
                        doc.insert("net_usage_words".into(), n);
                    }
                    usage_included = true;
                }

                let action_ordinal = av.get("action_ordinal").and_then(|v| v.as_u64()).unwrap_or(0);
                let creator_action_ordinal = av
                    .get("creator_action_ordinal")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let act_digest = receipt_obj
                    .get("act_digest")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                procs.push(Proc {
                    action_ordinal,
                    creator_action_ordinal,
                    act_digest,
                    receipt: receipt_obj,
                    doc,
                });
            }
            finalize_and_emit(procs, sink, stats);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Fail fast if the embedded SHiP ABI is wrong/unparseable before spawning workers.
    AbiHandle::from_json(SHIP_ABI_JSON)
        .map_err(|e| anyhow!("embedded SHiP ABI ({} bytes) failed to parse: {e:?}", SHIP_ABI_JSON.len()))?;

    eprintln!("[action-proto] loading ABI index {} ...", args.abi_index);
    let abi_index = Arc::new(load_abi_index(&args.abi_index)?);
    eprintln!("[action-proto] {} contracts in ABI index", abi_index.len());

    let log_path = format!("{}/trace_history.log", args.from_disk);
    let idx_path = format!("{}/trace_history.index", args.from_disk);
    let mut f = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
    let mut hdr = [0u8; 48];
    f.read_exact(&mut hdr).context("read first header")?;
    if !is_ship_magic(u64::from_le_bytes(hdr[0..8].try_into().unwrap())) {
        bail!("{log_path} is not a state-history log");
    }
    let first_block = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
    let n_idx = (std::fs::metadata(&idx_path)?.len() / 8) as u32;
    let last_block = first_block + n_idx.saturating_sub(1);
    let start = args.start.max(first_block);
    let end = args.end.min(last_block);
    if start > end {
        bail!("empty range after clamp to [{first_block}..{last_block}]");
    }
    let threads = args.threads.max(1);
    let total = (end - start + 1) as u64;
    let chunk = total.div_ceil(threads as u64) as u32;
    eprintln!("[action-proto] decoding action_traces [{start}..{end}] ({total} blocks) with {threads} threads");

    let stats = Arc::new(Stats::default());
    let failures = Arc::new(Failures::default());
    let (tx, rx) = mpsc::channel::<String>();
    let mut out: Option<Box<dyn Write + Send>> = args
        .out
        .as_ref()
        .map(|p| -> Result<Box<dyn Write + Send>> { Ok(Box::new(BufWriter::new(File::create(p)?))) })
        .transpose()?;
    let emit = out.is_some();
    let t0 = Instant::now();

    std::thread::scope(|s| {
        let written = s.spawn(move || {
            let mut n = 0u64;
            if let Some(w) = out.as_mut() {
                for line in rx {
                    let _ = writeln!(w, "{line}");
                    n += 1;
                }
                let _ = w.flush();
            } else {
                for _ in rx {}
            }
            n
        });
        let mut handles = Vec::new();
        for i in 0..threads {
            let cs = start.saturating_add(i.saturating_mul(chunk));
            if cs > end {
                break;
            }
            let ce = ((cs as u64 + chunk as u64 - 1).min(end as u64)) as u32;
            let (lp, ip) = (log_path.clone(), idx_path.clone());
            let (ai, st, fl) = (abi_index.clone(), stats.clone(), failures.clone());
            let txc = if emit { Some(tx.clone()) } else { None };
            handles.push(s.spawn(move || {
                if let Err(e) = worker(&lp, &ip, first_block, cs, ce, &ai, &st, &fl, txc.as_ref()) {
                    eprintln!("[action-proto] worker {i} [{cs}..{ce}] FAILED: {e:#}");
                }
            }));
        }
        drop(tx);
        for h in handles {
            let _ = h.join();
        }
        let _ = written.join();
    });

    let secs = t0.elapsed().as_secs_f64();
    let b = stats.blocks.load(Relaxed);
    let txs = stats.txs.load(Relaxed);
    let actions = stats.actions.load(Relaxed);
    let decoded = stats.decoded.load(Relaxed);
    let raw = stats.raw.load(Relaxed);
    eprintln!(
        "[action-proto] done: {b} blocks in {secs:.1}s ({:.0} blk/s) | txs={txs} actions={actions} -> docs={} | act.data decoded={decoded} ({:.2}%) + raw={raw} recovered_via_block-1={} no_abi={}",
        b as f64 / secs.max(1e-9),
        stats.docs.load(Relaxed),
        if actions > 0 { 100.0 * decoded as f64 / actions as f64 } else { 0.0 },
        stats.recovered.load(Relaxed),
        stats.no_abi.load(Relaxed),
    );
    let f = failures.lock().unwrap();
    if !f.is_empty() {
        let mut by_reason: std::collections::BTreeMap<&str, u64> = std::collections::BTreeMap::new();
        for ((reason, _, _), (cnt, _)) in f.iter() {
            *by_reason.entry(reason).or_default() += cnt;
        }
        eprintln!("[action-proto] raw (undecodable) act.data by reason: {by_reason:?}");
        let mut top: Vec<_> = f.iter().collect();
        top.sort_by_key(|e| std::cmp::Reverse(e.1 .0));
        eprintln!("[action-proto] top undecoded contract/action:");
        for ((reason, code, action), (cnt, sample)) in top.into_iter().take(25) {
            eprintln!("  {cnt:>6}  {reason:<8} {code}/{action}  {sample}");
        }
    }
    Ok(())
}
