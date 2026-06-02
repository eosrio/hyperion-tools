//! archive-server — an on-demand "historical archive" HTTP server.
//!
//! The keystone of a tiered-storage design: Elasticsearch keeps only indexed *metadata* for old
//! blocks (block_num, global_sequence, account, name, …), while action `act.data` payloads and
//! contract_row delta values are served straight from frozen, compressed state-history logs. The
//! data is never duplicated — one archive process fronts one frozen block range, decoding payloads
//! lazily, exactly when a request for them arrives.
//!
//! Mechanics (reused verbatim from `action-proto`, kept self-contained here per the bin contract):
//!   * Seek the index at `(block_num - first_block) * 8` -> 8-byte log offset.
//!   * Seek the log there, read the 48-byte entry header (block_num = BE u32 at [8..12],
//!     payload_size = LE u64 at [40..48]), read the payload.
//!   * `disk::decode_payload` (zlib inflate) -> the raw `transaction_trace[]` bytes.
//!   * `trace::parse_block` -> `Vec<Tx>`; each `act.data` is an `(offset,len)` range into the
//!     inflated buffer.
//!   * Decode `act.data` against the *contract* ABI active at the block (greatest `valid_from <= N`,
//!     same range-query as action-proto's `Registry`). On any decode failure, return the raw
//!     uppercase hex under a `"hex"` key — every request still gets an answer.
//!
//! Concurrency: a pool of `--threads` worker threads each call `Server::recv` (tiny_http
//! synchronises that internally) and each own their File handles, an inline `Registry`
//! (`AbiHandle` is Send-not-Sync, so never shared), an `Abieos` for `string_to_name`, and a small
//! per-thread LRU-ish block cache so `/block/<N>` followed by `/action?block_num=N` (or any nearby
//! request) skips the re-inflate + re-parse. The `Arc<AbiIndex>` is shared read-only.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use clap::Parser;
use rs_abieos::{AbiHandle, Abieos};
use tiny_http::{Header, Request, Response, Server};

use hyperion_ship::delta::read_varuint;
use hyperion_ship::disk::{decode_payload, is_ship_magic};
use hyperion_ship::trace::{self, Tx};

#[derive(Parser, Debug)]
#[command(
    about = "On-demand historical archive: decode action act.data and contract_row values from frozen state-history logs over HTTP."
)]
struct Args {
    /// nodeos state-history dir (must contain trace_history.{log,index}; chain_state_history.{log,index} enables /deltas).
    #[arg(long)]
    from_disk: String,
    /// abi-index NDJSON produced by abi-scanner ({account, block, abi_hex, ...}).
    #[arg(long)]
    abi_index: String,
    /// TCP port to listen on.
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// number of worker threads serving requests.
    #[arg(long, default_value_t = 8)]
    threads: u32,
}

// ---------------------------------------------------------------------------------------------
// ABI index + per-thread contract-ABI registry (copied inline from action-proto, re-keyed by the
// u64 `account` name — AbiHandle is Send-not-Sync, so each worker owns its own Registry).
// ---------------------------------------------------------------------------------------------

/// account (u64 name) -> versions sorted by the block the ABI took effect (valid_from).
type AbiIndex = HashMap<u64, Vec<(u32, String)>>;

fn load_abi_index(path: &str) -> Result<AbiIndex> {
    use serde_json::Value;
    use std::io::BufRead;
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
        eprintln!("[archive-server] skipped {skipped} malformed ABI-index line(s)");
    }
    Ok(idx)
}

/// Per-worker cache of parsed contract ABIs, backed by the shared (immutable) version index.
/// (Identical lookup to action-proto's `Registry`: range-query for the version active at `block`.)
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

    /// The contract ABI active at `block` for `code` (greatest `valid_from <= block`), parsed once
    /// and cached. `None` if no version is on file at/before `block` (or it failed to parse).
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

/// Decode one `act.data` (raw bytes) for `action` (u64 name) of contract `code` against the ABI
/// version active at `block`, writing JSON into `out`. `Err(())` on no-ABI / no-type / decode-fail.
fn decode_action(
    reg: &mut Registry,
    out: &mut String,
    code: u64,
    action: u64,
    data: &[u8],
    block: u32,
) -> std::result::Result<(), ()> {
    let handle = reg.active(code, block).ok_or(())?;
    let ty = handle.type_for_action(action).ok_or(())?.to_owned();
    handle.bin_to_json_into(&ty, data, out).map_err(|_| ())
}

/// Decode one contract_row `value` (raw bytes) for `table` (u64 name) of contract `code` against
/// the ABI version active at `block`, writing the row JSON into `out`. `Err(())` on no-ABI /
/// no-type-for-table / decode-fail. The DELTA analog of `decode_action` (port of delta-proto's
/// `decode_at`): `decode_table_row_into` looks up the table's row struct type in the ABI and
/// deserializes against it, with zero FFI on the rs_abieos rust-backend.
fn decode_delta_row(
    reg: &mut Registry,
    out: &mut String,
    code: u64,
    table: u64,
    value: &[u8],
    block: u32,
) -> std::result::Result<(), ()> {
    let handle = reg.active(code, block).ok_or(())?;
    handle
        .decode_table_row_into(table, value, out)
        .map_err(|_| ())
}

// ---------------------------------------------------------------------------------------------
// Delta (chain_state_history) row walking — ported verbatim from delta-proto. The inflated
// payload is a `table_delta[]`; we walk it, parse `contract_row` rows, and key them by
// (code, scope, table, primary_key) for the /deltas batch lookup.
// ---------------------------------------------------------------------------------------------

/// Walk every table_delta row, calling `f(table_name, present, row_data)`. (Port of delta-proto.)
fn for_each_row<F: FnMut(&[u8], u8, &[u8])>(deltas: &[u8], mut f: F) -> Option<()> {
    let mut off = 0usize;
    let (n_tables, k) = read_varuint(deltas.get(off..)?)?;
    off += k;
    for _ in 0..n_tables {
        let (_var, k) = read_varuint(deltas.get(off..)?)?;
        off += k;
        let (name_len, k) = read_varuint(deltas.get(off..)?)?;
        off += k;
        let name = deltas.get(off..off + name_len)?;
        off += name_len;
        let (rows, k) = read_varuint(deltas.get(off..)?)?;
        off += k;
        for _ in 0..rows {
            let present = *deltas.get(off)?;
            off += 1;
            let (dlen, k) = read_varuint(deltas.get(off..)?)?;
            off += k;
            let data = deltas.get(off..off + dlen)?;
            f(name, present, data);
            off += dlen;
        }
    }
    Some(())
}

/// Parse a `contract_row_v0` header to its (code, scope, table, pk, payer) names + the `value`
/// length, returning the byte offset of `value` within `row` and its length. (Port of delta-proto's
/// `parse_contract_row`, but returns an offset rather than a borrowed slice so the caller can map it
/// back into the whole inflated buffer.)
/// Layout: [varuint 0][code u64][scope u64][table u64][pk u64][payer u64][varuint vlen][value].
fn parse_contract_row(row: &[u8]) -> Option<(u64, u64, u64, u64, usize, usize)> {
    let (_v, k) = read_varuint(row)?;
    let mut off = k;
    let rd = |off: &mut usize| -> Option<u64> {
        let n = u64::from_le_bytes(row.get(*off..*off + 8)?.try_into().ok()?);
        *off += 8;
        Some(n)
    };
    let code = rd(&mut off)?;
    let scope = rd(&mut off)?;
    let table = rd(&mut off)?;
    let primary_key = rd(&mut off)?;
    let _payer = rd(&mut off)?;
    let (vlen, k) = read_varuint(row.get(off..)?)?;
    off += k;
    // bounds-check the value range exists, but return its position (not the slice)
    row.get(off..off + vlen)?;
    Some((code, scope, table, primary_key, off, vlen))
}

// ---------------------------------------------------------------------------------------------
// Frozen-log facts (read once at startup) and per-entry reader.
// ---------------------------------------------------------------------------------------------

/// Immutable facts about a frozen state-history log (trace_history or chain_state_history),
/// shared read-only across workers.
struct LogInfo {
    log_path: String,
    idx_path: String,
    first_block: u32,
    last_block: u32,
}

/// A single block read off disk: the inflated `transaction_trace[]` bytes and the parsed `Vec<Tx>`
/// (whose `act.data`/`return_value` ranges index back into `inflated`). Held together so the cache
/// can keep both without dangling the borrowed ranges.
struct BlockData {
    inflated: Vec<u8>,
    txs: Vec<Tx>,
}

/// Per-thread handles + reusable buffers for pulling one block out of the frozen log.
struct LogReader {
    idx: File,
    log: File,
    first_block: u32,
}

impl LogReader {
    fn open(info: &LogInfo) -> Result<Self> {
        Ok(Self {
            idx: File::open(&info.idx_path)?,
            log: File::open(&info.log_path)?,
            first_block: info.first_block,
        })
    }

    /// Read + inflate + parse block `n`. Returns `Ok(None)` if the entry inflates empty or the
    /// trace payload won't parse (treated as "no such block content"); `Err` only on I/O / format
    /// faults. The caller is responsible for the `[first_block, last_block]` range check.
    fn read_block(&mut self, n: u32) -> Result<Option<BlockData>> {
        // index entry -> log offset
        self.idx
            .seek(SeekFrom::Start((n - self.first_block) as u64 * 8))?;
        let mut ob = [0u8; 8];
        self.idx.read_exact(&mut ob)?;
        let pos = u64::from_le_bytes(ob);

        // entry header at that offset
        self.log.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 48];
        self.log.read_exact(&mut hdr)?;
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        if block_num != n {
            bail!(
                "index/log mismatch: asked block {n}, entry at offset {pos} is block {block_num}"
            );
        }
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        let log_len = self.log.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        if payload_size > log_len {
            bail!("payload_size {payload_size} at block {n} exceeds log length {log_len}");
        }
        let mut payload = vec![0u8; payload_size as usize];
        self.log.read_exact(&mut payload)?;

        let inflated = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => return Ok(None), // empty entry — no traces
            Err(e) => bail!("inflate block {n}: {e}"),
        };
        let Some(txs) = trace::parse_block(&inflated) else {
            return Ok(None); // unparsable trace payload — treat as empty
        };
        Ok(Some(BlockData { inflated, txs }))
    }
}

/// Tiny per-thread block cache: a bounded ring of recently-read blocks so repeated/nearby requests
/// skip the seek + inflate + parse. `None` slots mean "read but had no content" (cache the miss).
struct BlockCache {
    cap: usize,
    slots: Vec<(u32, Option<BlockData>)>,
    next: usize,
}

impl BlockCache {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            slots: Vec::with_capacity(cap.max(1)),
            next: 0,
        }
    }

    fn find(&self, n: u32) -> Option<usize> {
        self.slots.iter().position(|(b, _)| *b == n)
    }

    fn insert(&mut self, n: u32, data: Option<BlockData>) -> usize {
        if let Some(i) = self.find(n) {
            self.slots[i].1 = data;
            return i;
        }
        if self.slots.len() < self.cap {
            self.slots.push((n, data));
            self.slots.len() - 1
        } else {
            let i = self.next;
            self.slots[i] = (n, data);
            self.next = (self.next + 1) % self.cap;
            i
        }
    }

    /// Read `n` from cache, or via `reader`, caching the result. Returns the slot index, or `Err`
    /// only on a genuine I/O/format fault from `reader`.
    fn get_or_read(&mut self, n: u32, reader: &mut LogReader) -> Result<usize> {
        if let Some(i) = self.find(n) {
            return Ok(i);
        }
        let data = reader.read_block(n)?;
        Ok(self.insert(n, data))
    }
}

// ---------------------------------------------------------------------------------------------
// Delta block reader + cache (chain_state_history). Mirrors the trace LogReader/BlockCache, but the
// inflated payload is the raw `table_delta[]` bytes and the parsed product is a per-block map keyed
// by (code, scope, table, primary_key) -> the `value` byte-range, built ONCE per block at read time.
// ---------------------------------------------------------------------------------------------

/// A single chain_state_history block read off disk: the inflated `table_delta[]` bytes plus a map
/// from each present `contract_row`'s (code, scope, table, primary_key) to the byte-range of its
/// `value` within `inflated`. Held together so the cache keeps both without dangling the ranges.
struct DeltaBlockData {
    inflated: Vec<u8>,
    /// (code, scope_name, table, primary_key) -> (value_offset, value_len) into `inflated`.
    /// `scope` is keyed by its NAME STRING (`trace::name_to_string`) — the exact form Hyperion stores
    /// in the delta doc / `_id` and that the request echoes back. Antelope name encoding is NOT
    /// bijective over arbitrary integer scopes (legal on WAX/EOS), so keying on the raw u64 would miss
    /// those rows. `code`/`table` are always real names (round-trip safe), so they stay u64.
    rows: HashMap<(u64, String, u64, u64), (usize, usize)>,
}

/// Per-thread handles for pulling one block out of the frozen chain_state_history log.
struct DeltaReader {
    idx: File,
    log: File,
    first_block: u32,
}

impl DeltaReader {
    fn open(info: &LogInfo) -> Result<Self> {
        Ok(Self {
            idx: File::open(&info.idx_path)?,
            log: File::open(&info.log_path)?,
            first_block: info.first_block,
        })
    }

    /// Read + inflate + walk block `n`'s contract_row deltas into a key->value-range map. Returns
    /// `Ok(None)` if the entry inflates empty or the delta payload won't walk (treated as "no such
    /// block content"); `Err` only on I/O / format faults. Caller checks the range first.
    fn read_block(&mut self, n: u32) -> Result<Option<DeltaBlockData>> {
        // index entry -> log offset
        self.idx
            .seek(SeekFrom::Start((n - self.first_block) as u64 * 8))?;
        let mut ob = [0u8; 8];
        self.idx.read_exact(&mut ob)?;
        let pos = u64::from_le_bytes(ob);

        // entry header at that offset
        self.log.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 48];
        self.log.read_exact(&mut hdr)?;
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        if block_num != n {
            bail!(
                "index/log mismatch: asked block {n}, entry at offset {pos} is block {block_num}"
            );
        }
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap());
        let log_len = self.log.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        if payload_size > log_len {
            bail!("payload_size {payload_size} at block {n} exceeds log length {log_len}");
        }
        let mut payload = vec![0u8; payload_size as usize];
        self.log.read_exact(&mut payload)?;

        let inflated = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => return Ok(None), // empty entry — no deltas
            Err(e) => bail!("inflate block {n}: {e}"),
        };

        // Walk the table_delta[] once, recording every present contract_row's value range. The
        // ranges are offsets into `inflated`, which we keep alongside the map. A truncated/garbage
        // delta returns None from for_each_row -> treat as no content.
        let mut rows: HashMap<(u64, String, u64, u64), (usize, usize)> = HashMap::new();
        let base = inflated.as_ptr() as usize;
        let walked = for_each_row(&inflated, |name, present, data| {
            if name != b"contract_row" || present != 1 {
                return;
            }
            let Some((code, scope, table, primary_key, voff_in_row, vlen)) =
                parse_contract_row(data)
            else {
                return;
            };
            // `data` is a sub-slice of `inflated`; convert the value's in-row offset to an absolute
            // offset into `inflated` via pointer arithmetic (both live in the same allocation).
            let value_off = (data.as_ptr() as usize - base) + voff_in_row;
            // First write for a given key wins (a block normally has one present row per key). The
            // scope is keyed by its name string so it matches the request's echoed scope exactly.
            rows.entry((code, trace::name_to_string(scope), table, primary_key))
                .or_insert((value_off, vlen));
        });
        if walked.is_none() {
            return Ok(None); // unparsable delta payload — treat as empty
        }
        if rows.is_empty() {
            return Ok(None); // no present contract_row deltas in this block
        }
        Ok(Some(DeltaBlockData { inflated, rows }))
    }
}

/// Tiny per-thread delta-block cache: a bounded ring of recently-read delta blocks. `None` slots
/// mean "read but had no contract_row content" (cache the miss). Mirrors `BlockCache`.
struct DeltaCache {
    cap: usize,
    slots: Vec<(u32, Option<DeltaBlockData>)>,
    next: usize,
}

impl DeltaCache {
    fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            slots: Vec::with_capacity(cap.max(1)),
            next: 0,
        }
    }

    fn find(&self, n: u32) -> Option<usize> {
        self.slots.iter().position(|(b, _)| *b == n)
    }

    fn insert(&mut self, n: u32, data: Option<DeltaBlockData>) -> usize {
        if let Some(i) = self.find(n) {
            self.slots[i].1 = data;
            return i;
        }
        if self.slots.len() < self.cap {
            self.slots.push((n, data));
            self.slots.len() - 1
        } else {
            let i = self.next;
            self.slots[i] = (n, data);
            self.next = (self.next + 1) % self.cap;
            i
        }
    }

    /// Read `n` from cache, or via `reader`, caching the result. Returns the slot index, or `Err`
    /// only on a genuine I/O/format fault from `reader`.
    fn get_or_read(&mut self, n: u32, reader: &mut DeltaReader) -> Result<usize> {
        if let Some(i) = self.find(n) {
            return Ok(i);
        }
        let data = reader.read_block(n)?;
        Ok(self.insert(n, data))
    }
}

// ---------------------------------------------------------------------------------------------
// Minimal JSON helpers (string escaping + name rendering) — kept inline, no serde re-serialize.
// ---------------------------------------------------------------------------------------------

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

/// Serialize the act.data field value: decoded JSON if the ABI handled it, else `{"hex":"..."}`.
/// Appends to `out`. Uses a per-call scratch `String` for the decode (reused across actions).
fn append_data_value(
    out: &mut String,
    scratch: &mut String,
    reg: &mut Registry,
    code: u64,
    action: u64,
    data: &[u8],
    block: u32,
) {
    scratch.clear();
    if decode_action(reg, scratch, code, action, data, block).is_ok() {
        out.push_str(scratch);
    } else {
        out.push_str("{\"hex\":");
        push_json_str(out, &hex::encode_upper(data));
        out.push('}');
    }
}

/// Serialize a receipt object (mirrors action-proto's `receipts[]` element, but standalone).
fn append_receipt(out: &mut String, names: &Abieos, r: &trace::Receipt) {
    out.push_str("{\"receiver\":");
    push_json_str(out, &name_str(names, r.receiver));
    out.push_str(",\"global_sequence\":\"");
    out.push_str(&r.global_sequence.to_string());
    out.push_str("\",\"recv_sequence\":\"");
    out.push_str(&r.recv_sequence.to_string());
    out.push_str("\",\"auth_sequence\":[");
    for (i, (a, sq)) in r.auth_sequence.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"account\":");
        push_json_str(out, &name_str(names, *a));
        out.push_str(",\"sequence\":\"");
        out.push_str(&sq.to_string());
        out.push_str("\"}");
    }
    out.push_str("],\"code_sequence\":");
    out.push_str(&r.code_sequence.to_string());
    out.push_str(",\"abi_sequence\":");
    out.push_str(&r.abi_sequence.to_string());
    out.push_str(",\"act_digest\":");
    push_json_str(out, &hex::encode_upper(r.act_digest));
    out.push('}');
}

/// Render a u64 `name` to its string. Uses rs_abieos if it can (matches abi-index keying), else
/// falls back to the local charmap decode — both produce the standard Antelope name string.
fn name_str(names: &Abieos, n: u64) -> String {
    names
        .name_to_string(n)
        .unwrap_or_else(|_| trace::name_to_string(n))
}

// ---------------------------------------------------------------------------------------------
// Request handling.
// ---------------------------------------------------------------------------------------------

/// Per-worker mutable state. The trace path (`reader`/`cache`, for /action,/block,/actions) and
/// the delta path (`delta_reader`/`delta_cache`, for /deltas) each own their own File handles +
/// block cache; the contract-ABI `reg` is shared by both (same per-block ABI lookup).
struct Worker<'a> {
    reader: LogReader,
    cache: BlockCache,
    /// `None` when this archive was started without a chain_state_history log (actions-only); in that
    /// case POST /deltas answers 503 and the trace/GET endpoints are unaffected.
    delta_reader: Option<DeltaReader>,
    delta_cache: DeltaCache,
    reg: Registry<'a>,
    names: Abieos,
    scratch: String,
}

/// The HTTP outcome of a handler: a status code and a (already-serialized) body.
struct Reply {
    code: u16,
    body: String,
    json: bool,
}

impl Reply {
    fn json(code: u16, body: String) -> Self {
        Self {
            code,
            body,
            json: true,
        }
    }
    fn err(code: u16, msg: &str) -> Self {
        let mut b = String::new();
        b.push_str("{\"error\":");
        push_json_str(&mut b, msg);
        b.push('}');
        Self {
            code,
            body: b,
            json: true,
        }
    }
}

/// Parse a `k=v&k=v` query string into a tiny map (last value wins). No percent-decoding needed:
/// our only params are decimal integers.
fn parse_query(q: &str) -> HashMap<&str, &str> {
    let mut m = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        m.insert(k, v);
    }
    m
}

/// `GET /health` — liveness plus the archived block ranges this server can serve, so integrators
/// can discover coverage instead of probing for it. `deltas` is `null` when this node has no
/// `chain_state_history` log (contract_row hydration / `POST /deltas` unavailable).
fn handle_health(info: &LogInfo, delta_info: Option<&LogInfo>) -> Reply {
    let range =
        |i: &LogInfo| serde_json::json!({"first_block": i.first_block, "last_block": i.last_block});
    let body = serde_json::json!({
        "status": "ok",
        "actions": range(info),
        "deltas": delta_info.map(range),
    });
    Reply::json(200, body.to_string())
}

/// `GET /action?block_num=N&global_sequence=G` — find the single action_trace in block N whose
/// receipt.global_sequence == G, decode its act.data, return one JSON object.
fn handle_action(w: &mut Worker, info: &LogInfo, query: &str) -> Reply {
    let params = parse_query(query);
    let (Some(bn), Some(gs)) = (params.get("block_num"), params.get("global_sequence")) else {
        return Reply::err(400, "missing block_num or global_sequence");
    };
    let Ok(block_num) = bn.parse::<u32>() else {
        return Reply::err(400, "block_num must be a u32");
    };
    let Ok(global_sequence) = gs.parse::<u64>() else {
        return Reply::err(400, "global_sequence must be a u64");
    };
    if block_num < info.first_block || block_num > info.last_block {
        return Reply::err(
            416,
            &format!(
                "block {block_num} out of archived range [{}..{}]",
                info.first_block, info.last_block
            ),
        );
    }

    let slot = match w.cache.get_or_read(block_num, &mut w.reader) {
        Ok(i) => i,
        Err(e) => return Reply::err(500, &format!("read block {block_num}: {e}")),
    };
    let Some(block) = w.cache.slots[slot].1.as_ref() else {
        return Reply::err(404, "no actions for that block");
    };

    // Find the matching action_trace, capturing only the lightweight descriptors we need so the
    // immutable `block` borrow ends before we touch the registry (another field of the worker).
    let mut found: Option<(u64, u64, usize, usize)> = None; // (code, name, data_off, data_len)
    'outer: for tx in &block.txs {
        for act in &tx.actions {
            if act.except {
                continue;
            }
            let Some(receipt) = &act.receipt else {
                continue;
            };
            if receipt.global_sequence == global_sequence {
                found = Some((act.account, act.name, act.data.0, act.data.1));
                break 'outer;
            }
        }
    }
    let Some((code, action, off, len)) = found else {
        return Reply::err(404, "no action with that global_sequence in block");
    };

    let t0 = Instant::now();
    let mut body = String::with_capacity(256 + len * 2);
    body.push_str("{\"block_num\":");
    body.push_str(&block_num.to_string());
    body.push_str(",\"global_sequence\":");
    body.push_str(&global_sequence.to_string());
    body.push_str(",\"account\":");
    push_json_str(&mut body, &name_str(&w.names, code));
    body.push_str(",\"name\":");
    push_json_str(&mut body, &name_str(&w.names, action));
    body.push_str(",\"data\":");
    {
        // Copy the act.data slice out of the cached block (releases the `w.cache` borrow) so we can
        // then mutably split the disjoint `reg`/`scratch` fields of the worker for the decode.
        let data: Vec<u8> =
            w.cache.slots[slot].1.as_ref().unwrap().inflated[off..off + len].to_vec();
        let Worker { reg, scratch, .. } = w;
        append_data_value(&mut body, scratch, reg, code, action, &data, block_num);
    }
    body.push_str(",\"decode_us\":");
    body.push_str(&t0.elapsed().as_micros().to_string());
    body.push('}');
    Reply::json(200, body)
}

/// `GET /block/<N>` — return ALL action_traces (with receipt) of block N as a JSON array.
fn handle_block(w: &mut Worker, info: &LogInfo, n: u32) -> Reply {
    if n < info.first_block || n > info.last_block {
        return Reply::err(
            416,
            &format!(
                "block {n} out of archived range [{}..{}]",
                info.first_block, info.last_block
            ),
        );
    }
    let slot = match w.cache.get_or_read(n, &mut w.reader) {
        Ok(i) => i,
        Err(e) => return Reply::err(500, &format!("read block {n}: {e}")),
    };
    if w.cache.slots[slot].1.is_none() {
        // No traces for this block — a valid, empty answer.
        return Reply::json(200, "[]".to_string());
    }

    // Collect lightweight action descriptors up front so we can release the cache borrow before
    // decoding (which borrows the registry, another Worker field).
    let mut body = String::new();
    body.push('[');
    let mut first = true;

    // Walk the cached block; for each emitted action, decode act.data inline. We split Worker's
    // borrows: `block` from the cache, `reg/names/scratch` for decoding — disjoint fields.
    // To satisfy the borrow checker with the cache and registry both on `w`, gather lightweight
    // descriptors first, then decode in a second pass using fresh slices.
    struct ActRef {
        code: u64,
        name: u64,
        data: (usize, usize),
        ret: Option<(usize, usize)>,
        receiver: u64,
        action_ordinal: u32,
        creator_action_ordinal: u32,
        context_free: bool,
        elapsed: i64,
        authorization: Vec<(u64, u64)>,
        account_ram_deltas: Vec<(u64, i64)>,
        receipt: trace::Receipt,
        trx_id: [u8; 32],
    }
    let mut refs: Vec<ActRef> = Vec::new();
    {
        let block = w.cache.slots[slot].1.as_ref().unwrap();
        for tx in &block.txs {
            for act in &tx.actions {
                if act.except {
                    continue;
                }
                let Some(receipt) = &act.receipt else {
                    continue;
                };
                refs.push(ActRef {
                    code: act.account,
                    name: act.name,
                    data: act.data,
                    ret: act.return_value,
                    receiver: act.receiver,
                    action_ordinal: act.action_ordinal,
                    creator_action_ordinal: act.creator_action_ordinal,
                    context_free: act.context_free,
                    elapsed: act.elapsed,
                    authorization: act.authorization.clone(),
                    account_ram_deltas: act.account_ram_deltas.clone(),
                    receipt: trace::Receipt {
                        receiver: receipt.receiver,
                        act_digest: receipt.act_digest,
                        global_sequence: receipt.global_sequence,
                        recv_sequence: receipt.recv_sequence,
                        auth_sequence: receipt.auth_sequence.clone(),
                        code_sequence: receipt.code_sequence,
                        abi_sequence: receipt.abi_sequence,
                    },
                    trx_id: tx.id,
                });
            }
        }
    }

    for a in &refs {
        if !first {
            body.push(',');
        }
        first = false;
        body.push_str("{\"trx_id\":");
        push_json_str(&mut body, &hex::encode(a.trx_id));
        body.push_str(",\"action_ordinal\":");
        body.push_str(&a.action_ordinal.to_string());
        body.push_str(",\"creator_action_ordinal\":");
        body.push_str(&a.creator_action_ordinal.to_string());
        body.push_str(",\"receiver\":");
        push_json_str(&mut body, &name_str(&w.names, a.receiver));
        if a.context_free {
            body.push_str(",\"context_free\":true");
        }
        if a.elapsed != 0 {
            body.push_str(",\"elapsed\":\"");
            body.push_str(&a.elapsed.to_string());
            body.push('"');
        }
        // account_ram_deltas
        let mut ram_open = false;
        for (acc, d) in &a.account_ram_deltas {
            if *d == 0 {
                continue;
            }
            body.push_str(if ram_open {
                ","
            } else {
                ",\"account_ram_deltas\":["
            });
            ram_open = true;
            body.push_str("{\"account\":");
            push_json_str(&mut body, &name_str(&w.names, *acc));
            body.push_str(",\"delta\":\"");
            body.push_str(&d.to_string());
            body.push_str("\"}");
        }
        if ram_open {
            body.push(']');
        }
        // return_value (v1 traces)
        if let Some((roff, rlen)) = a.ret {
            if rlen > 0 {
                let block = w.cache.slots[slot].1.as_ref().unwrap();
                body.push_str(",\"return_value\":");
                push_json_str(
                    &mut body,
                    &hex::encode_upper(&block.inflated[roff..roff + rlen]),
                );
            }
        }
        // act { account, name, authorization, data }
        body.push_str(",\"act\":{\"account\":");
        push_json_str(&mut body, &name_str(&w.names, a.code));
        body.push_str(",\"name\":");
        push_json_str(&mut body, &name_str(&w.names, a.name));
        body.push_str(",\"authorization\":[");
        for (i, (actor, perm)) in a.authorization.iter().enumerate() {
            if i > 0 {
                body.push(',');
            }
            body.push_str("{\"actor\":");
            push_json_str(&mut body, &name_str(&w.names, *actor));
            body.push_str(",\"permission\":");
            push_json_str(&mut body, &name_str(&w.names, *perm));
            body.push('}');
        }
        body.push_str("],\"data\":");
        {
            let (off, len) = a.data;
            // Copy the slice out first (releases the `w.cache` borrow), then mutably split the
            // disjoint `reg`/`scratch` fields for the decode.
            let owned: Vec<u8> =
                w.cache.slots[slot].1.as_ref().unwrap().inflated[off..off + len].to_vec();
            let Worker { reg, scratch, .. } = w;
            append_data_value(&mut body, scratch, reg, a.code, a.name, &owned, n);
        }
        body.push('}');
        // receipt
        body.push_str(",\"receipt\":");
        append_receipt(&mut body, &w.names, &a.receipt);
        body.push('}');
    }
    body.push(']');
    Reply::json(200, body)
}

/// One parsed batch request item: which (block, global_sequence) pair to hydrate.
struct BatchItem {
    block_num: u64,
    global_sequence: u64,
}

/// A resolved batch entry, captured during the per-block pass so the cache borrow can be released
/// before decoding (which borrows the registry — a disjoint Worker field). `data` is the owned
/// act.data slice copied out of the inflated block.
struct ResolvedAction {
    block_num: u32,
    global_sequence: u64,
    code: u64,
    name: u64,
    data: Vec<u8>,
}

/// Maximum number of items accepted in a single `POST /actions` batch.
const MAX_BATCH_ITEMS: usize = 20_000;
/// Hard cap on the request body we will buffer (defends against an oversized/garbage POST before we
/// even parse). 64 MiB comfortably holds 20k small JSON items.
const MAX_BATCH_BODY_BYTES: u64 = 64 * 1024 * 1024;
/// Defense-in-depth bounds on the per-request DECODE work — distinct from the item/body caps above,
/// which bound only *parsing*. The expensive axis of `POST /actions` is the number of DISTINCT
/// blocks it must seek + inflate + parse, and a caller controls that almost independently of body
/// size (up to MAX_BATCH_ITEMS distinct blocks in a ~1 MB body). Without a bound, one cheap request
/// could pin a worker thread for minutes (uncancellable — nothing observes a dropped client socket).
/// So we cap both the number of distinct blocks resolved per request and the wall-clock time spent
/// resolving them; any positions not reached simply stay `found:false`. That is contract-safe and
/// best-effort: the client treats a short / `found:false` entry exactly as "no payload available".
const MAX_BLOCKS_PER_BATCH: usize = 4096;
const BATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);

/// Extract a u64 from a JSON value that may be a number or a decimal string (global_sequence arrives
/// either way over the wire). Returns `None` for anything else.
fn json_u64(v: &serde_json::Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    v.as_str().and_then(|s| s.trim().parse::<u64>().ok())
}

/// Read the request body bounded to `MAX_BATCH_BODY_BYTES` and parse it as a JSON array, applying the
/// shared caps: 400 on read failure / malformed JSON / non-array, 413 on over-size body or
/// over-`MAX_BATCH_ITEMS`. Used by both `/actions` and `/deltas`.
fn read_batch_array(req: &mut Request) -> std::result::Result<Vec<serde_json::Value>, Reply> {
    // Read the body, bounded so a runaway POST can't exhaust memory. `Content-Length` is advisory;
    // we cap the actual read regardless.
    let mut buf = Vec::new();
    let reader = req.as_reader();
    if reader
        .take(MAX_BATCH_BODY_BYTES + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        return Err(Reply::err(400, "failed to read request body"));
    }
    if buf.len() as u64 > MAX_BATCH_BODY_BYTES {
        return Err(Reply::err(413, "request body too large"));
    }

    let value: serde_json::Value =
        serde_json::from_slice(&buf).map_err(|_| Reply::err(400, "malformed JSON body"))?;
    let serde_json::Value::Array(arr) = value else {
        return Err(Reply::err(400, "request body must be a JSON array"));
    };
    if arr.len() > MAX_BATCH_ITEMS {
        return Err(Reply::err(
            413,
            &format!("too many items (max {MAX_BATCH_ITEMS})"),
        ));
    }
    Ok(arr)
}

/// Parse the JSON array request body into `BatchItem`s. `Err(Reply)` carries the right status code:
/// 400 for malformed/over-size body or bad item shape, 413 when the array exceeds `MAX_BATCH_ITEMS`.
fn parse_batch_body(req: &mut Request) -> std::result::Result<Vec<BatchItem>, Reply> {
    let arr = read_batch_array(req)?;

    let mut items = Vec::with_capacity(arr.len());
    for el in &arr {
        let (Some(bn), Some(gs)) = (
            el.get("block_num").and_then(|x| x.as_u64()),
            el.get("global_sequence").and_then(json_u64),
        ) else {
            return Err(Reply::err(
                400,
                "each item needs numeric block_num and number-or-string global_sequence",
            ));
        };
        items.push(BatchItem {
            block_num: bn,
            global_sequence: gs,
        });
    }
    Ok(items)
}

/// `POST /actions` — hydrate act.data for many cold-tier actions in one round-trip. The request body
/// is a JSON array of `{block_num, global_sequence}`; the response is `{"actions":[...]}` in the
/// SAME order as the request. Each distinct block is read + decoded ONCE (shared per-thread cache),
/// then every requested global_sequence within it is resolved.
fn handle_actions_batch(w: &mut Worker, info: &LogInfo, req: &mut Request) -> Reply {
    let items = match parse_batch_body(req) {
        Ok(v) => v,
        Err(reply) => return reply,
    };

    // Slot per input position; `None` until resolved (and left `None` => not-found).
    let mut resolved: Vec<Option<ResolvedAction>> = Vec::with_capacity(items.len());
    resolved.resize_with(items.len(), || None);

    // Group input positions by block so we decode each distinct block exactly once. Items whose
    // block is out of the archived range are simply never grouped -> stay not-found.
    // BTreeMap (not HashMap): iterate blocks in ascending order so that when the
    // MAX_BLOCKS_PER_BATCH / BATCH_DEADLINE cap trims a batch, the resolved subset is
    // deterministic across identical retries — and reads hit the log in ascending offset order.
    let mut by_block: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.block_num < info.first_block as u64 || it.block_num > info.last_block as u64 {
            continue; // out of range -> not found
        }
        by_block.entry(it.block_num as u32).or_default().push(i);
    }

    // Bound the per-request decode work (see MAX_BLOCKS_PER_BATCH / BATCH_DEADLINE): stop resolving
    // once either the distinct-block cap or the wall-clock deadline is hit. Positions in any block
    // we don't reach stay `None` -> emitted as `found:false` below — best-effort and contract-safe.
    let started = Instant::now();
    for (blocks_done, (&block_num, positions)) in by_block.iter().enumerate() {
        if blocks_done >= MAX_BLOCKS_PER_BATCH || started.elapsed() >= BATCH_DEADLINE {
            break;
        }
        // Read (or hit cache) the block once.
        let slot = match w.cache.get_or_read(block_num, &mut w.reader) {
            Ok(s) => s,
            // A genuine I/O/format fault on one block must not abort the whole batch; leave those
            // positions not-found and carry on.
            Err(_) => continue,
        };
        let Some(_block) = w.cache.slots[slot].1.as_ref() else {
            continue; // block had no traces -> all its positions stay not-found
        };

        // Build global_sequence -> action descriptor for THIS block, once, then satisfy every
        // requested gs from it. (Copy the act.data slice out so the cache borrow ends before the
        // decode, which needs the disjoint reg/scratch fields.)
        let mut by_gs: HashMap<u64, (u64, u64, usize, usize)> = HashMap::new();
        {
            let block = w.cache.slots[slot].1.as_ref().unwrap();
            for tx in &block.txs {
                for act in &tx.actions {
                    if act.except {
                        continue;
                    }
                    let Some(receipt) = &act.receipt else {
                        continue;
                    };
                    // First receipt wins for a given global_sequence (global_sequence is unique).
                    by_gs.entry(receipt.global_sequence).or_insert((
                        act.account,
                        act.name,
                        act.data.0,
                        act.data.1,
                    ));
                }
            }
        }

        for &i in positions {
            let gs = items[i].global_sequence;
            let Some(&(code, name, off, len)) = by_gs.get(&gs) else {
                continue; // no action with that global_sequence in this block -> not found
            };
            let data: Vec<u8> =
                w.cache.slots[slot].1.as_ref().unwrap().inflated[off..off + len].to_vec();
            resolved[i] = Some(ResolvedAction {
                block_num,
                global_sequence: gs,
                code,
                name,
                data,
            });
        }
    }

    // Emit in INPUT order. Decode each resolved action's act.data here (borrowing reg/scratch).
    let mut body = String::with_capacity(64 + items.len() * 96);
    body.push_str("{\"actions\":[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        match &resolved[i] {
            Some(r) => {
                body.push_str("{\"block_num\":");
                body.push_str(&r.block_num.to_string());
                body.push_str(",\"global_sequence\":");
                body.push_str(&r.global_sequence.to_string());
                body.push_str(",\"account\":");
                push_json_str(&mut body, &name_str(&w.names, r.code));
                body.push_str(",\"name\":");
                push_json_str(&mut body, &name_str(&w.names, r.name));
                body.push_str(",\"data\":");
                {
                    let Worker { reg, scratch, .. } = w;
                    append_data_value(
                        &mut body,
                        scratch,
                        reg,
                        r.code,
                        r.name,
                        &r.data,
                        r.block_num,
                    );
                }
                body.push_str(",\"found\":true}");
            }
            None => {
                body.push_str("{\"block_num\":");
                body.push_str(&item.block_num.to_string());
                body.push_str(",\"global_sequence\":");
                body.push_str(&item.global_sequence.to_string());
                body.push_str(",\"found\":false}");
            }
        }
    }
    body.push_str("]}");
    Reply::json(200, body)
}

// ---------------------------------------------------------------------------------------------
// POST /deltas — the delta analog of POST /actions. Hydrates contract_row values for many cold-tier
// delta docs in one round-trip. Mirrors handle_actions_batch 1:1 (parse caps, group-by-block,
// decode-each-block-once, work-bound, input-order emit, copy-out-before-decode borrow discipline).
// ---------------------------------------------------------------------------------------------

/// One parsed `/deltas` request item: the (block, code, scope, table, primary_key) of one
/// contract_row to hydrate. `code/scope/table` are kept as both the resolved u64 name (for the
/// per-block map lookup) and the original request string (echoed back verbatim).
struct DeltaItem {
    block_num: u64,
    code: u64,
    table: u64,
    primary_key: u64,
    code_str: String,
    /// The scope NAME STRING (verbatim from the request) — also the per-block map's scope key.
    scope_str: String,
    table_str: String,
}

/// A resolved `/deltas` entry, captured during the per-block pass so the cache borrow is released
/// before decoding (which borrows the registry — a disjoint Worker field). `value` is the owned
/// contract_row value bytes copied out of the inflated delta block.
struct ResolvedDelta {
    value: Vec<u8>,
}

/// Parse the JSON array request body into `DeltaItem`s. `Err(Reply)` carries the right status code:
/// 400 for malformed/over-size body or bad item shape, 413 when the array exceeds the caps. The
/// `primary_key` accepts a u64 number OR a decimal string (like /actions' global_sequence).
fn parse_deltas_body(
    req: &mut Request,
    names: &Abieos,
) -> std::result::Result<Vec<DeltaItem>, Reply> {
    let arr = read_batch_array(req)?;

    let mut items = Vec::with_capacity(arr.len());
    for el in &arr {
        let (Some(bn), Some(code_str), Some(scope_str), Some(table_str), Some(pk)) = (
            el.get("block_num").and_then(|x| x.as_u64()),
            el.get("code").and_then(|x| x.as_str()),
            el.get("scope").and_then(|x| x.as_str()),
            el.get("table").and_then(|x| x.as_str()),
            el.get("primary_key").and_then(json_u64),
        ) else {
            return Err(Reply::err(
                400,
                "each item needs numeric block_num, string code/scope/table, and number-or-string primary_key",
            ));
        };
        // `code`/`table` are always real Antelope names — resolve to u64 for the map key + decode.
        // `scope` is matched by its NAME STRING (see DeltaBlockData.rows), because arbitrary integer
        // scopes are not bijective under name encoding, so it is intentionally NOT resolved here.
        let (Ok(code), Ok(table)) = (
            names.string_to_name(code_str),
            names.string_to_name(table_str),
        ) else {
            return Err(Reply::err(400, "code/table must be valid Antelope names"));
        };
        items.push(DeltaItem {
            block_num: bn,
            code,
            table,
            primary_key: pk,
            code_str: code_str.to_string(),
            scope_str: scope_str.to_string(),
            table_str: table_str.to_string(),
        });
    }
    Ok(items)
}

/// Append the echoed request key (block_num, code, scope, table, primary_key) of a `/deltas` item.
/// `primary_key` is emitted as a string (the canonical Hyperion delta-doc form).
fn append_delta_key(
    out: &mut String,
    block_num: u64,
    code: &str,
    scope: &str,
    table: &str,
    primary_key: u64,
) {
    out.push_str("{\"block_num\":");
    out.push_str(&block_num.to_string());
    out.push_str(",\"code\":");
    push_json_str(out, code);
    out.push_str(",\"scope\":");
    push_json_str(out, scope);
    out.push_str(",\"table\":");
    push_json_str(out, table);
    out.push_str(",\"primary_key\":\"");
    out.push_str(&primary_key.to_string());
    out.push('"');
}

/// `POST /deltas` — hydrate contract_row values for many cold-tier deltas in one round-trip. The
/// request body is a JSON array of `{block_num, code, scope, table, primary_key}`; the response is
/// `{"deltas":[...]}` in the SAME order as the request. Each distinct block is read + walked ONCE
/// (shared per-thread delta cache), then every requested key within it is resolved.
fn handle_deltas_batch(w: &mut Worker, info: &LogInfo, req: &mut Request) -> Reply {
    // Defensive: the router only reaches here when a delta log was opened, so delta_reader is Some.
    if w.delta_reader.is_none() {
        return Reply::err(
            503,
            "delta archive unavailable on this node (no chain_state_history log)",
        );
    }
    let items = match parse_deltas_body(req, &w.names) {
        Ok(v) => v,
        Err(reply) => return reply,
    };

    // Slot per input position; `None` until resolved (and left `None` => not-found).
    let mut resolved: Vec<Option<ResolvedDelta>> = Vec::with_capacity(items.len());
    resolved.resize_with(items.len(), || None);

    // Group input positions by block so we walk each distinct block exactly once. Items whose block
    // is out of the archived range are never grouped -> stay not-found.
    // BTreeMap (not HashMap): iterate blocks in ascending order so that when the
    // MAX_BLOCKS_PER_BATCH / BATCH_DEADLINE cap trims a batch, the resolved subset is
    // deterministic across identical retries — and reads hit the log in ascending offset order.
    let mut by_block: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (i, it) in items.iter().enumerate() {
        if it.block_num < info.first_block as u64 || it.block_num > info.last_block as u64 {
            continue; // out of range -> not found
        }
        by_block.entry(it.block_num as u32).or_default().push(i);
    }

    // Bound the per-request decode work (see MAX_BLOCKS_PER_BATCH / BATCH_DEADLINE): stop resolving
    // once either the distinct-block cap or the wall-clock deadline is hit. Positions in any block
    // we don't reach stay `None` -> emitted as `found:false` below — best-effort and contract-safe.
    let started = Instant::now();
    for (blocks_done, (&block_num, positions)) in by_block.iter().enumerate() {
        if blocks_done >= MAX_BLOCKS_PER_BATCH || started.elapsed() >= BATCH_DEADLINE {
            break;
        }
        // Read (or hit cache) + walk the delta block once; its rows map is keyed by
        // (code, scope, table, primary_key) -> value byte-range into the inflated buffer.
        let slot = match w
            .delta_cache
            .get_or_read(block_num, w.delta_reader.as_mut().unwrap())
        {
            Ok(s) => s,
            // A genuine I/O/format fault on one block must not abort the whole batch; leave those
            // positions not-found and carry on.
            Err(_) => continue,
        };
        let Some(_block) = w.delta_cache.slots[slot].1.as_ref() else {
            continue; // block had no contract_row deltas -> all its positions stay not-found
        };

        for &i in positions {
            let it = &items[i];
            let key = (it.code, it.scope_str.clone(), it.table, it.primary_key);
            let Some(&(off, len)) = w.delta_cache.slots[slot].1.as_ref().unwrap().rows.get(&key)
            else {
                continue; // no such row in this block -> not found
            };
            // Copy the value slice OUT of the cached block (releases the `w.delta_cache` borrow) so
            // the decode below can mutably borrow the disjoint reg/scratch Worker fields.
            let value: Vec<u8> =
                w.delta_cache.slots[slot].1.as_ref().unwrap().inflated[off..off + len].to_vec();
            resolved[i] = Some(ResolvedDelta { value });
        }
    }

    // Emit in INPUT order. Decode each resolved value here (borrowing reg/scratch). On decode
    // failure emit the raw value as UPPERCASE hex under "value" instead of "data" — every found row
    // still gets an answer.
    let mut body = String::with_capacity(64 + items.len() * 96);
    body.push_str("{\"deltas\":[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            body.push(',');
        }
        append_delta_key(
            &mut body,
            item.block_num,
            &item.code_str,
            &item.scope_str,
            &item.table_str,
            item.primary_key,
        );
        match &resolved[i] {
            Some(r) => {
                w.scratch.clear();
                let blk = item.block_num as u32;
                let Worker { reg, scratch, .. } = w;
                // Decode against the row's block; on failure retry against block-1's ABI to handle a
                // setabi-in-the-same-block boundary (mirrors delta-proto's recovery) before falling
                // back to raw hex. Either way every found row still gets an answer.
                let decoded = decode_delta_row(reg, scratch, item.code, item.table, &r.value, blk)
                    .is_ok()
                    || (blk > 1 && {
                        scratch.clear();
                        decode_delta_row(reg, scratch, item.code, item.table, &r.value, blk - 1)
                            .is_ok()
                    });
                if decoded {
                    body.push_str(",\"data\":");
                    body.push_str(scratch);
                } else {
                    // LOWERCASE hex to match delta-proto (the Hyperion-validated producer of the cold
                    // doc): a hydrated undecodable `value` is then byte-identical to the hot doc.
                    body.push_str(",\"value\":");
                    push_json_str(&mut body, &hex::encode(&r.value));
                }
                body.push_str(",\"found\":true}");
            }
            None => {
                body.push_str(",\"found\":false}");
            }
        }
    }
    body.push_str("]}");
    Reply::json(200, body)
}

/// Route + handle one request, never panicking the worker thread.
fn handle(
    w: &mut Worker,
    info: &LogInfo,
    delta_info: Option<&LogInfo>,
    req: &mut Request,
) -> Reply {
    let raw = req.url().to_string();
    let (path, query) = match raw.split_once('?') {
        Some((p, q)) => (p, q),
        None => (raw.as_str(), ""),
    };
    let method = req.method().clone();

    // POST /actions or /deltas — batch hydration (reads the request body).
    if method == tiny_http::Method::Post {
        return match path {
            "/actions" => handle_actions_batch(w, info, req),
            "/deltas" => match delta_info {
                Some(di) => handle_deltas_batch(w, di, req),
                None => Reply::err(
                    503,
                    "delta archive unavailable on this node (no chain_state_history log)",
                ),
            },
            _ => Reply::err(404, "unknown endpoint"),
        };
    }

    // Everything else is GET-only.
    if method != tiny_http::Method::Get {
        return Reply::err(405, "method not allowed");
    }
    match path {
        "/health" => handle_health(info, delta_info),
        "/action" => handle_action(w, info, query),
        p if p.starts_with("/block/") => {
            let tail = &p["/block/".len()..];
            match tail.parse::<u32>() {
                Ok(n) => handle_block(w, info, n),
                Err(_) => Reply::err(400, "block number must be a u32"),
            }
        }
        _ => Reply::err(404, "unknown endpoint"),
    }
}

fn respond(req: Request, reply: Reply) {
    let ctype: &[u8] = if reply.json {
        b"application/json"
    } else {
        b"text/plain; charset=utf-8"
    };
    let header = Header::from_bytes(&b"Content-Type"[..], ctype)
        .expect("static content-type header is valid");
    let response = Response::from_string(reply.body)
        .with_status_code(reply.code)
        .with_header(header);
    let _ = req.respond(response);
}

fn worker_loop(
    server: Arc<Server>,
    info: Arc<LogInfo>,
    delta_info: Option<Arc<LogInfo>>,
    abi_index: Arc<AbiIndex>,
) -> Result<()> {
    let mut w = Worker {
        reader: LogReader::open(&info)?,
        cache: BlockCache::new(64),
        delta_reader: delta_info.as_deref().map(DeltaReader::open).transpose()?,
        delta_cache: DeltaCache::new(64),
        reg: Registry::new(&abi_index),
        names: Abieos::new(),
        scratch: String::new(),
    };
    loop {
        let mut req = match server.recv() {
            Ok(r) => r,
            Err(_) => break, // server shutting down
        };
        // Defensive: a handler should never panic, but if one ever does, catch it so the worker
        // thread survives and keeps serving.
        let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle(&mut w, &info, delta_info.as_deref(), &mut req)
        })) {
            Ok(r) => r,
            Err(_) => Reply::err(500, "internal error"),
        };
        respond(req, reply);
    }
    Ok(())
}

/// Open one frozen state-history log pair (`<dir>/<stem>.{log,index}`), validate its SHiP magic, and
/// compute the served block range from the first entry header + the index length. Shared by both the
/// trace (actions) and chain_state_history (deltas) logs.
fn open_log_info(dir: &str, stem: &str) -> Result<LogInfo> {
    let log_path = format!("{dir}/{stem}.log");
    let idx_path = format!("{dir}/{stem}.index");

    // first_block from the first entry header (and validate the ship magic).
    let mut f = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
    let mut hdr = [0u8; 48];
    f.read_exact(&mut hdr)
        .with_context(|| format!("read first header of {log_path}"))?;
    if !is_ship_magic(u64::from_le_bytes(hdr[0..8].try_into().unwrap())) {
        bail!("{log_path} is not a state-history log (bad ship magic)");
    }
    let first_block = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
    // last_block from the index length (one 8-byte entry per committed block).
    let n_idx = (std::fs::metadata(&idx_path)
        .with_context(|| format!("stat {idx_path}"))?
        .len()
        / 8) as u32;
    if n_idx == 0 {
        bail!("{idx_path} is empty — no blocks to serve");
    }
    let last_block = first_block + n_idx - 1;

    Ok(LogInfo {
        log_path,
        idx_path,
        first_block,
        last_block,
    })
}

fn main() -> Result<()> {
    let args = Args::parse();

    eprintln!("[archive-server] loading ABI index {} ...", args.abi_index);
    let abi_index = Arc::new(load_abi_index(&args.abi_index)?);
    eprintln!(
        "[archive-server] {} contracts in ABI index",
        abi_index.len()
    );

    // Both the trace (actions) and the chain_state_history (deltas) logs live in the same
    // --from-disk dir, with identical SHiP framing; open each the same way.
    let info = Arc::new(open_log_info(&args.from_disk, "trace_history")?);
    // The delta (chain_state_history) log is OPTIONAL: an actions-only archive (or one whose delta
    // range isn't frozen in this dir) must still boot and serve /action, /block, /health and POST
    // /actions. When it's absent, POST /deltas answers 503 and everything else is unaffected.
    let delta_info = match open_log_info(&args.from_disk, "chain_state_history") {
        Ok(li) => Some(Arc::new(li)),
        Err(e) => {
            eprintln!("[archive-server] chain_state_history not available ({e:#}) — POST /deltas disabled; actions + GET endpoints unaffected");
            None
        }
    };
    let (first_block, last_block) = (info.first_block, info.last_block);

    let addr = format!("0.0.0.0:{}", args.port);
    let server = Arc::new(Server::http(&addr).map_err(|e| anyhow::anyhow!("bind {addr}: {e}"))?);
    let threads = args.threads.max(1);
    eprintln!(
        "[archive-server] serving blocks [{first_block}..{last_block}] on http://{addr} with {threads} worker(s)"
    );
    match &delta_info {
        Some(di) => eprintln!(
            "[archive-server]   delta blocks [{}..{}] (chain_state_history)",
            di.first_block, di.last_block
        ),
        None => eprintln!("[archive-server]   delta blocks: none — POST /deltas disabled"),
    }
    eprintln!("[archive-server]   GET /action?block_num=<N>&global_sequence=<G>");
    eprintln!("[archive-server]   GET /block/<N>");
    eprintln!("[archive-server]   GET /health   (status + archived block ranges)");
    eprintln!("[archive-server]   POST /actions  [{{block_num, global_sequence}}, ...]");
    eprintln!(
        "[archive-server]   POST /deltas   [{{block_num, code, scope, table, primary_key}}, ...]"
    );

    let mut handles = Vec::new();
    for i in 0..threads {
        let (srv, inf, dinf, ai) = (
            server.clone(),
            info.clone(),
            delta_info.clone(),
            abi_index.clone(),
        );
        handles.push(std::thread::spawn(move || {
            if let Err(e) = worker_loop(srv, inf, dinf, ai) {
                eprintln!("[archive-server] worker {i} FAILED: {e:#}");
            }
        }));
    }
    for h in handles {
        let _ = h.join();
    }
    Ok(())
}
