//! Independent, sparse Wire ABI deployment history from frozen, index-addressed logs.
//!
//! A manifest is published only after every source block, output and identity check succeeds.
//! Its block membership sidecar is 32 bytes per block; no per-block database records are needed.
//!
//! Reader threads each take a contiguous chunk of blocks. For every block they inflate the
//! chain-state and trace entries, walk their bytes ([`crate::wire_walk`]) and keep only what can
//! change the history: deployments, ABI sequence observations that differ from the previous one
//! for the account, and account rows that differ from the account's previous row in the chunk or
//! that a deployment in the same block must match. One fold applies those facts in block order,
//! with the same checks and the same records as a block-by-block scan.
//!
//! A scan can extend a sealed catalog instead of starting at genesis. It verifies the base
//! catalog's members, rebuilds the state its records describe, checks that the logs still hold
//! the base's last block, scans only the blocks after it and writes the base members followed by
//! the new records and block ids.
//!
//! A [`Sealer`] extends a chain incrementally instead: it keeps the fold's state between seals,
//! reads a running node's logs as slabs up to an irreversible block ([`LiveLogs`]), and writes
//! [`SEGMENT_FORMAT`] segments that hold only the blocks they add. A chain's records and block
//! ids, concatenated link by link, are what a complete scan to the same end writes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc;
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::binio::{FcReader, FileCache};
use crate::catalog::{Catalog, discover, ship_catalog_within};
use crate::provider::{ProtocolProvider, WIRE_SOURCE_REVISION};
use crate::shiplog::{LogKind, ShipLog};
use crate::slab::ShipFile;
use crate::wire_walk::{self, name_string, name_value};

pub const FORMAT: &str = "hyperion-wire-abi-history-v1";
const MAX_PAYLOAD: u64 = 16 * 1024 * 1024;
/// Blocks per work unit: enough to amortize the hand-off, small enough to balance the threads.
const CHUNK: u32 = 2048;
const SYSIO: u64 = name_value("sysio");
const SETABI: u64 = name_value("setabi");

pub struct ScanOptions {
    pub directory: PathBuf,
    pub schema: String,
    pub chain_id: String,
    /// 1 for a complete scan; the base catalog's end + 1 for an extension.
    pub start: u32,
    pub end: u32,
    pub irreversible_block_id: String,
    pub output: PathBuf,
    pub provider: ProtocolProvider,
    /// Reader threads; 0 uses the available parallelism.
    pub threads: usize,
    /// Extend this sealed catalog instead of scanning from genesis.
    pub base: Option<BaseCatalog>,
}

/// A sealed catalog to extend, pinned by its manifest's SHA-256.
pub struct BaseCatalog {
    pub manifest: PathBuf,
    pub manifest_sha256: String,
}

#[derive(Clone, PartialEq, Eq)]
struct AccountState {
    present: bool,
    abi: Vec<u8>,
}

struct Deployment {
    account: String,
    abi: Vec<u8>,
    transaction: String,
    ordinal: u32,
    sequence: u64,
}
enum Event {
    Deploy(Deployment),
    /// A primary receipt's ABI sequence, which must equal the account's deployment count.
    Revision {
        account: String,
        abi_sequence: u64,
    },
}
struct Row {
    account: String,
    state: AccountState,
    ordinal: u32,
}
struct BlockFacts {
    block: u32,
    events: Vec<Event>,
    rows: Vec<Row>,
}
/// What a reader thread found in blocks `start..start + ids.len()`.
struct Chunk {
    start: u32,
    ids: Vec<[u8; 32]>,
    /// Only blocks with events or rows the fold must see.
    blocks: Vec<BlockFacts>,
    /// The chunk's first committed receipt and its block, and its last receipt.
    first_sequence: Option<(u64, u32)>,
    last_sequence: Option<u64>,
}

fn hash(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}
fn hex_id(value: &str) -> Result<[u8; 32]> {
    ensure!(
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid lowercase chain/block id"
    );
    hex::decode(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid id length"))
}
fn suffix(path: &Path, tail: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(tail);
    name.into()
}
fn fresh(path: &Path) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create fresh output {}", path.display()))
}
fn publish(from: &Path, to: &Path) -> Result<()> {
    // Unlike rename on Unix, hard_link cannot overwrite another run's completed artifact.
    fs::hard_link(from, to).with_context(|| format!("publish fresh output {}", to.display()))?;
    fs::remove_file(from)?;
    Ok(())
}
fn file_hash(path: &Path) -> Result<(u64, String)> {
    let mut input = BufReader::new(File::open(path)?);
    let mut state = Sha256::new();
    let mut bytes = 0;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        state.update(&buf[..n]);
        bytes += n as u64;
    }
    Ok((bytes, hex::encode(state.finalize())))
}
fn basename(path: &Path) -> Result<&str> {
    path.file_name()
        .and_then(|v| v.to_str())
        .context("output filename must be UTF-8")
}

fn open_catalog(
    paths: &[crate::catalog::PathPair],
    kind: LogKind,
    cache: &Arc<FileCache>,
    window: (u32, u32),
) -> Result<Catalog<ShipLog>> {
    let (catalog, findings) = ship_catalog_within(paths, kind, cache, Some(window))?;
    for finding in findings {
        eprintln!("[wire-disk] {finding}");
    }
    catalog.with_context(|| format!("missing {} logs", kind.label()))
}
fn payload(catalog: &Catalog<ShipLog>, block: u32, out: &mut Vec<u8>) -> Result<[u8; 32]> {
    let log = catalog
        .get(block)
        .with_context(|| format!("missing source block {block}"))?;
    let header = log.header(block)?;
    log.read_inline(&header, MAX_PAYLOAD, out)?;
    Ok(header.id)
}

/// The target account (value and rendering) and ABI bytes of `setabi` action data.
fn setabi(bytes: &[u8]) -> Result<(u64, String, Vec<u8>)> {
    let mut cursor = FcReader::new(bytes);
    let account = cursor.u64()?;
    let n = cursor.varuint32()? as usize;
    let abi = cursor.bytes(n)?.to_vec();
    ensure!(cursor.remaining() == 0, "trailing setabi bytes");
    let name = rs_abieos::Abieos::new()
        .name_to_string(account)
        .map_err(|e| anyhow::anyhow!("invalid setabi account: {e:?}"))?;
    Ok((account, name, abi))
}

/// A committed receipt in the order the history applies it.
struct Committed {
    account: u64,
    sequence: u64,
    abi_sequence: Option<u64>,
    ordinal: u32,
    transaction: [u8; 32],
    deployment: Option<(u64, String, Vec<u8>)>,
}

/// A block's committed receipts in global sequence order, refusing transaction effects the
/// history cannot prove complete.
fn committed(
    txs: &[wire_walk::Transaction<'_>],
    actions: &[wire_walk::Action<'_>],
    out: &mut Vec<Committed>,
) -> Result<()> {
    out.clear();
    for tx in txs {
        // Match the native observer's proven transaction scope. In particular,
        // soft_fail may contain committed onerror effects: silently skipping it
        // would manufacture complete coverage while losing ABI deployments.
        ensure!(
            tx.status == 0,
            "unsupported_feature: Wire disk history requires executed transaction traces (soft_fail/onerror and other statuses require qualification)"
        );
        ensure!(
            tx.scheduled == 0 && !tx.failed_dtrx_trace,
            "unsupported_feature: deferred Wire transaction trace requires qualification"
        );
        // A `partial_transaction_v0` (the walk refuses any other arm) is transaction header and
        // signature metadata, not truncated action coverage.
        ensure!(
            !tx.except && !tx.error_code,
            "executed transaction has failure metadata"
        );
        let transaction: [u8; 32] = tx.id.try_into()?;
        for action in &actions[tx.actions.clone()] {
            let Some(receipt) = action.receipt else {
                ensure!(
                    action.account != SYSIO || action.name != SETABI,
                    "setabi action lacks a committed receipt"
                );
                continue;
            };
            let primary = action.receiver == action.account && receipt.receiver == action.account;
            // A notification uses its original action's ABI; only primary receipts check its revision.
            if !primary {
                out.push(Committed {
                    account: action.account,
                    sequence: receipt.global_sequence,
                    abi_sequence: None,
                    ordinal: 0,
                    transaction,
                    deployment: None,
                });
                continue;
            }
            ensure!(
                !action.except && !action.error_code,
                "receipted action has failure metadata"
            );
            ensure!(action.ordinal > 0, "zero action ordinal");
            let deployment = if action.account == SYSIO && action.name == SETABI {
                Some(setabi(action.data)?)
            } else {
                None
            };
            out.push(Committed {
                account: action.account,
                sequence: receipt.global_sequence,
                abi_sequence: Some(u64::from(receipt.abi_sequence)),
                ordinal: action.ordinal,
                transaction,
                deployment,
            });
        }
    }
    out.sort_by_key(|a| a.sequence);
    ensure!(
        out.windows(2).all(|w| w[0].sequence < w[1].sequence),
        "duplicate or unordered global action receipts"
    );
    Ok(())
}

/// Receives one block: its number, id, inflated chain-state payload and (from the trace log's
/// first block) inflated trace payload.
type BlockVisit<'a> = dyn FnMut(u32, [u8; 32], &[u8], Option<&[u8]>) -> Result<()> + 'a;

/// Sees a segment's first block and block ids before the segment is published.
pub type IdCheck<'a> = dyn FnMut(u32, &[[u8; 32]]) -> Result<()> + 'a;

/// Where a scan reads its blocks.
trait ChunkSource: Sync {
    /// Call `f(block, id, state, trace)` for every block of `start..=end`, in order, with the
    /// inflated chain-state payload and (from the trace start) the inflated trace payload.
    fn read(&self, start: u32, end: u32, f: &mut BlockVisit<'_>) -> Result<()>;
}

/// A frozen, validated catalog of log files.
struct Sources<'a> {
    states: &'a Catalog<ShipLog>,
    traces: &'a Catalog<ShipLog>,
    trace_start: u32,
}

impl ChunkSource for Sources<'_> {
    fn read(&self, start: u32, end: u32, f: &mut BlockVisit<'_>) -> Result<()> {
        let (mut state, mut trace) = (Vec::new(), Vec::new());
        for block in start..=end {
            let id = payload(self.states, block, &mut state)?;
            if block >= self.trace_start {
                let trace_id = payload(self.traces, block, &mut trace)?;
                ensure!(
                    trace_id == id,
                    "trace/state block identity mismatch at {block}"
                );
                f(block, id, &state, Some(&trace))?;
            } else {
                f(block, id, &state, None)?;
            }
        }
        Ok(())
    }
}

/// A running node's logs, read as slabs up to an irreversible block: entries at or below it are
/// never rewritten, so the growing files need no frozen view.
pub struct LiveLogs {
    states: ShipFile,
    traces: ShipFile,
}

impl LiveLogs {
    /// The `chain_state_history` and `trace_history` logs in `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        Ok(Self {
            states: ShipFile::open(dir, "chain_state_history")?,
            traces: ShipFile::open(dir, "trace_history")?,
        })
    }

    /// The last block both logs index now.
    pub fn last_block(&self) -> Result<Option<u32>> {
        Ok(
            match (self.states.last_block()?, self.traces.last_block()?) {
                (Some(a), Some(b)) => Some(a.min(b)),
                _ => None,
            },
        )
    }

    /// The chain-state log's id for `block`.
    pub fn id(&self, block: u32) -> Result<[u8; 32]> {
        Ok(*self.states.slab(block, block)?.entry(block)?.id())
    }
}

impl ChunkSource for LiveLogs {
    fn read(&self, start: u32, end: u32, f: &mut BlockVisit<'_>) -> Result<()> {
        let states = self.states.slab(start, end)?;
        let trace_from = start.max(self.traces.first_block());
        let traces = (trace_from <= end)
            .then(|| self.traces.slab(trace_from, end))
            .transpose()?;
        let (mut state, mut trace) = (Vec::new(), Vec::new());
        for block in start..=end {
            let entry = states.entry(block)?;
            entry.inflate_into(MAX_PAYLOAD, &mut state)?;
            let id = *entry.id();
            match &traces {
                Some(t) if block >= trace_from => {
                    let entry = t.entry(block)?;
                    ensure!(
                        entry.id() == &id,
                        "trace/state block identity mismatch at {block}"
                    );
                    entry.inflate_into(MAX_PAYLOAD, &mut trace)?;
                    f(block, id, &state, Some(&trace))?;
                }
                _ => f(block, id, &state, None)?,
            }
        }
        Ok(())
    }
}

/// Read blocks `start..=end` and keep what the fold must see.
fn extract(source: &dyn ChunkSource, start: u32, end: u32) -> Result<Chunk> {
    let mut chunk = Chunk {
        start,
        ids: Vec::with_capacity((end - start + 1) as usize),
        blocks: Vec::new(),
        first_sequence: None,
        last_sequence: None,
    };
    let mut receipts = Vec::new();
    // The account's previous row in this chunk, and its last ABI sequence observation since its
    // last deployment in this chunk.
    let mut previous: HashMap<u64, (bool, Vec<u8>)> = HashMap::new();
    let mut observed: HashMap<u64, u64> = HashMap::new();
    source.read(start, end, &mut |block, id, state_bytes, trace_bytes| {
        chunk.ids.push(id);
        let mut events = Vec::new();
        let mut deployed = HashSet::new();
        if let Some(trace_bytes) = trace_bytes {
            let (mut txs, mut actions) = (Vec::new(), Vec::new());
            wire_walk::transactions(trace_bytes, &mut txs, &mut actions)
                .map_err(anyhow::Error::from)
                .and_then(|()| committed(&txs, &actions, &mut receipts))
                .with_context(|| format!("Wire trace block {block}"))?;
            for receipt in receipts.drain(..) {
                if let Some(prior) = chunk.last_sequence {
                    ensure!(
                        receipt.sequence > prior,
                        "global action sequence regressed at block {block}"
                    );
                }
                chunk
                    .first_sequence
                    .get_or_insert((receipt.sequence, block));
                chunk.last_sequence = Some(receipt.sequence);
                if let Some((target, account, abi)) = receipt.deployment {
                    observed.remove(&target);
                    deployed.insert(target);
                    events.push(Event::Deploy(Deployment {
                        account,
                        abi,
                        transaction: hex::encode(receipt.transaction),
                        ordinal: receipt.ordinal,
                        sequence: receipt.sequence,
                    }));
                } else if let Some(abi_sequence) = receipt.abi_sequence {
                    // Repeating the previous observation for the account checks nothing new.
                    if observed.insert(receipt.account, abi_sequence) != Some(abi_sequence) {
                        events.push(Event::Revision {
                            account: name_string(receipt.account),
                            abi_sequence,
                        });
                    }
                }
            }
        }
        // Wire emits deltas in a schema-selected order; the walk never assumes `account` is first.
        let mut rows = Vec::new();
        wire_walk::account_rows(state_bytes, &mut rows)
            .with_context(|| format!("Wire state block {block}"))?;
        let mut seen = HashSet::new();
        let mut kept = Vec::new();
        for row in &rows {
            ensure!(
                row.present <= 1,
                "Wire state block {block}: invalid delta presence"
            );
            let account = wire_walk::account(row.data)
                .with_context(|| format!("Wire state block {block}"))?;
            ensure!(
                seen.insert(account.name),
                "duplicate account final state at block {block}"
            );
            let present = row.present == 1;
            let abi: &[u8] = if present { account.abi } else { &[] };
            // A row equal to the account's previous one leaves the history unchanged, unless a
            // deployment in this block must be matched against it.
            let unchanged = previous
                .get(&account.name)
                .is_some_and(|(p, a)| *p == present && a.as_slice() == abi);
            if unchanged && !deployed.contains(&account.name) {
                continue;
            }
            previous.insert(account.name, (present, abi.to_vec()));
            kept.push(Row {
                account: name_string(account.name),
                state: AccountState {
                    present,
                    abi: abi.to_vec(),
                },
                ordinal: row.ordinal,
            });
        }
        if !events.is_empty() || !kept.is_empty() {
            chunk.blocks.push(BlockFacts {
                block,
                events,
                rows: kept,
            });
        }
        Ok(())
    })?;
    Ok(chunk)
}

fn abi_fields(record: &mut Value, provider: &ProtocolProvider, state: &AccountState) {
    if !state.present || state.abi.is_empty() {
        return;
    }
    match provider.decode_abi(&state.abi) {
        Ok(abi) => record["abi"] = abi,
        Err(error) => record["abiDecodeError"] = json!(format!("{error:#}")),
    }
}

/// The running history state, applied in block order.
struct Fold<'a, R: Write, I: Write> {
    provider: &'a ProtocolProvider,
    revisions: BTreeMap<String, u64>,
    current: BTreeMap<String, AccountState>,
    /// Extension: the base records describe every account with ABI history, but not accounts
    /// that only ever held an empty ABI, so their deletion cannot be proven.
    partial_current: bool,
    last_sequence: Option<u64>,
    record_count: u64,
    records: R,
    ids: I,
}
impl<R: Write, I: Write> Fold<'_, R, I> {
    fn emit(&mut self, record: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.records, record)?;
        self.records.write_all(b"\n")?;
        self.record_count += 1;
        Ok(())
    }
    fn apply(&mut self, chunk: Chunk) -> Result<()> {
        if let (Some((first, block)), Some(prior)) = (chunk.first_sequence, self.last_sequence) {
            ensure!(
                first > prior,
                "global action sequence regressed at block {block}"
            );
        }
        if chunk.last_sequence.is_some() {
            self.last_sequence = chunk.last_sequence;
        }
        for facts in chunk.blocks {
            let block = facts.block;
            let block_id = hex::encode(chunk.ids[(block - chunk.start) as usize]);
            let mut changed = BTreeMap::<String, Vec<u8>>::new();
            for event in facts.events {
                match event {
                    Event::Deploy(d) => {
                        let revision = self.revisions.entry(d.account.clone()).or_default();
                        *revision = revision.checked_add(1).context("ABI revision overflow")?;
                        let abi_sequence = revision.to_string();
                        let state = AccountState {
                            present: true,
                            abi: d.abi,
                        };
                        let mut record = json!({"account":d.account,"blockNum":block,"blockId":block_id,"phase":"deployment","present":true,"abiHex":hex::encode(&state.abi),"abiSequence":abi_sequence,
                            "action":{"transactionId":d.transaction,"actionOrdinal":d.ordinal,"globalSequence":d.sequence.to_string()}});
                        abi_fields(&mut record, self.provider, &state);
                        self.emit(&record)?;
                        changed.insert(d.account, state.abi);
                    }
                    Event::Revision {
                        account,
                        abi_sequence,
                    } => ensure!(
                        abi_sequence == *self.revisions.get(&account).unwrap_or(&0),
                        "ABI receipt revision disagrees with deployments for {account} at block {block}"
                    ),
                }
            }
            for Row {
                account,
                state,
                ordinal,
            } in facts.rows
            {
                if let Some(deployment) = changed.remove(&account) {
                    ensure!(
                        state.present && state.abi == deployment,
                        "deployment/final ABI mismatch for {account} at block {block}"
                    );
                } else if block != 1 {
                    ensure!(
                        self.current.get(&account).is_some_and(|old| *old == state)
                            || !state.present
                            || (!self.current.contains_key(&account) && state.abi.is_empty()),
                        "ABI state changed without a successful setabi for {account} at block {block}"
                    );
                }
                ensure!(
                    !(self.partial_current
                        && !state.present
                        && !self.current.contains_key(&account)),
                    "the base catalog cannot prove the state {account} removes at block {block}; scan from genesis"
                );
                let different = self.current.get(&account).is_none_or(|old| *old != state);
                if different && (!state.abi.is_empty() || self.current.contains_key(&account)) {
                    let mut record = json!({"account":account,"blockNum":block,"blockId":block_id,"phase":"block-final","present":state.present,"abiHex":hex::encode(&state.abi),
                        "abiSequence":self.revisions.get(&account).unwrap_or(&0).to_string(),"deltaOrdinal":ordinal});
                    abi_fields(&mut record, self.provider, &state);
                    self.emit(&record)?;
                }
                self.current.insert(account, state);
            }
            // Re-setting the same ABI can leave no changed account row, but still creates a new epoch.
            for (account, value) in changed {
                ensure!(
                    self.current
                        .get(&account)
                        .is_some_and(|old| old.present && old.abi == value),
                    "setabi lacks its changed final account row for {account} at block {block}"
                );
            }
        }
        for id in &chunk.ids {
            self.ids.write_all(id)?;
        }
        Ok(())
    }
}

/// Read `from..=to` on `threads` threads and fold the chunks in block order.
fn scan_range<R: Write, I: Write>(
    sources: &dyn ChunkSource,
    from: u32,
    to: u32,
    threads: usize,
    fold: &mut Fold<'_, R, I>,
    quiet: bool,
) -> Result<()> {
    let cursor = AtomicU64::new(u64::from(from));
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let total = u64::from(to - from) + 1;
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel::<Result<Chunk>>();
        for _ in 0..threads {
            let sender = sender.clone();
            let (cursor, stop) = (&cursor, &stop);
            scope.spawn(move || {
                while !stop.load(Relaxed) {
                    let start = cursor.fetch_add(u64::from(CHUNK), Relaxed);
                    if start > u64::from(to) {
                        break;
                    }
                    let end = (start + u64::from(CHUNK) - 1).min(u64::from(to)) as u32;
                    let result = extract(sources, start as u32, end);
                    let failed = result.is_err();
                    if sender.send(result).is_err() || failed {
                        stop.store(true, Relaxed);
                        break;
                    }
                }
            });
        }
        drop(sender);
        let outcome = (|| -> Result<()> {
            let mut pending = BTreeMap::new();
            let mut next = u64::from(from);
            let mut reported = 0u64;
            for result in &receiver {
                let chunk = result?;
                pending.insert(u64::from(chunk.start), chunk);
                while let Some(chunk) = pending.remove(&next) {
                    next += chunk.ids.len() as u64;
                    fold.apply(chunk)?;
                    let done = next - u64::from(from);
                    if !quiet && (done / 250_000 > reported / 250_000 || done == total) {
                        reported = done;
                        let seconds = started.elapsed().as_secs_f64().max(1e-9);
                        eprintln!(
                            "[wire-disk] {}/{to} blocks, {} sparse ABI records, {:.0} blocks/s",
                            next - 1,
                            fold.record_count,
                            done as f64 / seconds
                        );
                    }
                }
            }
            ensure!(
                next > u64::from(to),
                "reader threads stopped before block {next}"
            );
            Ok(())
        })();
        if outcome.is_err() {
            stop.store(true, Relaxed);
        }
        drop(receiver);
        outcome
    })
}

/// What a verified base catalog contributes to its extension.
///
/// Each base member is read once: the bytes that are hashed are the bytes that are parsed and
/// copied, so a member replaced while the extension runs cannot reach its output.
struct Base {
    end: u32,
    first_block_id: String,
    manifest_sha256: String,
    genesis_trace_absent: bool,
    last_sequence: Option<u64>,
    records: u64,
    /// The verified deployment records, copied verbatim ahead of the new ones.
    deployments: Vec<u8>,
    /// The block membership sidecar, verified while it is copied.
    block_ids: BaseIds,
    revisions: BTreeMap<String, u64>,
    current: BTreeMap<String, AccountState>,
    last_block_id: [u8; 32],
}

struct BaseIds {
    file: File,
    bytes: u64,
    sha256: String,
    first: [u8; 32],
    last: [u8; 32],
}

/// The bound the catalog's reader puts on the deployments member.
const MAX_DEPLOYMENT_BYTES: u64 = 128 * 1024 * 1024;

/// Copy the base block membership into the extension's sidecar. The size, digest and boundary
/// ids are checked on exactly the bytes written.
fn copy_base_ids(ids: &mut BaseIds, out: &mut impl Write) -> Result<()> {
    let mut source = (&mut ids.file).take(ids.bytes + 1);
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let (mut total, mut first, mut last) = (0u64, None, [0u8; 32]);
    loop {
        let mut filled = 0;
        while filled < buffer.len() {
            match source.read(&mut buffer[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        if filled == 0 {
            break;
        }
        let chunk = &buffer[..filled];
        digest.update(chunk);
        out.write_all(chunk)?;
        total += filled as u64;
        if filled >= 32 {
            first.get_or_insert_with(|| <[u8; 32]>::try_from(&chunk[..32]).expect("32 bytes"));
            last.copy_from_slice(&chunk[filled - 32..]);
        }
        if filled < buffer.len() {
            break;
        }
    }
    ensure!(
        total == ids.bytes && hex::encode(digest.finalize()) == ids.sha256,
        "base block membership differs from its manifest"
    );
    ensure!(
        first == Some(ids.first) && last == ids.last,
        "base block membership differs from its boundary ids"
    );
    Ok(())
}

/// The state a catalog's records describe at its end: each account's revision and last state,
/// and the record count. Accounts whose ABI was always empty have no records; the fold refuses
/// what that leaves unproven.
#[allow(clippy::type_complexity)]
fn base_state(
    deployments: &[u8],
    end: u32,
) -> Result<(BTreeMap<String, u64>, BTreeMap<String, AccountState>, u64)> {
    let text = |value: &Value, field: &str| -> Result<String> {
        Ok(value
            .as_str()
            .with_context(|| format!("base record lacks {field}"))?
            .to_owned())
    };
    let mut revisions = BTreeMap::new();
    let mut current = BTreeMap::new();
    let mut records = 0u64;
    for line in deployments.split(|b| *b == b'\n') {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let row: Value = serde_json::from_slice(line).context("invalid base ABI record")?;
        let account = text(&row["account"], "record account")?;
        ensure!(
            row["blockNum"]
                .as_u64()
                .is_some_and(|b| b >= 1 && b <= u64::from(end)),
            "base record lies outside the base range"
        );
        let abi_sequence = text(&row["abiSequence"], "record ABI sequence")?
            .parse::<u64>()
            .context("invalid base record ABI sequence")?;
        let present = row["present"]
            .as_bool()
            .context("invalid base record presence")?;
        let abi = hex::decode(text(&row["abiHex"], "record ABI")?)?;
        let revision = revisions.entry(account.clone()).or_insert(0);
        *revision = (*revision).max(abi_sequence);
        current.insert(account, AccountState { present, abi });
        records += 1;
    }
    Ok((revisions, current, records))
}

fn load_base(base: &BaseCatalog, options: &ScanOptions, node_revision: &str) -> Result<Base> {
    let raw = fs::read(&base.manifest)
        .with_context(|| format!("read base manifest {}", base.manifest.display()))?;
    ensure!(raw.len() <= 65536, "base manifest exceeds its bound");
    ensure!(
        hash(&raw) == base.manifest_sha256,
        "base manifest SHA-256 differs from the pinned value"
    );
    let m: Value = serde_json::from_slice(&raw).context("invalid base manifest")?;
    ensure!(
        m["format"] == FORMAT && m["complete"] == true && m["startBlock"] == 1,
        "base is not a complete Wire ABI history"
    );
    ensure!(
        m["chainId"] == options.chain_id.as_str()
            && m["nodeRevision"] == node_revision
            && m["producerProfileId"] == options.provider.id()
            && m["shipAbiSha256"] == hash(options.schema.as_bytes()).as_str(),
        "base catalog belongs to another chain, node revision, profile or SHiP schema"
    );
    let end = m["endBlock"]
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .filter(|v| *v >= 1)
        .context("invalid base end block")?;
    ensure!(end < options.end, "the base catalog must end before --end");
    let text = |value: &Value, field: &str| -> Result<String> {
        Ok(value
            .as_str()
            .with_context(|| format!("base manifest lacks {field}"))?
            .to_owned())
    };
    let last_block_id = text(&m["lastBlockId"], "lastBlockId")?;
    let first_block_id = text(&m["firstBlockId"], "firstBlockId")?;
    ensure!(
        m["irreversibleBoundary"]["blockNum"] == end
            && m["irreversibleBoundary"]["blockId"] == last_block_id.as_str(),
        "base irreversible boundary differs from its end"
    );
    let last = hex_id(&last_block_id)?;
    hex_id(&first_block_id)?;
    let last_sequence = match &m["lastGlobalSequence"] {
        Value::Null => None,
        value => {
            let s = value
                .as_str()
                .context("invalid base last global sequence")?;
            let n = s
                .parse::<u64>()
                .context("invalid base last global sequence")?;
            ensure!(n.to_string() == s, "noncanonical base last global sequence");
            Some(n)
        }
    };
    let genesis_trace_absent = m["nativeBaseline"]["genesisTraceAbsent"]
        .as_bool()
        .context("base manifest lacks its native baseline")?;
    let directory = base
        .manifest
        .parent()
        .context("base manifest has no directory")?;
    let member = |entry: &Value| -> Result<(String, PathBuf, u64, String)> {
        let name = text(&entry["name"], "member name")?;
        ensure!(
            Path::new(&name).file_name() == Some(std::ffi::OsStr::new(&name))
                && !name.starts_with('.'),
            "base member {name} is not a plain file name"
        );
        let bytes = entry["bytes"]
            .as_u64()
            .with_context(|| format!("base member {name} lacks its size"))?;
        let sha256 = text(&entry["sha256"], "member sha256")?;
        let path = directory.join(&name);
        Ok((name, path, bytes, sha256))
    };
    let (name, path, bytes, sha256) = member(&m["deployments"])?;
    ensure!(
        bytes <= MAX_DEPLOYMENT_BYTES,
        "base member {name} exceeds its bound"
    );
    let mut deployments = Vec::with_capacity(bytes as usize);
    File::open(&path)
        .with_context(|| format!("open base member {name}"))?
        .take(bytes + 1)
        .read_to_end(&mut deployments)?;
    ensure!(
        deployments.len() as u64 == bytes && hash(&deployments) == sha256,
        "base member {name} differs from its manifest"
    );
    let (name, path, bytes, sha256) = member(&m["blockIds"])?;
    ensure!(
        m["blockIds"]["count"] == end && bytes == u64::from(end) * 32,
        "base block membership does not cover its range"
    );
    let block_ids = BaseIds {
        file: File::open(&path).with_context(|| format!("open base member {name}"))?,
        bytes,
        sha256,
        first: hex_id(&first_block_id)?,
        last,
    };
    let (revisions, current, records) = base_state(&deployments, end)?;
    ensure!(
        m["deployments"]["records"] == records,
        "base record count differs from its manifest"
    );
    Ok(Base {
        end,
        first_block_id,
        manifest_sha256: base.manifest_sha256.clone(),
        genesis_trace_absent,
        last_sequence,
        records,
        deployments,
        block_ids,
        revisions,
        current,
        last_block_id: last,
    })
}

/// Scan the complete genesis-to-irreversible prefix, or extend a sealed base catalog to it.
/// Inputs must be a stable frozen view. Partial artifacts are retained for diagnosis; only a
/// sealed manifest authorizes consumption.
pub fn scan(options: &ScanOptions) -> Result<Value> {
    options.provider.validate()?;
    let ProtocolProvider::Wire(profile) = &options.provider else {
        bail!("Wire ABI history requires an explicit Wire profile");
    };
    ensure!(
        options.end > 0 && options.end < u32::MAX,
        "invalid Wire history end"
    );
    hex_id(&options.chain_id)?;
    let irreversible = hex_id(&options.irreversible_block_id)?;
    ensure!(
        u32::from_be_bytes(irreversible[..4].try_into()?) == options.end,
        "irreversible id does not identify end block"
    );
    let schema: Value =
        serde_json::from_str(&options.schema).context("invalid Wire SHiP schema")?;
    // The codec validates the schema once; the walks read its layouts as fixed definitions.
    options
        .provider
        .decode_value(&schema, "table_delta[]", &[0])
        .context("invalid Wire SHiP schema")?;
    wire_walk::check_ship_schema(&schema).map_err(anyhow::Error::msg)?;
    let mut base = options
        .base
        .as_ref()
        .map(|base| load_base(base, options, &profile.upstream_revision))
        .transpose()?;
    // A full scan reads from genesis; an extension needs the base's last block to prove continuity.
    let from = base.as_ref().map_or(1, |b| b.end + 1);
    ensure!(
        options.start == from,
        "{}",
        match &base {
            Some(base) => format!(
                "extending a catalog that ends at {} must start at {from}",
                base.end
            ),
            None => "complete Wire ABI history must start at genesis (block 1)".into(),
        }
    );
    let window = (base.as_ref().map_or(1, |b| b.end), options.end + 1);
    let cache = FileCache::new(16);
    let discovered = discover(std::slice::from_ref(&options.directory), &[], true)?;
    let states = open_catalog(&discovered.chain_state, LogKind::ChainState, &cache, window)?;
    let traces = open_catalog(&discovered.trace, LogKind::Trace, &cache, window)?;
    match &base {
        None => {
            ensure!(
                states.range().0 == 1 && states.range().1 > options.end,
                "state history does not cover genesis through end"
            );
            ensure!(
                matches!(traces.range().0, 1 | 2) && traces.range().1 > options.end,
                "trace history does not cover block 2 through end"
            );
        }
        Some(base) => {
            ensure!(
                states.range().0 <= base.end && states.range().1 > options.end,
                "state history does not cover the base's last block through end"
            );
            ensure!(
                traces.range().0 <= from && traces.range().1 > options.end,
                "trace history does not cover the blocks after the base through end"
            );
            let id = states
                .get(base.end)
                .context("missing base end state block")?
                .id(base.end)?;
            ensure!(
                id == base.last_block_id,
                "the logs no longer hold the base catalog's last block {}; the chain forked past it",
                base.end
            );
        }
    }
    let last = states
        .get(options.end)
        .context("missing last state block")?
        .id(options.end)?;
    ensure!(
        last == irreversible,
        "source end differs from the supplied irreversible boundary"
    );
    let first_block_id = match &base {
        Some(base) => base.first_block_id.clone(),
        None => hex::encode(states.get(1).context("missing genesis state")?.id(1)?),
    };
    let source_paths = states
        .files()
        .iter()
        .chain(traces.files())
        .flat_map(|log| [log.log_path(), log.index_path()])
        .collect::<BTreeSet<_>>();
    let stamps = source_paths
        .iter()
        .map(|path| {
            let m = fs::metadata(path)?;
            Ok(((*path).to_path_buf(), m.len(), m.modified()?))
        })
        .collect::<Result<Vec<_>>>()?;
    let blocks_path = suffix(&options.output, ".blocks.bin");
    let manifest_path = suffix(&options.output, ".manifest.json");
    for path in [&options.output, &blocks_path, &manifest_path] {
        ensure!(!path.exists(), "output already exists: {}", path.display());
    }
    let records_partial = suffix(&options.output, ".partial");
    let blocks_partial = suffix(&blocks_path, ".partial");
    let manifest_partial = suffix(&manifest_path, ".partial");
    let mut records = BufWriter::with_capacity(1 << 20, fresh(&records_partial)?);
    let mut ids = BufWriter::with_capacity(1 << 20, fresh(&blocks_partial)?);
    if let Some(base) = &mut base {
        records.write_all(&base.deployments)?;
        base.deployments = Vec::new();
        copy_base_ids(&mut base.block_ids, &mut ids)?;
    }
    let threads = match options.threads {
        0 => std::thread::available_parallelism().map_or(1, usize::from),
        n => n,
    };
    let mut fold = Fold {
        provider: &options.provider,
        revisions: base
            .as_ref()
            .map(|b| b.revisions.clone())
            .unwrap_or_default(),
        current: base.as_ref().map(|b| b.current.clone()).unwrap_or_default(),
        partial_current: base.is_some(),
        last_sequence: base.as_ref().and_then(|b| b.last_sequence),
        record_count: base.as_ref().map_or(0, |b| b.records),
        records,
        ids,
    };
    let sources = Sources {
        states: &states,
        traces: &traces,
        trace_start: traces.range().0,
    };
    scan_range(&sources, from, options.end, threads, &mut fold, false)?;
    ensure!(
        options.end == 1 || fold.last_sequence.is_some(),
        "history has no committed action receipts"
    );
    for (path, size, modified) in &stamps {
        let m = fs::metadata(path)?;
        ensure!(
            m.len() == *size && m.modified()? == *modified,
            "source changed while scanning: {}",
            path.display()
        );
    }
    let Fold {
        records,
        ids,
        record_count,
        last_sequence,
        ..
    } = fold;
    let records = records.into_inner().map_err(|e| e.into_error())?;
    records.sync_all()?;
    let ids = ids.into_inner().map_err(|e| e.into_error())?;
    ids.sync_all()?;
    drop(records);
    drop(ids);
    let (record_bytes, record_hash) = file_hash(&records_partial)?;
    let (id_bytes, id_hash) = file_hash(&blocks_partial)?;
    ensure!(
        id_bytes == u64::from(options.end) * 32,
        "block membership length mismatch"
    );
    let sources = states.files().iter().map(|log| ("chain-state",log)).chain(traces.files().iter().map(|log| ("trace",log)))
        .map(|(kind, log)| json!({"kind":kind,"log":log.log_path(),"index":log.index_path(),"begin":log.range().0,"endExclusive":log.range().1,"logBytes":log.size()})).collect::<Vec<_>>();
    let genesis_trace_absent = base
        .as_ref()
        .map_or(traces.range().0 == 2, |b| b.genesis_trace_absent);
    let mut manifest = json!({"format":FORMAT,"chainId":options.chain_id,"nodeRevision":profile.upstream_revision,"producerProfileId":options.provider.id(),
        "producerRevision":WIRE_SOURCE_REVISION,"shipAbiSha256":hash(options.schema.as_bytes()),"startBlock":1,"endBlock":options.end,"firstBlockId":first_block_id,"lastBlockId":options.irreversible_block_id,
        "irreversibleBoundary":{"blockNum":options.end,"blockId":options.irreversible_block_id},"complete":true,"lastGlobalSequence":last_sequence.map(|v|v.to_string()),
        "nativeBaseline":{"kind":"wire-native-profile","abiSequence":"0","genesisTraceAbsent":genesis_trace_absent},
        "deployments":{"name":basename(&options.output)?,"bytes":record_bytes,"sha256":record_hash,"records":record_count},
        "blockIds":{"name":basename(&blocks_path)?,"bytes":id_bytes,"sha256":id_hash,"count":options.end},"sources":sources});
    if let Some(base) = &base {
        // The base members are reproduced byte for byte; the sources list only this run's reads.
        manifest["base"] = json!({"manifestSha256":base.manifest_sha256,"endBlock":base.end,"lastBlockId":hex::encode(base.last_block_id)});
    }
    let mut manifest_file = fresh(&manifest_partial)?;
    serde_json::to_writer(&mut manifest_file, &manifest)?;
    manifest_file.write_all(b"\n")?;
    manifest_file.sync_all()?;
    drop(manifest_file);
    publish(&records_partial, &options.output)?;
    publish(&blocks_partial, &blocks_path)?;
    publish(&manifest_partial, &manifest_path)?;
    eprintln!(
        "[wire-disk] complete: {} blocks, {record_count} sparse ABI records; {}",
        options.end,
        manifest_path.display()
    );
    Ok(manifest)
}

/// Format of an incremental catalog segment: the records and block ids of `startBlock..=endBlock`
/// only, naming the catalog (complete or segment) it extends. A complete catalog's records and
/// block ids are the concatenation of its chain's members, oldest first.
pub const SEGMENT_FORMAT: &str = "hyperion-abi-history-segment-v1";

/// The newest catalog of a sealed chain: what the next segment names as its base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    pub manifest_sha256: String,
    pub end_block: u32,
    pub last_block_id: [u8; 32],
    pub last_global_sequence: Option<u64>,
    pub ship_abi_sha256: String,
    /// Records in the whole chain.
    pub total_records: u64,
}

/// Seals a running node's irreversible history as incremental segments, keeping the fold's
/// state between seals, so each seal reads only the blocks after the previous one.
pub struct Sealer {
    provider: ProtocolProvider,
    chain_id: String,
    node_revision: String,
    logs: LiveLogs,
    threads: usize,
    tip: Tip,
    revisions: BTreeMap<String, u64>,
    current: BTreeMap<String, AccountState>,
}

/// One sealed segment.
#[derive(Clone, Debug)]
pub struct Sealed {
    pub manifest: PathBuf,
    pub start_block: u32,
    pub end_block: u32,
    pub records: u64,
}

impl Sealer {
    /// Continue the chain whose newest catalog is `tip`, with `deployments` the records of the
    /// whole chain. The SHiP `schema` must be the one the chain was sealed with, and the logs in
    /// `state_history` must still hold the tip's last block.
    pub fn open(
        provider: &ProtocolProvider,
        schema: &str,
        chain_id: &str,
        state_history: &Path,
        tip: Tip,
        deployments: &[u8],
        threads: usize,
    ) -> Result<Self> {
        provider.validate()?;
        let ProtocolProvider::Wire(profile) = provider else {
            bail!("sealing ABI history needs a protocol walk; only Wire has one so far");
        };
        hex_id(chain_id)?;
        ensure!(
            hash(schema.as_bytes()) == tip.ship_abi_sha256,
            "the SHiP schema differs from the one the catalog chain was sealed with"
        );
        let parsed: Value = serde_json::from_str(schema).context("invalid Wire SHiP schema")?;
        provider
            .decode_value(&parsed, "table_delta[]", &[0])
            .context("invalid Wire SHiP schema")?;
        wire_walk::check_ship_schema(&parsed).map_err(anyhow::Error::msg)?;
        let logs = LiveLogs::open(state_history)?;
        ensure!(
            logs.id(tip.end_block)? == tip.last_block_id,
            "the logs no longer hold the catalog's last block {}; the chain forked past it",
            tip.end_block
        );
        let (revisions, current, records) = base_state(deployments, tip.end_block)?;
        ensure!(
            records == tip.total_records,
            "the chain's records ({records}) differ from its newest manifest ({})",
            tip.total_records
        );
        Ok(Self {
            provider: provider.clone(),
            chain_id: chain_id.to_owned(),
            node_revision: profile.upstream_revision.clone(),
            logs,
            threads: match threads {
                0 => std::thread::available_parallelism().map_or(1, usize::from),
                n => n,
            },
            tip,
            revisions,
            current,
        })
    }

    pub fn tip(&self) -> &Tip {
        &self.tip
    }

    /// The last block both state-history logs index now.
    pub fn logs_end(&self) -> Result<Option<u32>> {
        self.logs.last_block()
    }

    /// Seal the blocks after the tip through `end` (irreversible, with id `end_id`) as one
    /// segment in `dir`. `check(start, ids)` sees the segment's block ids before anything is
    /// published, so a caller can hold them against another log. Nothing changes on an error.
    pub fn seal(
        &mut self,
        end: u32,
        end_id: [u8; 32],
        dir: &Path,
        check: &mut IdCheck<'_>,
    ) -> Result<Sealed> {
        let start = self.tip.end_block + 1;
        ensure!(end >= start, "block {end} is already sealed");
        ensure!(
            u32::from_be_bytes(end_id[..4].try_into()?) == end,
            "irreversible id does not identify end block"
        );
        let indexed = self.logs.last_block()?.unwrap_or(0);
        ensure!(
            end <= indexed,
            "the logs index blocks up to {indexed}, not {end}"
        );
        ensure!(
            self.logs.id(end)? == end_id,
            "the logs name another block {end} than the irreversible one"
        );
        let mut fold = Fold {
            provider: &self.provider,
            revisions: self.revisions.clone(),
            current: self.current.clone(),
            partial_current: true,
            last_sequence: self.tip.last_global_sequence,
            record_count: self.tip.total_records,
            records: Vec::new(),
            ids: Vec::new(),
        };
        scan_range(&self.logs, start, end, self.threads, &mut fold, true)?;
        let (ids, _) = fold.ids.as_chunks::<32>();
        ensure!(
            ids.len() == (end - start + 1) as usize && ids.last() == Some(&end_id),
            "segment block membership does not end at the irreversible block"
        );
        check(start, ids)?;
        let added = fold.record_count - self.tip.total_records;
        let stem = format!("abi-history.{start:010}-{end:010}");
        let paths = [
            dir.join(format!("{stem}.ndjson")),
            dir.join(format!("{stem}.ndjson.blocks.bin")),
            dir.join(format!("{stem}.ndjson.manifest.json")),
        ];
        for path in &paths {
            ensure!(!path.exists(), "output already exists: {}", path.display());
        }
        let member = |path: &Path, bytes: &[u8]| -> Result<Value> {
            let partial = suffix(path, ".partial");
            let mut f = fresh(&partial)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            Ok(json!({"name":basename(path)?,"bytes":bytes.len(),"sha256":hash(bytes)}))
        };
        let mut deployments = member(&paths[0], &fold.records)?;
        deployments["records"] = json!(added);
        let mut block_ids = member(&paths[1], &fold.ids)?;
        block_ids["count"] = json!(end - start + 1);
        let end_hex = hex::encode(end_id);
        let manifest = json!({"format":SEGMENT_FORMAT,"chainId":self.chain_id,"nodeRevision":self.node_revision,
            "producerProfileId":self.provider.id(),"producerRevision":WIRE_SOURCE_REVISION,"shipAbiSha256":self.tip.ship_abi_sha256,
            "base":{"manifestSha256":self.tip.manifest_sha256,"endBlock":self.tip.end_block,"lastBlockId":hex::encode(self.tip.last_block_id)},
            "startBlock":start,"endBlock":end,"startBlockId":hex::encode(ids[0]),"lastBlockId":end_hex,
            "irreversibleBoundary":{"blockNum":end,"blockId":end_hex},"complete":true,
            "lastGlobalSequence":fold.last_sequence.map(|v| v.to_string()),"totalRecords":fold.record_count,
            "deployments":deployments,"blockIds":block_ids});
        let text = format!("{}\n", serde_json::to_string(&manifest)?);
        member(&paths[2], text.as_bytes())?;
        // Members first, the manifest last: a manifest names only complete members.
        for path in &paths {
            publish(&suffix(path, ".partial"), path)?;
        }
        let Fold {
            revisions,
            current,
            last_sequence,
            record_count,
            ..
        } = fold;
        self.revisions = revisions;
        self.current = current;
        self.tip = Tip {
            manifest_sha256: hash(text.as_bytes()),
            end_block: end,
            last_block_id: end_id,
            last_global_sequence: last_sequence,
            ship_abi_sha256: self.tip.ship_abi_sha256.clone(),
            total_records: record_count,
        };
        Ok(Sealed {
            manifest: paths[2].clone(),
            start_block: start,
            end_block: end,
            records: added,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_wire_delta_bytes_use_the_evidenced_count_correction() {
        let capture: Value =
            serde_json::from_str(include_str!("../testdata/wire/ship-capture.json")).unwrap();
        let schema: Value =
            serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
        let provider: ProtocolProvider = "wire-sysio-7dede884-engineering-v1".parse().unwrap();
        let frame = capture["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "captured-ship-block-result")
            .unwrap();
        let envelope = provider
            .decode_value(
                &schema,
                "result",
                &hex::decode(frame["hex"].as_str().unwrap()).unwrap(),
            )
            .unwrap();
        let raw = hex::decode(envelope[1]["deltas"].as_str().unwrap()).unwrap();
        let traces = hex::decode(envelope[1]["traces"].as_str().unwrap()).unwrap();
        assert_eq!(
            provider
                .decode_value(&schema, "transaction_trace[]", &traces)
                .unwrap()[0][1]["partial"][0],
            "partial_transaction_v0"
        );
        let (mut txs, mut actions, mut receipts) = (Vec::new(), Vec::new(), Vec::new());
        wire_walk::transactions(&traces, &mut txs, &mut actions).unwrap();
        committed(&txs, &actions, &mut receipts).unwrap();
        assert!(!receipts.is_empty());
        assert!(
            provider
                .decode_value(&schema, "table_delta[]", &raw)
                .is_err()
        );
        let mut rows = Vec::new();
        assert_eq!(
            wire_walk::account_rows(&raw, &mut rows).unwrap(),
            wire_walk::DeltaFraming::ResourceIndexCount
        );
        assert_eq!(rows.len(), 1);
        let account = wire_walk::account(rows[0].data).unwrap();
        assert_eq!(name_string(account.name), "sysio");
        assert_eq!(rows[0].present, 1);
        assert!(hex::encode(account.abi).starts_with("0e737973696f3a3a6162692f312e"));
        assert_eq!(
            provider.decode_abi(account.abi).unwrap()["version"],
            "sysio::abi/1.0"
        );
    }
}
