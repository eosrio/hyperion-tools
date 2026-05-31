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
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::Parser;
use rs_abieos::{AbiHandle, Abieos};
use serde_json::{Map, Value};

use abi_scanner::blocks::{slot_to_iso, BlockLog};
use abi_scanner::disk::{decode_payload, is_ship_magic};
use abi_scanner::trace;

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
    /// nodeos blocks dir (blocks.{log,index}) — supplies the block-header fields `@timestamp`
    /// and `producer`, which live only in signed_block (Phase B). Omit to leave them out.
    #[arg(long)]
    blocks_dir: Option<String>,
    /// match Hyperion's `features.index_transfer_memo`: move `memo` from a transfer's `act.data`
    /// into `@transfer.memo` (default: keep `memo` in `act.data`).
    #[arg(long, default_value_t = false)]
    index_transfer_memo: bool,
    /// direct Elasticsearch `_bulk` sink (e.g. http://localhost:9200). When set, decoded docs are
    /// POSTed straight to ES (the `--out` NDJSON path is ignored) to find the true ES write ceiling
    /// without the GIL-bound Python loader.
    #[arg(long)]
    es: Option<String>,
    /// chain name for the ES `_index` (`<chain>-action-<version>-<partition>`).
    #[arg(long, default_value = "wax")]
    chain: String,
    /// index version for the ES `_index` (`<chain>-action-<version>-<partition>`).
    #[arg(long, default_value = "v1")]
    index_version: String,
    /// `INDEX_PARTITION_SIZE`: partition = ceil(block_num / partition_size), zero-padded to 6 digits.
    #[arg(long, default_value_t = 10_000_000)]
    partition_size: u64,
    /// docs per `_bulk` request (per poster thread).
    #[arg(long, default_value_t = 4000)]
    es_batch: usize,
    /// concurrent `_bulk` poster threads pulling from the doc channel.
    #[arg(long, default_value_t = 8)]
    es_workers: usize,
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

/// One emitted action document flowing over the channel to the sink (NDJSON writer or ES posters).
/// Carries `block_num` + `global_sequence` alongside the serialized `doc` so the ES posters can
/// build the `_bulk` meta line (`_index` partition + `_id`) WITHOUT re-parsing the doc JSON.
struct DocMsg {
    block_num: u32,
    global_sequence: u64,
    doc: String,
}

/// A processed action_trace, held until the whole transaction is grouped. The doc fields are
/// pre-serialized into `body` (with a trailing comma); `receipt_json` is the receipt object for
/// `receipts[]` (without the hoisted act_digest/code_sequence/abi_sequence).
struct Proc {
    block_num: u32,
    global_sequence: u64,
    action_ordinal: u64,
    creator_action_ordinal: u64,
    act_digest: String,
    code_sequence: u32,
    abi_sequence: u32,
    receipt_json: String,
    body: String,
}

// ---------------------------------------------------------------------------------------------
// @-field handlers — faithful ports of Hyperion's action_data modules (transfer.ts, eosio-*.ts).
// They run per action AFTER the act.data decode and BEFORE grouping, adding a computed `@<name>`
// field and (for some) trimming/removing `act.data`. Dispatched by act.name (wildcard) + account.
// ---------------------------------------------------------------------------------------------

/// JS-`JSON.stringify`-style number: integral floats render without a trailing `.0` (1.0 -> 1),
/// matching `parseFloat`+`JSON.stringify`. (Very small magnitudes may still differ JS-vs-Rust in
/// exponential-vs-decimal form — a cosmetic edge on these denormalized `@`-amount fields.)
fn js_number(f: f64) -> Value {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 9_007_199_254_740_992.0 {
        Value::from(f as i64)
    } else {
        Value::from(f)
    }
}

fn act_data_mut(doc: &mut Map<String, Value>) -> Option<&mut Map<String, Value>> {
    doc.get_mut("act")?.get_mut("data")?.as_object_mut()
}
fn act_data(doc: &Map<String, Value>) -> Option<&Map<String, Value>> {
    doc.get("act")?.get("data")?.as_object()
}
fn delete_act_data(doc: &mut Map<String, Value>) {
    if let Some(act) = doc.get_mut("act").and_then(|a| a.as_object_mut()) {
        act.remove("data");
    }
}
/// `parseFloat(asset.split(' ')[0])` — the numeric part of an `"1.2345 SYM"` asset string.
fn asset_amount(v: &Value) -> f64 {
    v.as_str()
        .and_then(|s| s.split(' ').next())
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0)
}
fn str_of(v: &Value) -> String {
    v.as_str().map(|s| s.to_string()).unwrap_or_else(|| v.to_string())
}

/// Apply the matching `@`-field handler(s) for this action, mutating `doc` in place.
fn process_action_data(doc: &mut Map<String, Value>, account: &str, name: &str, memo_in_field: bool) {
    if name == "transfer" {
        h_transfer(doc, memo_in_field);
    }
    if account != "eosio" {
        return;
    }
    match name {
        "newaccount" => h_newaccount(doc),
        "updateauth" => h_updateauth(doc),
        "delegatebw" => h_delegatebw(doc),
        "undelegatebw" => h_undelegatebw(doc),
        "buyram" => h_buyram(doc),
        "buyrambytes" => h_buyrambytes(doc),
        "buyrex" => h_buyrex(doc),
        "unstaketorex" => h_unstaketorex(doc),
        "voteproducer" => h_voteproducer(doc),
        _ => {}
    }
}

fn h_transfer(doc: &mut Map<String, Value>, memo_in_field: bool) {
    let xfer = {
        let Some(data) = act_data_mut(doc) else { return };
        let qtd = data
            .get("quantity")
            .and_then(Value::as_str)
            .or_else(|| data.get("value").and_then(Value::as_str));
        let Some(qtd) = qtd else { return };
        let mut parts = qtd.split(' ');
        let (Some(amount), Some(symbol)) = (
            parts.next().and_then(|t| t.parse::<f64>().ok()),
            parts.next(),
        ) else {
            return;
        };
        let from = data.get("from").map(str_of).unwrap_or_default();
        let to = data.get("to").map(str_of).unwrap_or_default();
        let mut x = Map::new();
        x.insert("from".into(), Value::from(from));
        x.insert("to".into(), Value::from(to));
        x.insert("amount".into(), js_number(amount));
        x.insert("symbol".into(), Value::from(symbol.to_string()));
        data.remove("from");
        data.remove("to");
        if memo_in_field {
            if let Some(m) = data.remove("memo") {
                x.insert("memo".into(), m);
            }
        }
        x
    };
    doc.insert("@transfer".into(), Value::Object(xfer));
}

fn h_newaccount(doc: &mut Map<String, Value>) {
    let na = {
        let Some(data) = act_data_mut(doc) else { return };
        let name = if let Some(n) = data.get("newact").cloned() {
            n
        } else if let Some(n) = data.remove("name") {
            n
        } else {
            return;
        };
        let mut m = Map::new();
        m.insert("active".into(), data.get("active").cloned().unwrap_or(Value::Null));
        m.insert("owner".into(), data.get("owner").cloned().unwrap_or(Value::Null));
        m.insert("newact".into(), name);
        m
    };
    doc.insert("@newaccount".into(), Value::Object(na));
}

fn h_updateauth(doc: &mut Map<String, Value>) {
    let ua = {
        let Some(data) = act_data_mut(doc) else { return };
        if let Some(auth) = data.get_mut("auth").and_then(|a| a.as_object_mut()) {
            for k in ["accounts", "keys", "waits"] {
                if auth.get(k).and_then(|v| v.as_array()).map(|a| a.is_empty()).unwrap_or(false) {
                    auth.remove(k);
                }
            }
        }
        let mut m = Map::new();
        m.insert("permission".into(), data.get("permission").cloned().unwrap_or(Value::Null));
        m.insert("parent".into(), data.get("parent").cloned().unwrap_or(Value::Null));
        m.insert("auth".into(), data.get("auth").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@updateauth".into(), Value::Object(ua));
}

fn h_delegatebw(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let (cpu, net) = if data.contains_key("stake_cpu_quantity") && data.contains_key("stake_net_quantity") {
            (asset_amount(&data["stake_cpu_quantity"]), asset_amount(&data["stake_net_quantity"]))
        } else {
            (0.0, 0.0)
        };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(cpu + net));
        m.insert("stake_cpu_quantity".into(), js_number(cpu));
        m.insert("stake_net_quantity".into(), js_number(net));
        m.insert("from".into(), data.get("from").cloned().unwrap_or(Value::Null));
        m.insert("receiver".into(), data.get("receiver").cloned().unwrap_or(Value::Null));
        m.insert("transfer".into(), data.get("transfer").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@delegatebw".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_undelegatebw(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let (cpu, net) = if data.contains_key("unstake_cpu_quantity") && data.contains_key("unstake_net_quantity") {
            (asset_amount(&data["unstake_cpu_quantity"]), asset_amount(&data["unstake_net_quantity"]))
        } else {
            (0.0, 0.0)
        };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(cpu + net));
        m.insert("unstake_cpu_quantity".into(), js_number(cpu));
        m.insert("unstake_net_quantity".into(), js_number(net));
        m.insert("from".into(), data.get("from").cloned().unwrap_or(Value::Null));
        m.insert("receiver".into(), data.get("receiver").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@undelegatebw".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_buyram(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let mut m = Map::new();
        m.insert("payer".into(), data.get("payer").cloned().unwrap_or(Value::Null));
        m.insert("receiver".into(), data.get("receiver").cloned().unwrap_or(Value::Null));
        if let Some(q) = data.get("quant") {
            m.insert("quant".into(), js_number(asset_amount(q)));
        }
        m
    };
    doc.insert("@buyram".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_buyrambytes(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let bytes = data
            .get("bytes")
            .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
            .unwrap_or(0);
        let mut m = Map::new();
        m.insert("bytes".into(), Value::from(bytes));
        m.insert("payer".into(), data.get("payer").cloned().unwrap_or(Value::Null));
        m.insert("receiver".into(), data.get("receiver").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@buyrambytes".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_buyrex(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(data.get("amount").map(asset_amount).unwrap_or(0.0)));
        m.insert("from".into(), data.get("from").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@buyrex".into(), Value::Object(m));
}

fn h_unstaketorex(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let (cpu, net) = if data.contains_key("from_cpu") && data.contains_key("from_net") {
            (asset_amount(&data["from_cpu"]), asset_amount(&data["from_net"]))
        } else {
            (0.0, 0.0)
        };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(cpu + net));
        m.insert("owner".into(), data.get("owner").cloned().unwrap_or(Value::Null));
        m.insert("receiver".into(), data.get("receiver").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@unstaketorex".into(), Value::Object(m));
}

fn h_voteproducer(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let mut m = Map::new();
        m.insert("proxy".into(), data.get("proxy").cloned().unwrap_or(Value::Null));
        m.insert("producers".into(), data.get("producers").cloned().unwrap_or(Value::Null));
        m
    };
    doc.insert("@voteproducer".into(), Value::Object(m));
}

/// Append `s` to `out` as a JSON string literal (quotes + escaping).
fn push_json_str(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Does this action get a computed `@`-field? (If so its act.data must be parsed + transformed;
/// otherwise the raw decoded JSON is spliced verbatim.)
fn is_at_handled(account: &str, name: &str) -> bool {
    name == "transfer"
        || (account == "eosio"
            && matches!(
                name,
                "newaccount"
                    | "updateauth"
                    | "delegatebw"
                    | "undelegatebw"
                    | "buyram"
                    | "buyrambytes"
                    | "buyrex"
                    | "unstaketorex"
                    | "voteproducer"
            ))
}

/// Group notification receipts (port of Hyperion `groupActionTraces`) and assemble + send each
/// doc as a JSON string. `act_digest`/`code_sequence`/`abi_sequence` are hoisted onto the doc and
/// the group's receipts go in `receipts[]`. Per-field `cleanActionTrace` pruning already happened
/// while building `Proc.body`.
fn finalize_and_emit(procs: Vec<Proc>, sink: Option<&Sender<DocMsg>>, stats: &Stats) {
    if procs.is_empty() {
        return;
    }
    if procs.len() == 1 {
        let p = &procs[0];
        emit_doc(p, &[p.receipt_json.as_str()], sink, stats);
        return;
    }
    // action_ordinal -> act_digest, to tell notifications (same digest as creator) from inline.
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
    let mut group_receipts: HashMap<&str, Vec<&str>> = HashMap::new();
    for (i, p) in procs.iter().enumerate() {
        group_receipts
            .entry(keys[i].as_str())
            .or_default()
            .push(p.receipt_json.as_str());
    }
    // The first proc of each group becomes the head doc.
    let mut emitted: HashSet<&str> = HashSet::new();
    for (i, p) in procs.iter().enumerate() {
        let key = keys[i].as_str();
        if !emitted.insert(key) {
            continue;
        }
        emit_doc(p, &group_receipts[key], sink, stats);
    }
}

/// Assemble one grouped action doc: `{ <body> "act_digest":.., "code_sequence":.., "abi_sequence":.., "receipts":[..] }`.
fn emit_doc(p: &Proc, receipts: &[&str], sink: Option<&Sender<DocMsg>>, stats: &Stats) {
    stats.docs.fetch_add(1, Relaxed);
    let Some(tx) = sink else { return };
    let rlen: usize = receipts.iter().map(|r| r.len() + 1).sum();
    let mut s = String::with_capacity(p.body.len() + p.act_digest.len() + rlen + 96);
    s.push('{');
    s.push_str(&p.body); // ends with a trailing comma
    s.push_str("\"act_digest\":");
    push_json_str(&mut s, &p.act_digest);
    s.push_str(",\"code_sequence\":");
    s.push_str(&p.code_sequence.to_string());
    s.push_str(",\"abi_sequence\":");
    s.push_str(&p.abi_sequence.to_string());
    s.push_str(",\"receipts\":[");
    for (i, r) in receipts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r);
    }
    s.push_str("]}");
    let _ = tx.send(DocMsg {
        block_num: p.block_num,
        global_sequence: p.global_sequence,
        doc: s,
    });
}

#[allow(clippy::too_many_arguments)]
fn worker(
    log_path: &str,
    idx_path: &str,
    first_block: u32,
    cs: u32,
    ce: u32,
    abi_index: &AbiIndex,
    blocks_dir: Option<&str>,
    index_transfer_memo: bool,
    stats: &Stats,
    failures: &Failures,
    sink: Option<&Sender<DocMsg>>,
) -> Result<()> {
    let mut reg = Registry::new(abi_index);
    let mut blocklog = match blocks_dir {
        Some(d) => Some(BlockLog::open(d)?),
        None => None,
    };
    let profile = std::env::var("ACTION_PROFILE").is_ok();
    let (mut d_l1, mut d_act, mut d_l2) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
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

        // block-header fields (Phase B): @timestamp + producer from the block log, if provided.
        let (block_producer, block_ts) = match blocklog.as_mut().and_then(|bl| bl.header(block_num)) {
            Some((prod, slot)) => (Some(trace::name_to_string(prod)), Some(slot_to_iso(slot))),
            None => (None, None),
        };

        let inflated = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            _ => continue,
        };
        // L1 (hand-walk): parse the block's transaction_trace[] binary directly — no full-JSON
        // materialization, no serde re-parse; act.data stays a raw byte range.
        let l1_t = if profile { Some(Instant::now()) } else { None };
        let Some(txs) = trace::parse_block(&inflated) else {
            continue;
        };
        if let Some(t) = l1_t {
            d_l1 += t.elapsed();
        }
        let a_t = if profile { Some(Instant::now()) } else { None };
        for tx in &txs {
            stats.txs.fetch_add(1, Relaxed);
            let trx_id = hex::encode(tx.id);
            let mut procs: Vec<Proc> = Vec::new();
            let mut usage_included = false;
            for act in &tx.actions {
                // Hyperion only indexes actions with except===null and a receipt.
                if act.except {
                    continue;
                }
                let Some(receipt) = &act.receipt else { continue };
                let code = act.account;
                let action_u64 = act.name;
                let account = trace::name_to_string(code);
                let name = trace::name_to_string(action_u64);
                let data_bytes = &inflated[act.data.0..act.data.0 + act.data.1];
                stats.actions.fetch_add(1, Relaxed);

                // L2: decode act.data (raw bytes) against the contract ABI active at the block;
                // retry at block-1 (same-block setabi boundary); else preserve raw hex (ds_error).
                let l2_t = if profile { Some(Instant::now()) } else { None };
                let mut decoded = false;
                {
                    let r1 = decode_action(&mut reg, &mut data_buf, code, action_u64, data_bytes, block_num);
                    let outcome = match r1 {
                        Ok(()) => Ok(false),
                        Err(first) => {
                            let retry = if !matches!(first, Fail::NoAbi) && block_num > 1 {
                                decode_action(&mut reg, &mut data_buf, code, action_u64, data_bytes, block_num - 1)
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
                            decoded = true;
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
                            record_failure(failures, reason, &account, &name, &sample);
                        }
                    }
                }
                if let Some(t) = l2_t {
                    d_l2 += t.elapsed();
                }

                // act.data representation + computed @-fields. Non-@-handled actions splice the
                // raw decoded JSON verbatim (no parse/reserialize); @-handled ones parse, run the
                // handler on a tiny temp map, then re-serialize the (possibly trimmed) data.
                let mut at_fragment = String::new();
                let data_part: Option<String>;
                if decoded {
                    if is_at_handled(&account, &name) {
                        if let Ok(data_val) = serde_json::from_str::<Value>(&data_buf) {
                            let mut temp = Map::new();
                            let mut actm = Map::new();
                            actm.insert("data".into(), data_val);
                            temp.insert("act".into(), Value::Object(actm));
                            process_action_data(&mut temp, &account, &name, index_transfer_memo);
                            for (k, v) in &temp {
                                if k.starts_with('@') {
                                    push_json_str(&mut at_fragment, k);
                                    at_fragment.push(':');
                                    at_fragment.push_str(&v.to_string());
                                    at_fragment.push(',');
                                }
                            }
                            data_part = temp
                                .get("act")
                                .and_then(|a| a.get("data"))
                                .map(|d| d.to_string());
                        } else {
                            data_part = Some(data_buf.clone());
                        }
                    } else {
                        data_part = Some(data_buf.clone());
                    }
                    stats.decoded.fetch_add(1, Relaxed);
                } else {
                    let mut hx = String::new();
                    push_json_str(&mut hx, &hex::encode(data_bytes));
                    data_part = Some(hx);
                    stats.raw.fetch_add(1, Relaxed);
                }

                // Build the doc body directly as JSON; cleanActionTrace pruning = conditional
                // writes. Ends with a trailing comma. act_digest/code/abi/receipts are appended
                // by emit_doc after grouping.
                let mut body = String::with_capacity(data_part.as_ref().map_or(0, |d| d.len()) + 256);
                if let Some(ts) = &block_ts {
                    body.push_str("\"@timestamp\":");
                    push_json_str(&mut body, ts);
                    body.push(',');
                }
                if let Some(pr) = &block_producer {
                    body.push_str("\"producer\":");
                    push_json_str(&mut body, pr);
                    body.push(',');
                }
                body.push_str("\"block_num\":");
                body.push_str(&block_num.to_string());
                body.push_str(",\"block_id\":");
                push_json_str(&mut body, &block_id);
                body.push_str(",\"trx_id\":");
                push_json_str(&mut body, &trx_id);
                body.push_str(",\"global_sequence\":");
                body.push_str(&receipt.global_sequence.to_string());
                body.push_str(",\"action_ordinal\":");
                body.push_str(&act.action_ordinal.to_string());
                body.push_str(",\"creator_action_ordinal\":");
                body.push_str(&act.creator_action_ordinal.to_string());
                body.push(',');
                if act.context_free {
                    body.push_str("\"context_free\":true,");
                }
                if act.elapsed != 0 {
                    body.push_str("\"elapsed\":\"");
                    body.push_str(&act.elapsed.to_string());
                    body.push_str("\",");
                }
                if let Some(rv) = act.return_value {
                    if rv.1 > 0 {
                        body.push_str("\"return_value\":");
                        push_json_str(&mut body, &hex::encode_upper(&inflated[rv.0..rv.0 + rv.1]));
                        body.push(',');
                    }
                }
                let mut ram_open = false;
                for (a, d) in &act.account_ram_deltas {
                    if *d == 0 {
                        continue;
                    }
                    body.push_str(if ram_open {
                        ","
                    } else {
                        "\"account_ram_deltas\":["
                    });
                    ram_open = true;
                    body.push_str("{\"account\":");
                    push_json_str(&mut body, &trace::name_to_string(*a));
                    body.push_str(",\"delta\":\"");
                    body.push_str(&d.to_string());
                    body.push_str("\"}");
                }
                if ram_open {
                    body.push_str("],");
                }
                if !usage_included {
                    body.push_str("\"cpu_usage_us\":");
                    body.push_str(&tx.cpu_usage_us.to_string());
                    body.push(',');
                    if tx.net_usage_words != 0 {
                        body.push_str("\"net_usage_words\":");
                        body.push_str(&tx.net_usage_words.to_string());
                        body.push(',');
                    }
                    usage_included = true;
                }
                if !decoded {
                    body.push_str("\"ds_error\":true,");
                }
                body.push_str("\"act\":{\"account\":");
                push_json_str(&mut body, &account);
                body.push_str(",\"name\":");
                push_json_str(&mut body, &name);
                body.push_str(",\"authorization\":[");
                for (i, (actor, perm)) in act.authorization.iter().enumerate() {
                    if i > 0 {
                        body.push(',');
                    }
                    body.push_str("{\"actor\":");
                    push_json_str(&mut body, &trace::name_to_string(*actor));
                    body.push_str(",\"permission\":");
                    push_json_str(&mut body, &trace::name_to_string(*perm));
                    body.push('}');
                }
                body.push(']');
                if let Some(dp) = &data_part {
                    body.push_str(",\"data\":");
                    body.push_str(dp);
                }
                body.push_str("},");
                body.push_str(&at_fragment);

                // receipt object for receipts[] (act_digest/code_sequence/abi_sequence hoisted out).
                let mut receipt_json = String::with_capacity(160);
                receipt_json.push_str("{\"receiver\":");
                push_json_str(&mut receipt_json, &trace::name_to_string(receipt.receiver));
                receipt_json.push_str(",\"global_sequence\":\"");
                receipt_json.push_str(&receipt.global_sequence.to_string());
                receipt_json.push_str("\",\"recv_sequence\":\"");
                receipt_json.push_str(&receipt.recv_sequence.to_string());
                receipt_json.push_str("\",\"auth_sequence\":[");
                for (i, (a, sq)) in receipt.auth_sequence.iter().enumerate() {
                    if i > 0 {
                        receipt_json.push(',');
                    }
                    receipt_json.push_str("{\"account\":");
                    push_json_str(&mut receipt_json, &trace::name_to_string(*a));
                    receipt_json.push_str(",\"sequence\":\"");
                    receipt_json.push_str(&sq.to_string());
                    receipt_json.push_str("\"}");
                }
                receipt_json.push_str("]}");

                procs.push(Proc {
                    block_num,
                    global_sequence: receipt.global_sequence,
                    action_ordinal: act.action_ordinal as u64,
                    creator_action_ordinal: act.creator_action_ordinal as u64,
                    act_digest: hex::encode_upper(receipt.act_digest),
                    code_sequence: receipt.code_sequence,
                    abi_sequence: receipt.abi_sequence,
                    receipt_json,
                    body,
                });
            }
            finalize_and_emit(procs, sink, stats);
        }
        if let Some(t) = a_t {
            d_act += t.elapsed();
        }
    }
    if profile {
        eprintln!(
            "[action-proto][profile] worker [{cs}..{ce}]: l1_handwalk={:.2}s actions_build={:.2}s (of which l2_decode+parse={:.2}s)",
            d_l1.as_secs_f64(),
            d_act.as_secs_f64(),
            d_l2.as_secs_f64()
        );
    }
    Ok(())
}

/// Shared counters for the direct-ES `_bulk` sink (Arc'd across all poster threads).
#[derive(Default)]
struct EsStats {
    docs: AtomicU64,     // docs accepted into a posted _bulk body
    bytes: AtomicU64,    // total _bulk request body bytes
    requests: AtomicU64, // _bulk requests issued
    errors: AtomicU64,   // bulk responses whose body contained `"errors":true` (or transport errors)
}

/// Config the ES posters need to build each `_bulk` meta line. (Cheap to clone — only the URL +
/// chain/version are owned strings.)
#[derive(Clone)]
struct EsCfg {
    url: String, // `<es>/_bulk`
    chain: String,
    version: String,
    partition_size: u64,
    batch: usize,
}

/// `<chain>-action-<version>-<partition>`, partition = ceil(block_num / partition_size) zero-padded
/// to 6 digits (>= 1) — mirrors Hyperion's index naming (see bench/scripts/bulk-load.py).
fn action_index(cfg: &EsCfg, block_num: u32) -> String {
    let partition = (block_num as u64).div_ceil(cfg.partition_size).max(1);
    format!("{}-action-{}-{:06}", cfg.chain, cfg.version, partition)
}

/// One ES poster thread: pull up to `cfg.batch` docs from the shared channel (locking only while
/// draining, never while POSTing), accumulate them into an `application/x-ndjson` `_bulk` body
/// (`{"index":{"_index":..,"_id":..}}\n<doc>\n` per doc), POST to `<es>/_bulk`, and fold the
/// outcome into the shared `EsStats`. The action `_id` is the doc's `global_sequence`.
fn es_poster(rx: &std::sync::Mutex<mpsc::Receiver<DocMsg>>, cfg: &EsCfg, stats: &EsStats) {
    let mut body = String::new();
    loop {
        // Drain a batch under the lock, then release it before the (slow) network round-trip.
        let mut batch: Vec<DocMsg> = Vec::with_capacity(cfg.batch);
        {
            let guard = rx.lock().unwrap();
            // Block for the first item; once we have one, greedily take whatever is ready.
            match guard.recv() {
                Ok(msg) => batch.push(msg),
                Err(_) => return, // all senders dropped and channel drained
            }
            while batch.len() < cfg.batch {
                match guard.try_recv() {
                    Ok(msg) => batch.push(msg),
                    Err(_) => break,
                }
            }
        }
        if batch.is_empty() {
            continue;
        }

        body.clear();
        for m in &batch {
            let index = action_index(cfg, m.block_num);
            body.push_str("{\"index\":{\"_index\":\"");
            body.push_str(&index);
            body.push_str("\",\"_id\":\"");
            body.push_str(&m.global_sequence.to_string());
            body.push_str("\"}}\n");
            body.push_str(&m.doc);
            body.push('\n');
        }
        let n = batch.len() as u64;
        let nbytes = body.len() as u64;

        match minreq::post(cfg.url.as_str())
            .with_header("Content-Type", "application/x-ndjson")
            .with_body(body.as_str())
            .send()
        {
            Ok(resp) => {
                // Cheap error check: the bulk response sets `"errors":true` iff any item failed.
                let failed = resp.as_str().map(|s| s.contains("\"errors\":true")).unwrap_or(true)
                    || resp.status_code < 200
                    || resp.status_code >= 300;
                if failed {
                    stats.errors.fetch_add(1, Relaxed);
                }
            }
            Err(_) => {
                stats.errors.fetch_add(1, Relaxed);
            }
        }
        stats.docs.fetch_add(n, Relaxed);
        stats.bytes.fetch_add(nbytes, Relaxed);
        stats.requests.fetch_add(1, Relaxed);
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
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
    let (tx, rx) = mpsc::channel::<DocMsg>();

    // Output mode: --es (direct ES `_bulk`) takes precedence over --out (NDJSON); with neither set
    // we just drain the channel (pure-decode benchmark). Only build the NDJSON writer when ES is
    // NOT in play, so `--es` cleanly ignores `--out`.
    let es_cfg = args.es.as_ref().map(|url| EsCfg {
        url: format!("{}/_bulk", url.trim_end_matches('/')),
        chain: args.chain.clone(),
        version: args.index_version.clone(),
        partition_size: args.partition_size.max(1),
        batch: args.es_batch.max(1),
    });
    let es_workers = args.es_workers.max(1);
    let es_stats = Arc::new(EsStats::default());
    let mut out: Option<Box<dyn Write + Send>> = if es_cfg.is_some() {
        None
    } else {
        args.out
            .as_ref()
            .map(|p| -> Result<Box<dyn Write + Send>> {
                Ok(Box::new(BufWriter::new(File::create(p)?)))
            })
            .transpose()?
    };
    // Workers send docs whenever there is a consumer (ES posters or the NDJSON writer).
    let emit = es_cfg.is_some() || out.is_some();
    if let Some(c) = &es_cfg {
        eprintln!(
            "[action-proto] direct-ES sink -> {} | index={}-action-{}-<part> partition_size={} batch={} posters={}",
            c.url, c.chain, c.version, c.partition_size, c.batch, es_workers
        );
    }
    let t0 = Instant::now();

    std::thread::scope(|s| {
        // Consumer side: either N ES `_bulk` posters sharing the Receiver, or the single NDJSON
        // writer thread, or a lone drain thread.
        let rx_shared = Arc::new(std::sync::Mutex::new(rx));
        let mut consumers = Vec::new();
        if let Some(cfg) = es_cfg.clone() {
            for _ in 0..es_workers {
                let (rxc, cfgc, est) = (rx_shared.clone(), cfg.clone(), es_stats.clone());
                consumers.push(s.spawn(move || {
                    es_poster(&rxc, &cfgc, &est);
                }));
            }
        } else {
            consumers.push(s.spawn(move || {
                if let Some(w) = out.as_mut() {
                    for msg in rx_shared.lock().unwrap().iter() {
                        let _ = writeln!(w, "{}", msg.doc);
                    }
                    let _ = w.flush();
                } else {
                    for _ in rx_shared.lock().unwrap().iter() {}
                }
            }));
        }
        let mut handles = Vec::new();
        for i in 0..threads {
            let cs = start.saturating_add(i.saturating_mul(chunk));
            if cs > end {
                break;
            }
            let ce = ((cs as u64 + chunk as u64 - 1).min(end as u64)) as u32;
            let (lp, ip) = (log_path.clone(), idx_path.clone());
            let (ai, st, fl) = (abi_index.clone(), stats.clone(), failures.clone());
            let bd = args.blocks_dir.clone();
            let txc = if emit { Some(tx.clone()) } else { None };
            handles.push(s.spawn(move || {
                if let Err(e) = worker(&lp, &ip, first_block, cs, ce, &ai, bd.as_deref(), args.index_transfer_memo, &st, &fl, txc.as_ref()) {
                    eprintln!("[action-proto] worker {i} [{cs}..{ce}] FAILED: {e:#}");
                }
            }));
        }
        drop(tx);
        for h in handles {
            let _ = h.join();
        }
        for c in consumers {
            let _ = c.join();
        }
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
    if es_cfg.is_some() {
        let posted = es_stats.docs.load(Relaxed);
        let bytes = es_stats.bytes.load(Relaxed);
        let requests = es_stats.requests.load(Relaxed);
        let errors = es_stats.errors.load(Relaxed);
        let mb = bytes as f64 / 1e6;
        eprintln!(
            "[action-proto] ES sink: posted {posted} docs in {secs:.1}s ({:.0} docs/s) | {mb:.1} MB ({:.1} MB/s) | {requests} bulk reqs | errors={errors}",
            posted as f64 / secs.max(1e-9),
            mb / secs.max(1e-9),
        );
        if errors > 0 {
            eprintln!("[action-proto] WARNING: {errors} bulk request(s) reported errors — inspect ES response / mapping conflicts.");
        }
    }
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
