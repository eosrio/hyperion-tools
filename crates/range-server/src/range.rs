//! One range: chunks produced in parallel, streamed in block order as framed NDJSON.
//!
//! A response is a sequence of lines. Every frame starts with one JSON header line:
//!
//! * `start`: the range, its kinds, the catalog that sealed its last block and the id of the block
//!   before it;
//! * `block`: one block's id, the byte length of what follows and, per requested kind, its document
//!   count and byte length. The header is followed by exactly that many bytes: the block's
//!   `_bulk` lines (an index line and a document line per document), kinds in the fixed order
//!   blocks, actions, deltas;
//! * `manifest`: the trailer. Per kind: documents, bytes and the SHA-256 of that kind's `_bulk`
//!   bytes in stream order; the first and last block ids; the first and last committed global
//!   sequence; and what the producers left without a document;
//! * `error`: the range failed; no manifest follows.
//!
//! A response without its manifest is incomplete. The same request over the same catalog chain
//! returns the same bytes.

use std::io::Write;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use action_proto::{BlockInput, BlockWorker, DocKind, DocMsg, EsCfg, Producer, Segment, WireDocs};
use anyhow::{Context, Result, bail, ensure};
use delta_proto::wire::{DeltaWorker, DocConfig};
use hyperion_ship::slab::{Chunks, ordered_chunks};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{Sealed, Service};

/// Blocks per work unit.
pub const CHUNK: u32 = 2048;
/// Bytes buffered before they are written as one HTTP chunk.
const FLUSH: usize = 1 << 20;

pub const KINDS: [&str; 3] = ["blocks", "actions", "deltas"];

/// A validated range request.
#[derive(Clone, Debug)]
pub struct Request {
    pub from: u32,
    pub to: u32,
    /// Requested kinds, indexed like [`KINDS`].
    pub kinds: [bool; 3],
    pub doc_profile: String,
    pub index: EsCfg,
    pub index_transfer_memo: bool,
    /// Block frames the client grants up front; 0 streams without credits (TCP backpressure).
    pub credits: u64,
    pub threads: Option<usize>,
}

/// Why a request is refused before anything is streamed.
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub extra: Value,
}

fn refuse(status: u16, code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal {
        status,
        code,
        message: message.into(),
        extra: Value::Null,
    }
}

/// Parse and check a `POST /v1/ranges` body against the service.
pub fn parse(svc: &Service, body: &[u8], sealed_lib: u32) -> Result<Request, Refusal> {
    let v: Value = serde_json::from_slice(body)
        .map_err(|e| refuse(400, "invalid_request", format!("body is not JSON: {e}")))?;
    let block = |key: &str| -> Result<u32, Refusal> {
        v[key]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n >= 1)
            .ok_or_else(|| {
                refuse(
                    400,
                    "invalid_request",
                    format!("`{key}` must be a block number"),
                )
            })
    };
    let (from, to) = (block("from")?, block("to")?);
    if to < from {
        return Err(refuse(400, "invalid_request", "`to` is before `from`"));
    }
    let len = to - from + 1;
    if len > svc.max_range {
        return Err(refuse(
            413,
            "range_too_large",
            format!("{len} blocks; at most {} per range", svc.max_range),
        ));
    }
    if to > sealed_lib {
        let mut r = refuse(
            409,
            "beyond_sealed_lib",
            format!("block {to} is past the sealed LIB {sealed_lib}"),
        );
        r.extra = json!({"sealedLib": sealed_lib});
        return Err(r);
    }
    let mut kinds = [false; 3];
    match &v["kinds"] {
        Value::Null => kinds = [true; 3],
        Value::Array(list) if !list.is_empty() => {
            for k in list {
                let name = k.as_str().unwrap_or("");
                match KINDS.iter().position(|x| *x == name) {
                    Some(i) => kinds[i] = true,
                    None if name == "abis" => {
                        return Err(refuse(
                            400,
                            "unsupported_kind",
                            "`abis` is not served yet: no producer of 4.5 ABI documents has parity evidence",
                        ));
                    }
                    None => {
                        return Err(refuse(400, "invalid_request", format!("unknown kind {k}")));
                    }
                }
            }
        }
        _ => {
            return Err(refuse(
                400,
                "invalid_request",
                "`kinds` must be a non-empty list",
            ));
        }
    }
    match v["profile"].as_str() {
        Some(p) if p == svc.protocol.id() => {}
        Some(p) => {
            return Err(refuse(
                409,
                "profile_mismatch",
                format!("this service reads {} logs, not {p}", svc.protocol.id()),
            ));
        }
        None => return Err(refuse(400, "invalid_request", "`profile` is required")),
    }
    let doc_profile = match &v["docProfile"] {
        Value::Null => svc.defaults.doc_profile.clone(),
        Value::String(s) if !s.is_empty() => s.clone(),
        _ => {
            return Err(refuse(
                400,
                "invalid_request",
                "`docProfile` must be a string",
            ));
        }
    };
    let index = &v["index"];
    let text = |key: &str, default: &str| -> Result<String, Refusal> {
        match &index[key] {
            Value::Null => Ok(default.to_owned()),
            Value::String(s)
                if !s.is_empty()
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b)) =>
            {
                Ok(s.clone())
            }
            _ => Err(refuse(
                400,
                "invalid_request",
                format!("`index.{key}` is not an index name part"),
            )),
        }
    };
    let partition_size = match &index["partitionSize"] {
        Value::Null => svc.defaults.partition_size,
        p => p.as_u64().filter(|p| *p >= 1).ok_or_else(|| {
            refuse(
                400,
                "invalid_request",
                "`index.partitionSize` must be positive",
            )
        })?,
    };
    let index = EsCfg {
        url: String::new(),
        chain: text("prefix", &svc.defaults.index_prefix)?,
        version: text("version", &svc.defaults.index_version)?,
        partition_size,
        batch: 0,
    };
    let flag = |key: &str, default: bool| -> Result<bool, Refusal> {
        match &v[key] {
            Value::Null => Ok(default),
            Value::Bool(b) => Ok(*b),
            _ => Err(refuse(
                400,
                "invalid_request",
                format!("`{key}` must be a boolean"),
            )),
        }
    };
    let count = |key: &str| -> Result<Option<u64>, Refusal> {
        match &v[key] {
            Value::Null => Ok(None),
            c => c
                .as_u64()
                .map(Some)
                .ok_or_else(|| refuse(400, "invalid_request", format!("`{key}` must be a count"))),
        }
    };
    Ok(Request {
        from,
        to,
        kinds,
        doc_profile,
        index,
        index_transfer_memo: flag("indexTransferMemo", svc.defaults.index_transfer_memo)?,
        credits: count("credits")?.unwrap_or(0),
        threads: count("threads")?.map(|t| t.max(1) as usize),
    })
}

/// Credits a client grants a stream, in block frames.
pub struct Credits {
    state: Mutex<(u64, bool)>,
    granted: Condvar,
}

impl Credits {
    pub fn new(initial: u64) -> Self {
        Self {
            state: Mutex::new((initial, false)),
            granted: Condvar::new(),
        }
    }

    pub fn grant(&self, frames: u64) -> u64 {
        let mut s = self.state.lock().unwrap();
        s.0 = s.0.saturating_add(frames);
        self.granted.notify_all();
        s.0
    }

    pub fn cancel(&self) {
        self.state.lock().unwrap().1 = true;
        self.granted.notify_all();
    }

    fn available(&self) -> bool {
        self.state.lock().unwrap().0 > 0
    }

    /// Take one credit, waiting at most `timeout` for a grant.
    fn take(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut s = self.state.lock().unwrap();
        loop {
            ensure!(!s.1, "the stream was cancelled");
            if s.0 > 0 {
                s.0 -= 1;
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(!left.is_zero(), "no credit was granted within {timeout:?}");
            s = self.granted.wait_timeout(s, left).unwrap().0;
        }
    }
}

/// Counting semaphore: a worker holds a permit while it produces a chunk, so concurrent ranges
/// share `--threads` instead of the first range taking them all.
pub struct Permits {
    free: Mutex<usize>,
    released: Condvar,
}

pub struct Permit<'a>(&'a Permits);

impl Permits {
    pub fn new(n: usize) -> Self {
        Self {
            free: Mutex::new(n.max(1)),
            released: Condvar::new(),
        }
    }

    pub fn acquire(&self) -> Permit<'_> {
        let mut free = self.free.lock().unwrap();
        while *free == 0 {
            free = self.released.wait(free).unwrap();
        }
        *free -= 1;
        Permit(self)
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        *self.0.free.lock().unwrap() += 1;
        self.0.released.notify_one();
    }
}

/// One block's frame payload.
struct BlockOut {
    block: u32,
    id: [u8; 32],
    parts: [String; 3],
    counts: [u64; 3],
    sequences: Option<(u64, u64)>,
}

/// One chunk: its blocks and the walked run of block headers.
struct ChunkOut {
    blocks: Vec<BlockOut>,
    segment: Option<Segment>,
}

/// A worker's decoders for one range.
struct Workers<'a> {
    actions: BlockWorker<'a>,
    deltas: DeltaWorker<'a>,
}

/// Chunked HTTP body writer: frames are buffered and written as large chunks.
struct Body<'w> {
    out: &'w mut dyn Write,
    buf: Vec<u8>,
}

impl Body<'_> {
    fn flush(&mut self) -> Result<()> {
        if !self.buf.is_empty() {
            write!(self.out, "{:x}\r\n", self.buf.len())?;
            self.out.write_all(&self.buf)?;
            self.out.write_all(b"\r\n")?;
            self.buf.clear();
        }
        self.out.flush()?;
        Ok(())
    }

    fn line(&mut self, v: &str) -> Result<()> {
        self.buf.extend_from_slice(v.as_bytes());
        self.buf.push(b'\n');
        if self.buf.len() >= FLUSH {
            self.flush()?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        self.flush()?;
        self.out.write_all(b"0\r\n\r\n")?;
        self.out.flush()?;
        Ok(())
    }
}

fn id_or_null(id: Option<[u8; 32]>) -> Value {
    id.map_or(Value::Null, |id| Value::from(hex::encode(id)))
}

/// What the HTTP layer needs to know once a range is accepted.
pub struct Plan {
    pub catalog_sha: String,
    pub previous: Option<[u8; 32]>,
    pub threads: usize,
}

pub fn plan(svc: &Service, sealed: &Sealed, req: &Request) -> Result<Plan> {
    let catalog_sha = sealed
        .chain_segments
        .iter()
        .find(|s| s.end_block >= req.to)
        .context("no catalog seals the range")?
        .manifest_sha256
        .clone();
    let previous = if req.from > 1 {
        Some(sealed.newest.block_id(req.from - 1)?)
    } else {
        None
    };
    let chunks = (req.to - req.from) / CHUNK + 1;
    // Every worker holds decoder state for the range's life; a range that starts while others
    // run takes its share of the budget (the permits still bound what runs at once).
    let share = svc.threads / svc.active_ranges().max(1);
    let threads = req
        .threads
        .unwrap_or(svc.range_threads)
        .min(share.max(1))
        .min(chunks as usize)
        .max(1);
    Ok(Plan {
        catalog_sha,
        previous,
        threads,
    })
}

/// Produce and stream `req`. Errors after the first byte are reported as an `error` frame.
pub fn stream(
    svc: &Service,
    sealed: &Sealed,
    req: &Request,
    plan: &Plan,
    credits: Option<&Arc<Credits>>,
    out: &mut dyn Write,
) -> Result<()> {
    let mut body = Body {
        out,
        buf: Vec::with_capacity(FLUSH + (FLUSH >> 2)),
    };
    let kinds: Vec<&str> = KINDS
        .iter()
        .zip(req.kinds)
        .filter_map(|(k, on)| on.then_some(*k))
        .collect();
    body.line(
        &json!({"frame":"start","from":req.from,"to":req.to,"kinds":kinds,
            "catalogSha":plan.catalog_sha,"previousBlockId":id_or_null(plan.previous)})
        .to_string(),
    )?;
    let result = produce(svc, sealed, req, plan, credits, &mut body);
    match result {
        Ok(manifest) => {
            body.line(&manifest.to_string())?;
            body.finish()
        }
        Err(e) => {
            let message = format!("{e:#}");
            // The client may be gone; the error is still the range's outcome.
            let _ = body.line(&json!({"frame":"error","message":message}).to_string());
            let _ = body.finish();
            Err(e)
        }
    }
}

fn produce(
    svc: &Service,
    sealed: &Sealed,
    req: &Request,
    plan: &Plan,
    credits: Option<&Arc<Credits>>,
    body: &mut Body<'_>,
) -> Result<Value> {
    let [want_blocks, want_actions, want_deltas] = req.kinds;
    let docs = WireDocs {
        chain_id: Some(svc.chain_id.clone()),
        blocks: want_blocks,
        profile_id: req.doc_profile.clone(),
    };
    let action_stats = action_proto::Stats::default();
    let action_failures = action_proto::Failures::default();
    let producer = Producer {
        abi_index: &sealed.actions,
        index_transfer_memo: req.index_transfer_memo,
        metadata_only: false,
        timestamps: svc.timestamps,
        docs: &docs,
        stats: &action_stats,
        failures: &action_failures,
    };
    let delta_cfg = DocConfig {
        profile: svc.profile.clone(),
        timestamps: svc.timestamps,
        doc_profile_id: req.doc_profile.clone(),
        chain_id: svc.chain_id.clone(),
        system_contract: svc.defaults.system_contract.clone(),
    };
    let delta_stats = delta_proto::wire::Stats::default();
    let delta_failures = delta_proto::wire::Failures::default();
    let trace_first = svc.trace.first_block();

    let mut digests = [Sha256::new(), Sha256::new(), Sha256::new()];
    let (mut counts, mut bytes) = ([0u64; 3], [0u64; 3]);
    let mut sequences: Option<(u64, u64)> = None;
    let (mut first_id, mut last_id) = (None, None);
    let mut blocks_seen = 0u64;
    let mut previous: Option<Segment> = None;
    let timeout = svc.credit_timeout;

    ordered_chunks(
        Chunks {
            from: req.from,
            to: req.to,
            size: CHUNK,
            threads: plan.threads,
            window: plan.threads + 2,
            limit: Some(&svc.window_limit),
        },
        || {
            Ok(Workers {
                actions: BlockWorker::new(producer),
                deltas: DeltaWorker::new(&delta_cfg, &sealed.deltas, &delta_stats, &delta_failures),
            })
        },
        |w, cs, ce| -> Result<ChunkOut> {
            let _permit = svc.permits.acquire();
            let ids = sealed.newest.block_ids(cs, ce)?;
            let blocks = svc.blocks.slab(cs, ce)?;
            let traced_from = cs.max(trace_first);
            let trace = if want_actions && traced_from <= ce {
                Some(svc.trace.slab(traced_from, ce)?)
            } else {
                None
            };
            let state = if want_deltas {
                Some(svc.state.slab(cs, ce)?)
            } else {
                None
            };
            let mut segment = None;
            let mut outs = Vec::with_capacity((ce - cs + 1) as usize);
            let mut docs: Vec<DocMsg> = Vec::new();
            let mut deltas = Vec::new();
            for block in cs..=ce {
                let id = ids[(block - cs) as usize];
                let bytes = blocks.block(block);
                let mut out = BlockOut {
                    block,
                    id,
                    parts: Default::default(),
                    counts: [0; 3],
                    sequences: None,
                };
                docs.clear();
                if want_actions && block >= trace_first {
                    let entry = trace.as_ref().unwrap().entry(block)?;
                    ensure!(
                        entry.id() == &id,
                        "block {block}: the trace log names {}, the catalog {}",
                        hex::encode(entry.id()),
                        hex::encode(id)
                    );
                    let input = match bytes {
                        Some(b) => BlockInput::Bytes(b),
                        None => BlockInput::Missing,
                    };
                    w.actions.traced(
                        block,
                        &id,
                        entry.payload,
                        input,
                        &mut segment,
                        Some(&mut docs),
                    )?;
                    out.sequences = w.actions.take_sequences();
                } else if want_blocks || want_actions {
                    let walked = w
                        .actions
                        .untraced(block, bytes, &mut segment, Some(&mut docs))?;
                    ensure!(
                        walked.id == id,
                        "block {block}: the block log's header hashes to {}, the catalog names {}",
                        hex::encode(walked.id),
                        hex::encode(id)
                    );
                } else {
                    let walked = action_proto::walk_block(bytes, block, Some(&id))?;
                    action_proto::chain_block(&mut segment, block, &walked)?;
                }
                for d in &docs {
                    let k = match d.kind {
                        DocKind::Block => 0,
                        DocKind::Action(_) => 1,
                    };
                    action_proto::push_bulk(&mut out.parts[k], &req.index, d);
                    out.counts[k] += 1;
                }
                if want_deltas {
                    let entry = state.as_ref().unwrap().entry(block)?;
                    ensure!(
                        entry.id() == &id,
                        "block {block}: the chain-state log names {}, the catalog {}",
                        hex::encode(entry.id()),
                        hex::encode(id)
                    );
                    deltas.clear();
                    w.deltas.block(
                        block,
                        &id,
                        entry.payload,
                        blocks.header(block),
                        Some(&mut deltas),
                    );
                    for d in &deltas {
                        delta_proto::wire::push_bulk(
                            &mut out.parts[2],
                            &req.index.chain,
                            &req.index.version,
                            req.index.partition_size,
                            d,
                        );
                        out.counts[2] += 1;
                    }
                }
                outs.push(out);
            }
            // Nothing a producer could not read may be left out silently.
            let unreadable =
                action_stats.unreadable.load(Relaxed) + delta_stats.unreadable.load(Relaxed);
            if unreadable > 0 {
                bail!(
                    "{unreadable} log entries could not be read (blocks {:?} {:?})",
                    action_stats.unreadable_blocks.lock().unwrap(),
                    delta_stats.unreadable_blocks.lock().unwrap()
                );
            }
            ensure!(
                delta_stats.no_header.load(Relaxed) == 0,
                "a block in {cs}..={ce} has no block-log header"
            );
            match segment {
                Some(run) if run.first == cs && run.last == ce => {}
                other => bail!(
                    "chunk [{cs}..{ce}] walked {}",
                    other.map_or("no blocks".to_string(), |run| format!(
                        "only [{}..{}]",
                        run.first, run.last
                    ))
                ),
            }
            Ok(ChunkOut {
                blocks: outs,
                segment,
            })
        },
        |_, _, chunk| {
            // Chunks chain into one run, and the first names the block before the range.
            let segment = chunk.segment.unwrap();
            let expected_previous = match previous {
                Some(before) => {
                    ensure!(segment.first == before.last + 1, "chunks out of order");
                    before.last_id
                }
                None => plan.previous.unwrap_or([0; 32]),
            };
            ensure!(
                segment.first_previous == expected_previous,
                "block {} names {} as its parent, but block {} is {}",
                segment.first,
                hex::encode(segment.first_previous),
                segment.first - 1,
                hex::encode(expected_previous)
            );
            previous = Some(segment);
            for out in chunk.blocks {
                if let Some(c) = credits {
                    // Everything granted so far reaches the client before the stream waits.
                    if !c.available() {
                        body.flush()?;
                    }
                    c.take(timeout)?;
                }
                let len: usize = out.parts.iter().map(String::len).sum();
                let mut header = format!(
                    "{{\"frame\":\"block\",\"block\":{},\"id\":\"{}\",\"bytes\":{len},\"docs\":{{",
                    out.block,
                    hex::encode(out.id)
                );
                let mut first = true;
                for (k, kind) in KINDS.iter().enumerate() {
                    if !req.kinds[k] {
                        continue;
                    }
                    if !first {
                        header.push(',');
                    }
                    first = false;
                    header.push_str(&format!(
                        "\"{kind}\":[{},{}]",
                        out.counts[k],
                        out.parts[k].len()
                    ));
                }
                header.push_str("}}");
                body.line(&header)?;
                for (k, part) in out.parts.iter().enumerate() {
                    digests[k].update(part.as_bytes());
                    bytes[k] += part.len() as u64;
                    counts[k] += out.counts[k];
                    body.buf.extend_from_slice(part.as_bytes());
                }
                if body.buf.len() >= FLUSH {
                    body.flush()?;
                }
                if let Some((low, high)) = out.sequences {
                    sequences = Some(match sequences {
                        Some((l, h)) => (l.min(low), h.max(high)),
                        None => (low, high),
                    });
                }
                first_id.get_or_insert(out.id);
                last_id = Some(out.id);
                blocks_seen += 1;
            }
            Ok(())
        },
    )?;

    let mut per_kind = serde_json::Map::new();
    for (k, digest) in digests.into_iter().enumerate() {
        if req.kinds[k] {
            per_kind.insert(
                KINDS[k].into(),
                json!({"docs":counts[k],"bytes":bytes[k],"sha256":hex::encode(digest.finalize())}),
            );
        }
    }
    let seq = |v: Option<u64>| v.map_or(Value::Null, |n| Value::from(n.to_string()));
    let mut skipped = serde_json::Map::new();
    if want_actions {
        skipped.insert(
            "actionsUndecoded".into(),
            json!(action_stats.raw.load(Relaxed)),
        );
    }
    if want_deltas {
        skipped.insert(
            "deltaRows".into(),
            json!({"noAbi":delta_stats.no_abi.load(Relaxed),"noTable":delta_stats.no_table.load(Relaxed),
                "decode":delta_stats.decode_fail.load(Relaxed),"handler":delta_stats.handler_fail.load(Relaxed),
                "badRow":delta_stats.bad_row.load(Relaxed)}),
        );
    }
    Ok(
        json!({"frame":"manifest","from":req.from,"to":req.to,"blocks":blocks_seen,
        "catalogSha":plan.catalog_sha,"previousBlockId":id_or_null(plan.previous),
        "firstBlockId":id_or_null(first_id),"lastBlockId":id_or_null(last_id),
        "kinds":per_kind,
        "firstGlobalSequence":if want_actions {seq(sequences.map(|s| s.0))} else {Value::Null},
        "lastGlobalSequence":if want_actions {seq(sequences.map(|s| s.1))} else {Value::Null},
        "skipped":skipped}),
    )
}
