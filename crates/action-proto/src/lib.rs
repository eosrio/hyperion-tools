//! action-proto — experimental direct-from-disk Hyperion *action* indexer.
//!
//! The sibling of `delta-proto`, pointed at `trace_history.{log,index}` instead of
//! `chain_state_history.*`. It reads the append-only trace log off disk in parallel (no nodeos,
//! no SHiP websocket), decodes each block's `transaction_trace[]`, walks the nested
//! `action_traces`, decodes every action's `act.data` against the contract ABI active *at that
//! block*, groups notification receipts à la Hyperion, and emits `<chain>-action-v1`-shaped
//! NDJSON. It is the next slice after deltas in the "delete the ship-0 bottleneck" direction.
//!
//! TWO-LEVEL DECODE:
//!   L1 (trace skeleton): a hand walk of the block's `transaction_trace[]` binary
//!      ([`hyperion_ship::trace::parse_block`]); `act.data` stays a raw byte range.
//!   L2 (act.data): per action, look up the contract ABI active at the block in the per-worker
//!      `Registry`, resolve the action's type and decode it. On failure retry at block-1 (same-block
//!      setabi boundary); if still undecodable, preserve the raw hex (`ds_error`) — every action
//!      still emits a doc.
//!
//! The block-header fields `@timestamp` and `producer` live only in the signed_block, so they come
//! from the block log (`--blocks-dir`).
//!
//! WIRE (Hyperion 4.5): every block-log entry is walked once (`wire_block`). Its header must hash
//! to the id the trace log names, and each header must name the previous block's id, so a run
//! checks that the logs agree over its whole range. Actions are grouped by input origin as the 4.5
//! indexer groups them (`occurrence`). A chain id adds schema-3 occurrence metadata to every
//! action document, which the 4.5 API proves from the node's logs; block documents are the
//! `<chain>-block-<version>` documents.
//!
//! The library is the per-block producer ([`BlockWorker`]); the `action-proto` binary and the
//! range service both drive it over chunks of a range, so they write the same bytes.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rs_abieos::{AbiHandle, Abieos};
use serde_json::{Map, Value};

use hyperion_ship::disk::decode_payload;
use hyperion_ship::provider::{ProtocolProvider, TimestampProfile};
use hyperion_ship::trace;
use hyperion_ship::wire_codec::WireDecoder;

mod occurrence;
pub mod wire_block;

/// Select the protocol for this process: a Wire protocol loads its codec profile, its SHiP ABI
/// and the native onblock ABI. Call once, before any worker runs.
pub fn init_protocol(protocol: &ProtocolProvider) -> Result<()> {
    if let ProtocolProvider::Wire(profile) = protocol {
        WIRE_PROFILE.set(profile.clone()).ok();
        WIRE_SHIP
            .set(serde_json::from_str(WIRE_SHIP_ABI).context("Wire SHiP ABI")?)
            .ok();
        let names = Abieos::new();
        let native: Value =
            serde_json::from_str(WIRE_NATIVE_ACTION_ABI).context("native Wire action ABI")?;
        WIRE_ONBLOCK
            .set((
                names.string_to_name("sysio")?,
                names.string_to_name("onblock")?,
                native,
            ))
            .ok();
    }
    Ok(())
}

/// Whether [`init_protocol`] selected Wire.
pub fn is_wire() -> bool {
    WIRE_PROFILE.get().is_some()
}

/// `protocol.id` of Hyperion's Wire documents: the provider package.
const WIRE_PROTOCOL_ID: &str = "@eosrio/hyperion-wire-plugin";

/// account (u64 name) -> versions sorted by the block the ABI took effect (valid_from).
/// Each version keeps its hex ABI and, when the catalog provides it, the decoded JSON ABI, parsed
/// once and shared by every worker. Wire ABIs can carry sections (`enums`, `protobuf_types`) the
/// binary parser rejects; the JSON form still loads.
pub type AbiIndex = HashMap<u64, Vec<(u32, String, Option<Arc<Value>>)>>;

/// Load an abi-index NDJSON file (or a Wire ABI history catalog's deployment records).
pub fn load_abi_index(path: &str) -> Result<AbiIndex> {
    abi_index_from(BufReader::new(
        File::open(path).with_context(|| format!("open {path}"))?,
    ))
}

/// [`load_abi_index`] over any reader, e.g. a verified catalog's records held in memory.
pub fn abi_index_from(f: impl BufRead) -> Result<AbiIndex> {
    let names = Abieos::new();
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
        // A Wire catalog records each deployment twice; the block-final row is the ABI in force.
        if v.get("phase")
            .and_then(|x| x.as_str())
            .is_some_and(|phase| phase != "block-final")
        {
            continue;
        }
        let (Some(acct), Some(block), Some(hex)) = (
            v.get("account").and_then(|x| x.as_str()),
            v.get("block")
                .or_else(|| v.get("blockNum"))
                .and_then(|x| x.as_u64()),
            v.get("abi_hex")
                .or_else(|| v.get("abiHex"))
                .and_then(|x| x.as_str()),
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
        let json = v
            .get("abi")
            .filter(|x| x.is_object())
            .map(|abi| Arc::new(abi.clone()));
        idx.entry(code)
            .or_default()
            .push((block as u32, hex.to_string(), json));
    }
    for versions in idx.values_mut() {
        versions.sort_by_key(|(b, _, _)| *b);
    }
    if skipped > 0 {
        eprintln!("[action-proto] skipped {skipped} malformed ABI-index line(s)");
    }
    Ok(idx)
}

/// Per-worker cache of parsed contract ABIs, backed by the shared (immutable) version index.
/// Most ABIs load into a fast `rs_abieos` handle. A Wire ABI that uses Wire-only features
/// (`slug_name`, ABI enums, protobuf types) is decoded by the selected Wire codec instead.
enum Loaded {
    Fast(AbiHandle),
    Wire {
        abi: Arc<Value>,
        actions: HashMap<u64, String>,
        decoder: WireDecoder,
    },
}

static WIRE_PROFILE: std::sync::OnceLock<hyperion_wire_protocol::codec::CodecProfile> =
    std::sync::OnceLock::new();

// Per-thread decoders for the two fixed ABIs (the native onblock ABI and Wire's SHiP ABI).
thread_local! {
    static STATIC_ABIS: std::cell::RefCell<WireDecoder> = std::cell::RefCell::new(WireDecoder::default());
}
const NATIVE_ABI_KEY: (u64, u64) = (0, 1);
const SHIP_ABI_KEY: (u64, u64) = (0, 2);

/// `sysio::onblock` carries the chain's own block header, which the `sysio` contract ABI describes
/// with the stale Antelope layout. Like the Wire plugin, decode it with the native onblock ABI.
/// Copied verbatim from hyperion-wire-plugin `crates/wire-protocol/src/codec/native_action_abi.json`.
const WIRE_NATIVE_ACTION_ABI: &str = include_str!("wire_native_action_abi.json");
static WIRE_ONBLOCK: std::sync::OnceLock<(u64, u64, Value)> = std::sync::OnceLock::new();

/// Wire's SHiP ABI as served by the node handshake; used to decode `signed_block` from the block log.
const WIRE_SHIP_ABI: &str = include_str!("wire_ship_abi.json");
static WIRE_SHIP: std::sync::OnceLock<Value> = std::sync::OnceLock::new();

/// Transaction id -> signatures for one Wire block, from its `signed_block` in the block log.
/// A transaction id is the sha256 of the serialized (decompressed) transaction.
fn wire_block_signatures(block: &[u8]) -> HashMap<[u8; 32], Vec<String>> {
    use sha2::{Digest, Sha256};
    let mut out = HashMap::new();
    let (Some(profile), Some(ship)) = (WIRE_PROFILE.get(), WIRE_SHIP.get()) else {
        return out;
    };
    let decoded = match STATIC_ABIS
        .with_borrow_mut(|d| d.decode(SHIP_ABI_KEY, ship, "signed_block", block, profile))
    {
        Ok(v) => v,
        Err(e) => {
            static REPORTED: std::sync::Once = std::sync::Once::new();
            REPORTED.call_once(|| {
                eprintln!(
                    "[action-proto] signed_block decode failed ({} bytes, first {}): {}",
                    block.len(),
                    hex::encode(&block[..block.len().min(24)]),
                    e.message()
                )
            });
            return out;
        }
    };

    for receipt in decoded
        .get("transactions")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
    {
        // Wire's receipt carries the packed transaction directly; an Antelope-style variant renders
        // as [type, value], where only packed transactions carry signatures.
        let packed = match receipt.get("trx") {
            Some(Value::Object(_)) => receipt.get("trx"),
            Some(Value::Array(v))
                if v.first().and_then(|t| t.as_str()) == Some("packed_transaction") =>
            {
                v.get(1)
            }
            _ => None,
        };
        let Some(packed) = packed else { continue };
        let Some(raw) = packed
            .get("packed_trx")
            .and_then(|x| x.as_str())
            .and_then(|h| hex::decode(h).ok())
        else {
            continue;
        };
        let serialized = match packed.get("compression").and_then(|c| c.as_u64()) {
            Some(0) | None => raw,
            Some(_) => {
                let mut inflated = Vec::new();
                if std::io::Read::read_to_end(
                    &mut flate2::read::ZlibDecoder::new(&raw[..]),
                    &mut inflated,
                )
                .is_err()
                {
                    continue;
                }
                inflated
            }
        };
        let id: [u8; 32] = Sha256::digest(&serialized).into();
        let signatures = packed
            .get("signatures")
            .and_then(|s| s.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        out.insert(id, signatures);
    }
    out
}

struct Registry<'a> {
    idx: &'a AbiIndex,
    handles: HashMap<(u64, u32), Option<Loaded>>,
}

impl<'a> Registry<'a> {
    fn new(idx: &'a AbiIndex) -> Self {
        Self {
            idx,
            handles: HashMap::new(),
        }
    }

    fn active(&mut self, code: u64, block: u32) -> Option<&mut Loaded> {
        let Registry { idx, handles } = self;
        let versions = idx.get(&code)?;
        let pos = versions.partition_point(|(vf, _, _)| *vf <= block);
        if pos == 0 {
            return None;
        }
        let valid_from = versions[pos - 1].0;
        handles
            .entry((code, valid_from))
            .or_insert_with(|| {
                let (_, hex, json) = &versions[pos - 1];
                // Wire renders values (hex case, Wire-only types, native structs) exactly as the
                // selected Wire codec does, so a Wire run never takes the rs_abieos fast path.
                let from_hex = if WIRE_PROFILE.get().is_some() {
                    Err(rs_abieos::AbieosError::SetAbi(
                        "Wire ABIs use the Wire codec".into(),
                    ))
                } else {
                    AbiHandle::from_hex(hex)
                };
                if let Ok(handle) = from_hex {
                    return Some(Loaded::Fast(handle));
                }
                if WIRE_PROFILE.get().is_some()
                    && let Some(abi) = json.clone()
                {
                    let names = Abieos::new();
                    let actions = abi
                        .get("actions")
                        .and_then(|a| a.as_array())
                        .map(|list| {
                            list.iter()
                                .filter_map(|a| {
                                    let name =
                                        names.string_to_name(a.get("name")?.as_str()?).ok()?;
                                    Some((name, a.get("type")?.as_str()?.to_owned()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    return Some(Loaded::Wire {
                        abi,
                        actions,
                        decoder: WireDecoder::default(),
                    });
                }
                // Report each unloadable ABI version once.
                eprintln!(
                    "[action-proto] ABI unusable code={code} valid_from={valid_from}: {:?}",
                    from_hex.err()
                );
                None
            })
            .as_mut()
    }
}

#[derive(Default)]
pub struct Stats {
    pub blocks: AtomicU64,
    pub txs: AtomicU64,                                // transaction_trace seen
    pub actions: AtomicU64, // action_trace processed (except==null, has receipt)
    pub decoded: AtomicU64, // act.data -> JSON ok
    pub raw: AtomicU64,     // act.data undecodable -> raw hex preserved (ds_error)
    pub no_abi: AtomicU64,  // (subset of raw) no ABI version for (code, block)
    pub recovered: AtomicU64, // decoded only after retrying against block-1's ABI
    pub docs: AtomicU64,    // grouped docs emitted
    pub block_docs: AtomicU64, // block documents emitted
    pub unreadable: AtomicU64, // trace entries that failed to inflate or parse (their actions are lost)
    pub stale: AtomicU64,      // bytes of superseded (forked) log entries skipped
    pub unreadable_blocks: std::sync::Mutex<Vec<u32>>, // the first few, for the summary
}

impl Stats {
    fn unreadable(&self, block_num: u32) {
        self.unreadable.fetch_add(1, Relaxed);
        let mut list = self.unreadable_blocks.lock().unwrap();
        if list.len() < 20 {
            list.push(block_num);
        }
    }
}

/// Why an act.data failed to decode at a given block.
enum Fail {
    NoAbi,
    NoType,
    Decode(String),
}

pub type Failures =
    std::sync::Mutex<std::collections::BTreeMap<(&'static str, String, String), (u64, String)>>;

fn record_failure(
    failures: &Failures,
    reason: &'static str,
    code: &str,
    action: &str,
    sample: &str,
) {
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
    if let (Some(profile), Some((sysio, onblock, native))) =
        (WIRE_PROFILE.get(), WIRE_ONBLOCK.get())
        && code == *sysio
        && action == *onblock
    {
        let value = STATIC_ABIS
            .with_borrow_mut(|d| {
                d.decode(
                    NATIVE_ABI_KEY,
                    native,
                    "wire_native_onblock_v1",
                    data,
                    profile,
                )
            })
            .map_err(|e| Fail::Decode(e.message().to_owned()))?;
        out.clear();
        out.push_str(&value.to_string());
        return Ok(());
    }
    let Some(loaded) = reg.active(code, block) else {
        return Err(Fail::NoAbi);
    };
    match loaded {
        Loaded::Fast(handle) => {
            let ty = match handle.type_for_action(action) {
                Some(t) => t.to_owned(),
                None => return Err(Fail::NoType),
            };
            handle
                .bin_to_json_into(&ty, data, out)
                .map_err(|e| Fail::Decode(format!("{e:?}")))
        }
        Loaded::Wire {
            abi,
            actions,
            decoder,
        } => {
            let Some(ty) = actions.get(&action) else {
                return Err(Fail::NoType);
            };
            let profile = WIRE_PROFILE
                .get()
                .expect("Wire ABIs load only with a Wire profile");
            // One decoder per loaded ABI version, so a constant key identifies it.
            let value = decoder
                .decode((0, 0), abi, ty, data, profile)
                .map_err(|e| Fail::Decode(e.message().to_owned()))?;
            out.clear();
            out.push_str(&value.to_string());
            Ok(())
        }
    }
}

/// Where a document goes and its `_id` there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DocKind {
    /// `<chain>-action-<version>-<partition>`; `_id` is the document's global sequence.
    Action(u64),
    /// `<chain>-block-<version>`; `_id` is the block number.
    Block,
}

/// One emitted document flowing over the channel to the sink (NDJSON writers or ES posters).
/// Carries `block_num` + its kind alongside the serialized `doc` so the ES posters can build the
/// `_bulk` meta line (`_index` + `_id`) WITHOUT re-parsing the doc JSON.
pub struct DocMsg {
    pub block_num: u32,
    pub kind: DocKind,
    pub doc: String,
}

/// A processed action_trace, held until the whole transaction is grouped. The doc fields are
/// pre-serialized into `body` (with a trailing comma); `receipt_json` is the receipt object for
/// `receipts[]` (without the hoisted act_digest/code_sequence/abi_sequence).
struct Proc {
    block_num: u32,
    global_sequence: u64,
    action_ordinal: u64,
    creator_action_ordinal: u64,
    /// Wire: the input action ordinal this occurrence belongs to.
    input_ordinal: u64,
    act_digest: String,
    code_sequence: u32,
    abi_sequence: u32,
    receipt_json: String,
    body: String,
    /// Schema-3 occurrence metadata, built for each input action when --chain-id is set.
    protocol: Option<String>,
}

/// What the Wire block walk writes, beyond the action documents.
#[derive(Clone, Debug, Default)]
pub struct WireDocs {
    /// With a chain id, action documents carry schema-3 occurrence metadata.
    pub chain_id: Option<String>,
    /// Write block documents.
    pub blocks: bool,
    /// `protocol.profileId` of block documents.
    pub profile_id: String,
}

/// A contiguous run of walked blocks, for checking that runs chain into each other.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub first: u32,
    pub first_previous: [u8; 32],
    pub last: u32,
    pub last_id: [u8; 32],
}

/// Extend `segment` with the next walked block, whose header must name the previous block's id.
pub fn chain_block(
    segment: &mut Option<Segment>,
    block_num: u32,
    block: &wire_block::Block,
) -> Result<()> {
    match segment {
        None => {
            *segment = Some(Segment {
                first: block_num,
                first_previous: block.previous,
                last: block_num,
                last_id: block.id,
            })
        }
        Some(run) => {
            if block_num != run.last + 1 || block.previous != run.last_id {
                bail!(
                    "block {block_num} does not follow block {}: its header names {} as its parent, but block {} is {}",
                    run.last,
                    hex::encode(block.previous),
                    run.last,
                    hex::encode(run.last_id)
                );
            }
            run.last = block_num;
            run.last_id = block.id;
        }
    }
    Ok(())
}

/// Hyperion 4.5's Wire block document: header fields, total CPU, the receipt count and each
/// receipt's per-input-action CPU vector. Wire receipts carry no status or NET, so a 4.5 Wire
/// block document has no `net_usage`, `schedule_version` or `new_producers`.
fn block_doc(
    block_num: u32,
    block: &wire_block::Block,
    timestamps: TimestampProfile,
    profile_id: &str,
) -> String {
    let mut s = String::with_capacity(360 + 24 * block.receipts.len());
    s.push_str("{\"@timestamp\":");
    push_json_str(&mut s, &timestamps.slot_to_iso(block.slot));
    s.push_str(",\"block_num\":");
    s.push_str(&block_num.to_string());
    s.push_str(",\"block_id\":\"");
    s.push_str(&hex::encode(block.id));
    s.push_str("\",\"prev_id\":\"");
    s.push_str(&hex::encode(block.previous));
    s.push_str("\",\"producer\":");
    push_json_str(&mut s, &trace::name_to_string(block.producer));
    let cpu: u64 = block
        .receipts
        .iter()
        .flat_map(|r| &r.cpu_usage_us)
        .map(|&c| u64::from(c))
        .sum();
    s.push_str(",\"cpu_usage\":");
    s.push_str(&cpu.to_string());
    s.push_str(",\"trx_count\":");
    s.push_str(&block.receipts.len().to_string());
    s.push_str(",\"protocol\":{\"id\":");
    push_json_str(&mut s, WIRE_PROTOCOL_ID);
    s.push_str(",\"profileId\":");
    push_json_str(&mut s, profile_id);
    s.push_str(",\"receipts\":[");
    for (i, receipt) in block.receipts.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"cpu_usage_us\":[");
        for (j, cpu) in receipt.cpu_usage_us.iter().enumerate() {
            if j > 0 {
                s.push(',');
            }
            s.push_str(&cpu.to_string());
        }
        s.push_str("]}");
    }
    s.push_str("]}}");
    s
}

/// Walk block `block_num`'s block-log entry. Its header is checked against `expected_id` when
/// the trace log names one.
pub fn walk_block(
    bytes: Option<&[u8]>,
    block_num: u32,
    expected_id: Option<&[u8]>,
) -> Result<wire_block::Block> {
    let bytes =
        bytes.with_context(|| format!("block {block_num} cannot be read from the block log"))?;
    let block = wire_block::parse(bytes, block_num)
        .map_err(|e| anyhow!("block {block_num} in the block log: {e}"))?;
    if let Some(expected) = expected_id
        && block.id[..] != *expected
    {
        bail!(
            "block {block_num}: the block log's header hashes to {}, but the trace log names {}",
            hex::encode(block.id),
            hex::encode(expected)
        );
    }
    Ok(block)
}

/// Where a producer puts documents: `None` only counts them.
pub type Sink<'s> = Option<&'s mut Vec<DocMsg>>;

fn send_block_doc(
    block_num: u32,
    block: &wire_block::Block,
    timestamps: TimestampProfile,
    profile_id: &str,
    sink: Sink<'_>,
    stats: &Stats,
) {
    stats.block_docs.fetch_add(1, Relaxed);
    if let Some(out) = sink {
        out.push(DocMsg {
            block_num,
            kind: DocKind::Block,
            doc: block_doc(block_num, block, timestamps, profile_id),
        });
    }
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
    v.as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| v.to_string())
}

/// Hyperion's system-contract handlers are declared for the literal `eosio` contract and dispatched
/// by `act.account`. On Wire, whose system contract is `sysio`, they never apply: Hyperion 4.5
/// stores no `@newaccount` or `@updateauth` there, and neither does this tool.
const HANDLER_CONTRACT: &str = "eosio";

/// Apply the matching `@`-field handler(s) for this action, mutating `doc` in place.
fn process_action_data(
    doc: &mut Map<String, Value>,
    account: &str,
    name: &str,
    memo_in_field: bool,
) {
    if name == "transfer" {
        h_transfer(doc, memo_in_field);
    }
    if account != HANDLER_CONTRACT {
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
        let Some(data) = act_data_mut(doc) else {
            return;
        };
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
        if memo_in_field && let Some(m) = data.remove("memo") {
            x.insert("memo".into(), m);
        }
        x
    };
    doc.insert("@transfer".into(), Value::Object(xfer));
}

fn h_newaccount(doc: &mut Map<String, Value>) {
    let na = {
        let Some(data) = act_data_mut(doc) else {
            return;
        };
        let name = if let Some(n) = data.get("newact").cloned() {
            n
        } else if let Some(n) = data.remove("name") {
            n
        } else {
            return;
        };
        let mut m = Map::new();
        m.insert(
            "active".into(),
            data.get("active").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "owner".into(),
            data.get("owner").cloned().unwrap_or(Value::Null),
        );
        m.insert("newact".into(), name);
        m
    };
    doc.insert("@newaccount".into(), Value::Object(na));
}

fn h_updateauth(doc: &mut Map<String, Value>) {
    let ua = {
        let Some(data) = act_data_mut(doc) else {
            return;
        };
        if let Some(auth) = data.get_mut("auth").and_then(|a| a.as_object_mut()) {
            for k in ["accounts", "keys", "waits"] {
                if auth
                    .get(k)
                    .and_then(|v| v.as_array())
                    .map(|a| a.is_empty())
                    .unwrap_or(false)
                {
                    auth.remove(k);
                }
            }
        }
        let mut m = Map::new();
        m.insert(
            "permission".into(),
            data.get("permission").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "parent".into(),
            data.get("parent").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "auth".into(),
            data.get("auth").cloned().unwrap_or(Value::Null),
        );
        m
    };
    doc.insert("@updateauth".into(), Value::Object(ua));
}

fn h_delegatebw(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let (cpu, net) =
            if data.contains_key("stake_cpu_quantity") && data.contains_key("stake_net_quantity") {
                (
                    asset_amount(&data["stake_cpu_quantity"]),
                    asset_amount(&data["stake_net_quantity"]),
                )
            } else {
                (0.0, 0.0)
            };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(cpu + net));
        m.insert("stake_cpu_quantity".into(), js_number(cpu));
        m.insert("stake_net_quantity".into(), js_number(net));
        m.insert(
            "from".into(),
            data.get("from").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "receiver".into(),
            data.get("receiver").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "transfer".into(),
            data.get("transfer").cloned().unwrap_or(Value::Null),
        );
        m
    };
    doc.insert("@delegatebw".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_undelegatebw(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let (cpu, net) = if data.contains_key("unstake_cpu_quantity")
            && data.contains_key("unstake_net_quantity")
        {
            (
                asset_amount(&data["unstake_cpu_quantity"]),
                asset_amount(&data["unstake_net_quantity"]),
            )
        } else {
            (0.0, 0.0)
        };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(cpu + net));
        m.insert("unstake_cpu_quantity".into(), js_number(cpu));
        m.insert("unstake_net_quantity".into(), js_number(net));
        m.insert(
            "from".into(),
            data.get("from").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "receiver".into(),
            data.get("receiver").cloned().unwrap_or(Value::Null),
        );
        m
    };
    doc.insert("@undelegatebw".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_buyram(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let mut m = Map::new();
        m.insert(
            "payer".into(),
            data.get("payer").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "receiver".into(),
            data.get("receiver").cloned().unwrap_or(Value::Null),
        );
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
            .and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
            })
            .unwrap_or(0);
        let mut m = Map::new();
        m.insert("bytes".into(), Value::from(bytes));
        m.insert(
            "payer".into(),
            data.get("payer").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "receiver".into(),
            data.get("receiver").cloned().unwrap_or(Value::Null),
        );
        m
    };
    doc.insert("@buyrambytes".into(), Value::Object(m));
    delete_act_data(doc);
}

fn h_buyrex(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let mut m = Map::new();
        m.insert(
            "amount".into(),
            js_number(data.get("amount").map(asset_amount).unwrap_or(0.0)),
        );
        m.insert(
            "from".into(),
            data.get("from").cloned().unwrap_or(Value::Null),
        );
        m
    };
    doc.insert("@buyrex".into(), Value::Object(m));
}

fn h_unstaketorex(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let (cpu, net) = if data.contains_key("from_cpu") && data.contains_key("from_net") {
            (
                asset_amount(&data["from_cpu"]),
                asset_amount(&data["from_net"]),
            )
        } else {
            (0.0, 0.0)
        };
        let mut m = Map::new();
        m.insert("amount".into(), js_number(cpu + net));
        m.insert(
            "owner".into(),
            data.get("owner").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "receiver".into(),
            data.get("receiver").cloned().unwrap_or(Value::Null),
        );
        m
    };
    doc.insert("@unstaketorex".into(), Value::Object(m));
}

fn h_voteproducer(doc: &mut Map<String, Value>) {
    let m = {
        let Some(data) = act_data(doc) else { return };
        let mut m = Map::new();
        m.insert(
            "proxy".into(),
            data.get("proxy").cloned().unwrap_or(Value::Null),
        );
        m.insert(
            "producers".into(),
            data.get("producers").cloned().unwrap_or(Value::Null),
        );
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
        || (account == HANDLER_CONTRACT
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
///
/// Wire (`certified`) follows the 4.5 `certified-extension` mode: one document per input action,
/// which is the member whose ordinal is the input. It keeps its own counters, takes the first
/// member's `act_digest`, and lists every member's receipt in order. 4.5 falls back to the first
/// member when the input action was filtered out; this tool processes every committed action, so
/// a missing input action is refused instead.
fn finalize_and_emit(
    procs: Vec<Proc>,
    certified: bool,
    mut sink: Sink<'_>,
    stats: &Stats,
) -> Result<()> {
    if procs.is_empty() {
        return Ok(());
    }
    if certified {
        let mut order: Vec<u64> = Vec::new();
        let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
        for (i, p) in procs.iter().enumerate() {
            groups
                .entry(p.input_ordinal)
                .or_insert_with(|| {
                    order.push(p.input_ordinal);
                    Vec::new()
                })
                .push(i);
        }
        for input in order {
            let members = &groups[&input];
            let Some(representative) = members
                .iter()
                .copied()
                .find(|&i| procs[i].action_ordinal == input)
            else {
                bail!("input action {input} of a grouped occurrence was not processed");
            };
            let receipts: Vec<&str> = members
                .iter()
                .map(|&i| procs[i].receipt_json.as_str())
                .collect();
            emit_doc(
                &procs[representative],
                &procs[members[0]].act_digest,
                &receipts,
                sink.as_deref_mut(),
                stats,
            );
        }
        return Ok(());
    }
    if procs.len() == 1 {
        let p = &procs[0];
        emit_doc(p, &p.act_digest, &[p.receipt_json.as_str()], sink, stats);
        return Ok(());
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
        emit_doc(
            p,
            &p.act_digest,
            &group_receipts[key],
            sink.as_deref_mut(),
            stats,
        );
    }
    Ok(())
}

/// Assemble one grouped action doc:
/// `{ <body> "act_digest":.., "code_sequence":.., "abi_sequence":.., "receipts":[..], "protocol":.. }`.
fn emit_doc(p: &Proc, act_digest: &str, receipts: &[&str], sink: Sink<'_>, stats: &Stats) {
    stats.docs.fetch_add(1, Relaxed);
    let Some(out) = sink else { return };
    let rlen: usize = receipts.iter().map(|r| r.len() + 1).sum();
    let plen = p.protocol.as_ref().map_or(0, |m| m.len() + 12);
    let mut s = String::with_capacity(p.body.len() + act_digest.len() + rlen + plen + 96);
    s.push('{');
    s.push_str(&p.body); // ends with a trailing comma
    s.push_str("\"act_digest\":");
    push_json_str(&mut s, act_digest);
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
    s.push(']');
    if let Some(metadata) = &p.protocol {
        s.push_str(",\"protocol\":");
        s.push_str(metadata);
    }
    s.push('}');
    out.push(DocMsg {
        block_num: p.block_num,
        kind: DocKind::Action(p.global_sequence),
        doc: s,
    });
}

/// The block-log input of one block.
#[derive(Clone, Copy)]
pub enum BlockInput<'b> {
    /// No block log: documents carry no header fields.
    None,
    /// Antelope: the header fields only.
    Header(Option<(u64, u32)>),
    /// The complete `signed_block` (Wire walks it).
    Bytes(&'b [u8]),
    /// The block log does not hold this block.
    Missing,
}

impl BlockInput<'_> {
    fn header(&self) -> Option<(u64, u32)> {
        match self {
            BlockInput::Header(h) => *h,
            BlockInput::Bytes(b) => Some((
                u64::from_le_bytes(b.get(4..12)?.try_into().ok()?),
                u32::from_le_bytes(b.get(0..4)?.try_into().ok()?),
            )),
            _ => None,
        }
    }
}

/// What every worker of a run shares.
#[derive(Clone, Copy)]
pub struct Producer<'a> {
    pub abi_index: &'a AbiIndex,
    pub index_transfer_memo: bool,
    pub metadata_only: bool,
    pub timestamps: TimestampProfile,
    pub docs: &'a WireDocs,
    pub stats: &'a Stats,
    pub failures: &'a Failures,
}

/// One worker's decoder state: parsed ABIs and buffers, reused across blocks.
pub struct BlockWorker<'a> {
    producer: Producer<'a>,
    reg: Registry<'a>,
    data_buf: String,
    profile: bool,
    d_l1: Duration,
    d_act: Duration,
    d_l2: Duration,
    /// The lowest and highest receipt global sequence seen since the last take.
    sequences: Option<(u64, u64)>,
}

impl<'a> BlockWorker<'a> {
    pub fn new(producer: Producer<'a>) -> Self {
        Self {
            producer,
            reg: Registry::new(producer.abi_index),
            data_buf: String::new(),
            profile: std::env::var("ACTION_PROFILE").is_ok(),
            d_l1: Duration::ZERO,
            d_act: Duration::ZERO,
            d_l2: Duration::ZERO,
            sequences: None,
        }
    }

    /// The lowest and highest committed receipt global sequence of the blocks processed since
    /// the last call.
    pub fn take_sequences(&mut self) -> Option<(u64, u64)> {
        self.sequences.take()
    }

    /// A block before the trace log's first entry: its header is walked and chained, and it gets
    /// a block document. Its id rests on its header; the first traced block's header then names
    /// the last of them.
    pub fn untraced(
        &mut self,
        block_num: u32,
        bytes: Option<&[u8]>,
        segment: &mut Option<Segment>,
        sink: Sink<'_>,
    ) -> Result<wire_block::Block> {
        let Producer {
            docs,
            timestamps,
            stats,
            ..
        } = self.producer;
        let block = walk_block(bytes, block_num, None)?;
        if block_num == 1 && block.previous != [0; 32] {
            bail!("block 1 must name the zero block as its parent");
        }
        chain_block(segment, block_num, &block)?;
        if docs.blocks {
            send_block_doc(block_num, &block, timestamps, &docs.profile_id, sink, stats);
        }
        Ok(block)
    }

    /// One traced block: its canonical trace-log entry (`id` from the entry header, `payload` its
    /// payload) and its block-log input. Wire walks the block, checks it against the trace log and
    /// chains it into `segment`. Returns the walked block.
    pub fn traced(
        &mut self,
        block_num: u32,
        id: &[u8; 32],
        payload: &[u8],
        block: BlockInput<'_>,
        segment: &mut Option<Segment>,
        mut sink: Sink<'_>,
    ) -> Result<Option<wire_block::Block>> {
        let Producer {
            index_transfer_memo,
            metadata_only,
            timestamps,
            docs,
            stats,
            failures,
            ..
        } = self.producer;
        let wire = WIRE_PROFILE.get().is_some();
        let block_id = hex::encode(id);
        stats.blocks.fetch_add(1, Relaxed);

        // Wire: one walk of the block-log entry, checked against the id the trace log names and
        // chained to the previous block, gives the header fields, the block document and the
        // transaction ids. Antelope reads only the header prefix.
        let walked = match (wire, block) {
            (true, BlockInput::Bytes(bytes)) => {
                let block = walk_block(Some(bytes), block_num, Some(id))?;
                chain_block(segment, block_num, &block)?;
                if docs.blocks {
                    send_block_doc(
                        block_num,
                        &block,
                        timestamps,
                        &docs.profile_id,
                        sink.as_deref_mut(),
                        stats,
                    );
                }
                Some((block, bytes))
            }
            (true, BlockInput::Missing) => {
                walk_block(None, block_num, Some(id))?;
                None
            }
            _ => None,
        };
        let (block_producer, block_ts) = match &walked {
            Some((block, _)) => (
                Some(trace::name_to_string(block.producer)),
                Some(timestamps.slot_to_iso(block.slot)),
            ),
            None => match block.header() {
                Some((prod, slot)) => (
                    Some(trace::name_to_string(prod)),
                    Some(timestamps.slot_to_iso(slot)),
                ),
                None => (None, None),
            },
        };
        // Schema-3 metadata up to `input`: one disk selection per block.
        let metadata_prefix = match (&docs.chain_id, &walked) {
            (Some(chain_id), Some((block, _))) => Some(occurrence::block_prefix(
                chain_id,
                block_num,
                &block_id,
                &hex::encode(block.previous),
            )),
            _ => None,
        };

        let inflated = match decode_payload(payload) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => {
                // An entry with no traces, which a block with transactions cannot have.
                if let Some((block, _)) = &walked
                    && !block.receipts.is_empty()
                {
                    bail!(
                        "block {block_num} has {} transaction receipts but no traces",
                        block.receipts.len()
                    );
                }
                return Ok(walked.map(|(block, _)| block));
            }
            Err(_) => {
                stats.unreadable(block_num);
                return Ok(walked.map(|(block, _)| block));
            }
        };
        // L1 (hand-walk): parse the block's transaction_trace[] binary directly — no full-JSON
        // materialization, no serde re-parse; act.data stays a raw byte range.
        let l1_t = if self.profile {
            Some(Instant::now())
        } else {
            None
        };
        let Some(txs) = trace::parse_block(&inflated) else {
            stats.unreadable(block_num);
            return Ok(walked.map(|(block, _)| block));
        };
        // Every transaction the block includes has a trace in it; beyond those, the trace log
        // holds only the implicit onblock. Only blocks with receipts carry signatures.
        let signatures = match &walked {
            Some((block, bytes)) if !block.receipts.is_empty() => {
                if let Some(receipt) = block
                    .receipts
                    .iter()
                    .find(|r| !txs.iter().any(|tx| tx.id == r.trx_id))
                {
                    bail!(
                        "block {block_num} includes transaction {} but its traces do not",
                        hex::encode(receipt.trx_id)
                    );
                }
                wire_block_signatures(bytes)
            }
            _ => HashMap::new(),
        };
        if let Some(t) = l1_t {
            self.d_l1 += t.elapsed();
        }
        let a_t = if self.profile {
            Some(Instant::now())
        } else {
            None
        };
        for tx in &txs {
            stats.txs.fetch_add(1, Relaxed);
            let trx_id = hex::encode(tx.id);
            // Wire: every action is committed with a receipt, and each one's input origin is
            // checked against its complete ancestry before anything of the transaction is written.
            let origins = if wire {
                if let Some(act) = tx.actions.iter().find(|a| {
                    a.except || a.receipt.as_ref().is_none_or(|r| r.receiver != a.receiver)
                }) {
                    bail!(
                        "block {block_num} transaction {trx_id}: action {} has no committed receipt for its receiver",
                        act.action_ordinal
                    );
                }
                Some(
                    occurrence::input_origins(&tx.actions, &inflated)
                        .map_err(|e| anyhow!("block {block_num} transaction {trx_id}: {e}"))?,
                )
            } else {
                None
            };
            let mut procs: Vec<Proc> = Vec::new();
            let mut usage_included = false;
            for (position, act) in tx.actions.iter().enumerate() {
                // Hyperion only indexes actions with except===null and a receipt.
                if act.except {
                    continue;
                }
                let Some(receipt) = &act.receipt else {
                    continue;
                };
                let gs = receipt.global_sequence;
                self.sequences = Some(match self.sequences {
                    Some((low, high)) => (low.min(gs), high.max(gs)),
                    None => (gs, gs),
                });
                let input_ordinal = origins.as_ref().map_or(0, |o| o[position]);
                let code = act.account;
                let action_u64 = act.name;
                let account = trace::name_to_string(code);
                let name = trace::name_to_string(action_u64);
                let data_bytes = &inflated[act.data.0..act.data.0 + act.data.1];
                stats.actions.fetch_add(1, Relaxed);

                // L2: decode act.data (raw bytes) against the contract ABI active at the block;
                // retry at block-1 (same-block setabi boundary); else preserve raw hex (ds_error).
                let l2_t = if self.profile {
                    Some(Instant::now())
                } else {
                    None
                };
                let mut decoded = false;
                {
                    let r1 = decode_action(
                        &mut self.reg,
                        &mut self.data_buf,
                        code,
                        action_u64,
                        data_bytes,
                        block_num,
                    );
                    let outcome = match r1 {
                        Ok(()) => Ok(false),
                        Err(first) => {
                            let retry = if !matches!(first, Fail::NoAbi) && block_num > 1 {
                                decode_action(
                                    &mut self.reg,
                                    &mut self.data_buf,
                                    code,
                                    action_u64,
                                    data_bytes,
                                    block_num - 1,
                                )
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
                    self.d_l2 += t.elapsed();
                }

                // act.data representation + computed @-fields. Non-@-handled actions splice the
                // raw decoded JSON verbatim (no parse/reserialize); @-handled ones parse, run the
                // handler on a tiny temp map, then re-serialize the (possibly trimmed) data.
                let mut at_fragment = String::new();
                let data_part: Option<String>;
                if decoded {
                    if is_at_handled(&account, &name) {
                        if let Ok(data_val) = serde_json::from_str::<Value>(&self.data_buf) {
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
                            data_part = Some(self.data_buf.clone());
                        }
                    } else {
                        data_part = Some(self.data_buf.clone());
                    }
                    stats.decoded.fetch_add(1, Relaxed);
                } else {
                    let mut hx = String::new();
                    push_json_str(&mut hx, &hex::encode(data_bytes));
                    data_part = Some(hx);
                    stats.raw.fetch_add(1, Relaxed);
                }
                // Cold tier: drop act.data (served on-demand by archive-server); the @-fields and
                // all metadata stay, so the doc is fully searchable and block_num + global_sequence
                // let the API fetch the payload from the archive.
                let data_part = if metadata_only { None } else { data_part };

                // Build the doc body directly as JSON; cleanActionTrace pruning = conditional
                // writes. Ends with a trailing comma. act_digest/code/abi/receipts are appended
                // by emit_doc after grouping.
                let mut body =
                    String::with_capacity(data_part.as_ref().map_or(0, |d| d.len()) + 256);
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
                if wire {
                    body.push_str(",\"global_sequence\":\"");
                    body.push_str(&receipt.global_sequence.to_string());
                    body.push('"');
                } else {
                    body.push_str(",\"global_sequence\":");
                    body.push_str(&receipt.global_sequence.to_string());
                }
                body.push_str(",\"action_ordinal\":");
                body.push_str(&act.action_ordinal.to_string());
                body.push_str(",\"creator_action_ordinal\":");
                body.push_str(&act.creator_action_ordinal.to_string());
                body.push(',');
                if wire {
                    body.push_str(if act.context_free {
                        "\"context_free\":1,"
                    } else {
                        "\"context_free\":0,"
                    });
                } else if act.context_free {
                    body.push_str("\"context_free\":true,");
                }
                if act.elapsed != 0 {
                    body.push_str("\"elapsed\":\"");
                    body.push_str(&act.elapsed.to_string());
                    body.push_str("\",");
                }
                if let Some(rv) = act.return_value
                    && rv.1 > 0
                {
                    // Hyperion renders return values as abieos does (uppercase) on Antelope, and as
                    // the Wire codec does (lowercase) on Wire.
                    let bytes = &inflated[rv.0..rv.0 + rv.1];
                    body.push_str("\"return_value\":");
                    push_json_str(
                        &mut body,
                        &if wire {
                            hex::encode(bytes)
                        } else {
                            hex::encode_upper(bytes)
                        },
                    );
                    body.push(',');
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
                    if let Some(list) = signatures.get(&tx.id).filter(|l| !l.is_empty()) {
                        body.push_str("\"signatures\":");
                        body.push_str(&serde_json::to_string(list).unwrap_or_else(|_| "[]".into()));
                        body.push(',');
                    }
                    if wire && tx.actions.len() > 1 {
                        body.push_str("\"inline_count\":");
                        body.push_str(&(tx.actions.len() - 1).to_string());
                        body.push_str(",\"inline_filtered\":false,");
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
                if wire {
                    receipt_json.push_str("],\"code_sequence\":");
                    receipt_json.push_str(&receipt.code_sequence.to_string());
                    receipt_json.push_str(",\"abi_sequence\":");
                    receipt_json.push_str(&receipt.abi_sequence.to_string());
                    receipt_json.push('}');
                } else {
                    receipt_json.push_str("]}");
                }

                // Only an input action becomes a document, so only it needs metadata.
                let protocol = match &metadata_prefix {
                    Some(prefix) if input_ordinal == act.action_ordinal => Some(
                        occurrence::metadata(prefix, &trx_id, act, data_bytes, input_ordinal)
                            .context("occurrence metadata needs a receipt")?,
                    ),
                    _ => None,
                };
                procs.push(Proc {
                    block_num,
                    global_sequence: receipt.global_sequence,
                    action_ordinal: act.action_ordinal as u64,
                    creator_action_ordinal: act.creator_action_ordinal as u64,
                    input_ordinal: u64::from(input_ordinal),
                    act_digest: if wire {
                        hex::encode(receipt.act_digest)
                    } else {
                        hex::encode_upper(receipt.act_digest)
                    },
                    code_sequence: receipt.code_sequence,
                    abi_sequence: receipt.abi_sequence,
                    receipt_json,
                    body,
                    protocol,
                });
            }
            finalize_and_emit(procs, wire, sink.as_deref_mut(), stats)
                .with_context(|| format!("block {block_num} transaction {trx_id}"))?;
        }
        if let Some(t) = a_t {
            self.d_act += t.elapsed();
        }
        Ok(walked.map(|(block, _)| block))
    }
}

impl Drop for BlockWorker<'_> {
    /// With `ACTION_PROFILE` set, print where this worker's time went.
    fn drop(&mut self) {
        if self.profile {
            eprintln!(
                "[action-proto][profile] worker: l1_handwalk={:.2}s actions_build={:.2}s (of which l2_decode+parse={:.2}s)",
                self.d_l1.as_secs_f64(),
                self.d_act.as_secs_f64(),
                self.d_l2.as_secs_f64()
            );
        }
    }
}

/// Documents in flight between the decoders and the sink. Decoding outruns every sink, so the
/// channel applies backpressure instead of holding the backlog in memory.
pub const DOC_BACKLOG: usize = 1 << 15;

/// How long an ES poster waits for its batch to fill before posting what it has.
const BULK_LINGER: Duration = Duration::from_millis(50);

/// Shared counters for the direct-ES `_bulk` sink (Arc'd across all poster threads).
#[derive(Default)]
pub struct EsStats {
    pub docs: AtomicU64,     // docs accepted into a posted _bulk body
    pub bytes: AtomicU64,    // total _bulk request body bytes
    pub requests: AtomicU64, // _bulk requests issued
    pub errors: AtomicU64, // bulk responses whose body contained `"errors":true` (or transport errors)
}

/// Config the ES posters need to build each `_bulk` meta line. (Cheap to clone — only the URL +
/// chain/version are owned strings.)
#[derive(Clone, Debug)]
pub struct EsCfg {
    pub url: String, // `<es>/_bulk`
    pub chain: String,
    pub version: String,
    pub partition_size: u64,
    pub batch: usize,
}

/// `<chain>-action-<version>-<partition>`, partition = ceil(block_num / partition_size) zero-padded
/// to 6 digits (>= 1) — mirrors Hyperion's index naming (see bench/scripts/bulk-load.py).
fn action_index(cfg: &EsCfg, block_num: u32) -> String {
    let partition = (block_num as u64).div_ceil(cfg.partition_size).max(1);
    format!("{}-action-{}-{:06}", cfg.chain, cfg.version, partition)
}

/// Append `msg`'s `_bulk` index line and the document itself, each with its newline.
pub fn push_bulk(body: &mut String, cfg: &EsCfg, m: &DocMsg) {
    body.push_str("{\"index\":{\"_index\":\"");
    match m.kind {
        DocKind::Action(global_sequence) => {
            body.push_str(&action_index(cfg, m.block_num));
            body.push_str("\",\"_id\":\"");
            body.push_str(&global_sequence.to_string());
        }
        DocKind::Block => {
            body.push_str(&cfg.chain);
            body.push_str("-block-");
            body.push_str(&cfg.version);
            body.push_str("\",\"_id\":\"");
            body.push_str(&m.block_num.to_string());
        }
    }
    body.push_str("\"}}\n");
    body.push_str(&m.doc);
    body.push('\n');
}

/// One ES poster thread: pull up to `cfg.batch` docs from the shared channel (locking only while
/// draining, never while POSTing), accumulate them into an `application/x-ndjson` `_bulk` body
/// (`{"index":{"_index":..,"_id":..}}\n<doc>\n` per doc), POST to `<es>/_bulk`, and fold the
/// outcome into the shared `EsStats`. An action `_id` is the doc's `global_sequence`; a block
/// `_id` is its block number. The first failed response is printed once.
pub fn es_poster(rx: &std::sync::Mutex<mpsc::Receiver<DocMsg>>, cfg: &EsCfg, stats: &EsStats) {
    static REPORTED: std::sync::Once = std::sync::Once::new();
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
            // Linger briefly so each request carries a full batch instead of whatever is queued.
            let deadline = Instant::now() + BULK_LINGER;
            while batch.len() < cfg.batch {
                match guard.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
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
            push_bulk(&mut body, cfg, m);
        }
        let n = batch.len() as u64;
        let nbytes = body.len() as u64;

        let failure = match minreq::post(cfg.url.as_str())
            .with_header("Content-Type", "application/x-ndjson")
            .with_body(body.as_str())
            .send()
        {
            // Cheap error check: the bulk response sets `"errors":true` iff any item failed.
            Ok(resp) => match resp.as_str() {
                Ok(text)
                    if (200..300).contains(&resp.status_code)
                        && !text.contains("\"errors\":true") =>
                {
                    None
                }
                Ok(text) => Some(format!(
                    "HTTP {}: {}",
                    resp.status_code,
                    text.chars().take(2000).collect::<String>()
                )),
                Err(e) => Some(format!("HTTP {}: unreadable body: {e}", resp.status_code)),
            },
            Err(e) => Some(format!("transport: {e}")),
        };
        if let Some(reason) = failure {
            stats.errors.fetch_add(1, Relaxed);
            REPORTED.call_once(|| eprintln!("[action-proto] first failed _bulk request: {reason}"));
        }
        stats.docs.fetch_add(n, Relaxed);
        stats.bytes.fetch_add(nbytes, Relaxed);
        stats.requests.fetch_add(1, Relaxed);
    }
}
