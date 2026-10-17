//! Real data through the whole server, compared byte for byte with what the strict readers give
//! when they read the same files directly, outside the catalog, the store, the prefetcher and
//! the session.
//!
//! The committed tests serve only real data the repository already holds (see
//! `testdata/README.md`): the state-history entries of `crates/core/testdata/ship`, each the
//! only entry of a log, inline and streamed; and the EOS mainnet block of
//! `crates/core/testdata/blocks`, in a block log beside an entry that names its id.
//!
//! The ignored tests read real logs from directories the environment names, so that none of
//! that data is committed. Run them in release mode, one at a time for clean timings:
//! `cargo test --release -p antelope-ship-archive --test real_slices -- --ignored
//! --nocapture --test-threads 1`. Each one prints what it measured and skips (passing, with a
//! line saying so) when its variables are not set.
//!   * `SHIP_REAL_DIR`: state-history logs, with block logs or without, flat or retained
//!     (sub-directories are scanned). Without block logs the server runs with `--blocks none`
//!     and needs the chain id: `SHIP_REAL_CHAIN_ID`, or the `global_property` row of a
//!     chain_state log that starts with the full state (at genesis or from a snapshot). Every
//!     source of the chain id that is there must agree. `SHIP_REAL_BLOCKS` sets how many blocks
//!     are compared byte for byte (default 5000), and `SHIP_REAL_URL` points the full-speed run
//!     at a server already serving the same logs (to measure it as its own process).
//!   * `SHIP_DIFF_NODE` (`ws://host:port`) and `SHIP_DIFF_DIR`: the differential test asks a
//!     real node and this server, over a copy of that node's logs, for the same blocks, and
//!     every result must be byte-equal but for head and LIB (bytes 1..73). `SHIP_DIFF_CHAIN_ID`
//!     (as above), `SHIP_DIFF_RANGE` (`<begin>-<end>`, also served as `--range`),
//!     `SHIP_DIFF_BLOCKS` (the most blocks compared, default 10000) and `SHIP_DIFF_V1` (also
//!     compare v1 requests, which only Spring nodes answer) tune it.
//!
//! The test binary counts every allocation, so the ignored runs also report the peak heap of
//! the process (server and client together).

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use common::*;
use hyperion_ship::binio::{FcReader, FileCache, num_from_id};
use hyperion_ship::blocklog::BlockLog;
use hyperion_ship::catalog::Finding;
use hyperion_ship::protocol::{self, BlockPos, BlocksReq, Request, Status};
use hyperion_ship::shiplog::{LogKind, ShipLog};
use hyperion_ship::testkit::blocks::{self as blocklogs, Block, Preamble};
use hyperion_ship::testkit::ship::{Encoding, LogBuilder, synthetic_payload, zlib_stored};
use ship_archive::check::{self, CheckConfig, WARN_ENTRY_BYTES};
use ship_archive::frames::CHUNK;
use ship_archive::store::{BlocksMode, Store, StoreConfig};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

// --- the allocation counter ------------------------------------------------------------------

/// The system allocator, counting the bytes allocated now and at most.
struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = NOW.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

fn shrank(by: usize) {
    NOW.fetch_sub(by, Relaxed);
}

// SAFETY: every call goes to the system allocator unchanged; only the sizes are counted.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, forwarded.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, forwarded.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, forwarded.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                shrank(layout.size() - new_size);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Start counting the peak heap from what is allocated now.
fn reset_peak() {
    PEAK.store(NOW.load(Relaxed), Relaxed);
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / f64::from(1 << 20)
}

/// The peak heap since [`reset_peak`], in MiB.
fn peak_mib() -> f64 {
    mib(PEAK.load(Relaxed) as u64)
}

// --- the logs, read directly -------------------------------------------------------------------

/// The logs of some directory trees, each file opened on its own with the strict readers (no
/// discovery, catalog or store), sorted by first block. Each kind must be contiguous.
struct Logs {
    ship: [Vec<ShipLog>; 3],
    blocks: Vec<BlockLog>,
}

fn kind_at(kind: LogKind) -> usize {
    LogKind::ALL.iter().position(|&k| k == kind).unwrap()
}

impl Logs {
    fn open(dirs: &[PathBuf]) -> Self {
        let cache = FileCache::new(64);
        let mut logs = Self {
            ship: Default::default(),
            blocks: Vec::new(),
        };
        let mut todo = dirs.to_vec();
        while let Some(dir) = todo.pop() {
            for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    todo.push(path);
                    continue;
                }
                let Some(stem) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".log"))
                else {
                    continue;
                };
                // `<stem>.log` or a retained `<stem>-<first>-<last>.log`; the index beside it.
                let base = stem.split('-').next().unwrap();
                let index = path.with_extension("index");
                let failed = |e: anyhow::Error| format!("{}: {e:#}", path.display());
                if base == "blocks" {
                    let log = BlockLog::open(&path, &index, &cache);
                    if let Some(log) = log.unwrap_or_else(|e| panic!("{}", failed(e))) {
                        logs.blocks.push(log);
                    }
                } else if let Some(&kind) = LogKind::ALL.iter().find(|k| k.stem() == base) {
                    let log = ShipLog::open(&path, &index, kind, &cache);
                    if let Some(log) = log.unwrap_or_else(|e| panic!("{}", failed(e))) {
                        logs.ship[kind_at(kind)].push(log);
                    }
                }
            }
        }
        for files in &mut logs.ship {
            files.sort_by_key(|f| f.range());
            contiguous(files.iter().map(ShipLog::range));
        }
        logs.blocks.sort_by_key(|f| f.range());
        contiguous(logs.blocks.iter().map(BlockLog::range));
        logs
    }

    fn held(&self, kind: LogKind) -> Option<(u32, u32)> {
        let files = &self.ship[kind_at(kind)];
        Some((files.first()?.range().0, files.last()?.range().1))
    }

    fn blocks_held(&self) -> Option<(u32, u32)> {
        Some((
            self.blocks.first()?.range().0,
            self.blocks.last()?.range().1,
        ))
    }

    fn ship_log(&self, kind: LogKind, n: u32) -> Option<&ShipLog> {
        self.ship[kind_at(kind)]
            .iter()
            .find(|f| contains(f.range(), n))
    }

    fn block_log(&self, n: u32) -> Option<&BlockLog> {
        self.blocks.iter().find(|f| contains(f.range(), n))
    }

    /// The first block of the chain_state log and its payload: the full state, when the log
    /// starts at genesis or from a snapshot.
    fn first_state(&self) -> Option<(u32, Vec<u8>)> {
        let log = self.ship[kind_at(LogKind::ChainState)].first()?;
        let n = log.range().0;
        Some((n, read(log, n)))
    }

    /// The chain id of the `global_property` row in the first chain_state entry, if it holds
    /// one.
    fn genesis_chain_id(&self) -> Option<[u8; 32]> {
        let (n, deltas) = self.first_state()?;
        let tables = tables(&deltas).unwrap_or_else(|e| panic!("block {n}: {e:#}"));
        let (_, rows) = tables.iter().find(|(name, _)| name == "global_property")?;
        let (_, row) = rows.first()?;
        Some(global_property_chain_id(row).unwrap_or_else(|e| panic!("block {n}: {e:#}")))
    }
}

/// Every range starts where the one before it ends.
fn contiguous(ranges: impl Iterator<Item = (u32, u32)>) {
    let ranges: Vec<_> = ranges.collect();
    for w in ranges.windows(2) {
        assert_eq!(w[0].1, w[1].0, "the files are not contiguous: {ranges:?}");
    }
}

fn contains((begin, end): (u32, u32), n: u32) -> bool {
    begin <= n && n < end
}

/// The whole inflated payload of block `n`'s entry in `log`.
fn read(log: &ShipLog, n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    log.header(n)
        .and_then(|h| log.read_inline(&h, u64::MAX, &mut out))
        .unwrap_or_else(|e| panic!("{e:#}"));
    out
}

/// What a Spring node holding `logs` serves, given `--range` and whether block logs give
/// results their `block`: the served ranges, `L*`, and every message, from the logs alone.
struct View<'a> {
    logs: &'a Logs,
    served: [(u32, u32); 3],
    blocks: (u32, u32),
    last: BlockPos,
    chain_id: [u8; 32],
}

/// `r` within `window`, or `(0, 0)`.
fn clip(r: (u32, u32), window: Option<(u32, u32)>) -> (u32, u32) {
    let Some((begin, end)) = window else {
        return r;
    };
    let (b, e) = (r.0.max(begin), r.1.min(end));
    if b < e { (b, e) } else { (0, 0) }
}

impl<'a> View<'a> {
    fn new(
        logs: &'a Logs,
        range: Option<(u32, u32)>,
        with_blocks: bool,
        chain_id: [u8; 32],
    ) -> Self {
        let blocks = match (with_blocks, logs.blocks_held()) {
            (true, Some(held)) => clip(held, range),
            _ => (0, 0),
        };
        let served = LogKind::ALL.map(|k| {
            logs.held(k).map_or((0, 0), |held| {
                let r = clip(held, range);
                if with_blocks {
                    clip(r, Some(blocks))
                } else {
                    r
                }
            })
        });
        let mut view = Self {
            logs,
            served,
            blocks,
            last: BlockPos::default(),
            chain_id,
        };
        let last = served.iter().map(|r| r.1).max().unwrap() - 1;
        view.last = BlockPos {
            num: last,
            id: view.id(last).expect("the last block has an id"),
        };
        view
    }

    fn served(&self, kind: LogKind) -> (u32, u32) {
        self.served[kind_at(kind)]
    }

    /// The first block any log serves.
    fn begin(&self) -> u32 {
        self.served
            .iter()
            .chain([&self.blocks])
            .filter(|r| r.0 < r.1)
            .map(|r| r.0)
            .min()
            .unwrap()
    }

    /// One past `L*`.
    fn end(&self) -> u32 {
        self.last.num + 1
    }

    /// Spring's order: the trace, chain_state and finality entries, then the block log's
    /// `previous` of the next block. There is no block 0, whatever block 1's `previous` says.
    fn id(&self, n: u32) -> Option<[u8; 32]> {
        if n == 0 {
            return None;
        }
        for kind in LogKind::ALL {
            if contains(self.served(kind), n) {
                let log = self.logs.ship_log(kind, n).unwrap();
                return Some(log.id(n).unwrap_or_else(|e| panic!("{e:#}")));
            }
        }
        let next = n.checked_add(1)?;
        contains(self.blocks, next).then(|| {
            let log = self.logs.block_log(next).unwrap();
            log.previous(next).unwrap_or_else(|e| panic!("{e:#}"))
        })
    }

    fn payload(&self, kind: LogKind, n: u32) -> Option<Vec<u8>> {
        contains(self.served(kind), n).then(|| read(self.logs.ship_log(kind, n).unwrap(), n))
    }

    fn block(&self, n: u32) -> Option<Vec<u8>> {
        contains(self.blocks, n).then(|| {
            let mut body = Vec::new();
            let log = self.logs.block_log(n).unwrap();
            log.body(n, &mut body).unwrap_or_else(|e| panic!("{e:#}"));
            body
        })
    }

    fn status(&self, v1: bool) -> Vec<u8> {
        protocol::encode_status(
            v1,
            &Status {
                head: self.last,
                lib: self.last,
                trace: self.served(LogKind::Trace),
                chain_state: self.served(LogKind::ChainState),
                finality: self.served(LogKind::Finality),
                chain_id: self.chain_id,
            },
        )
    }

    /// The whole result message for block `n` answering `req`.
    fn result(&self, n: u32, req: &BlocksReq) -> Vec<u8> {
        let head = self.last;
        let mut out = vec![protocol::result_variant(req.v1)];
        let fields = if req.v1 { 3 } else { 2 };
        let Some(id) = self.id(n) else {
            out.extend(protocol::encode_result_base(head, head, None, None, None));
            for _ in 0..fields {
                out.extend(protocol::encode_opt_len(None));
            }
            return out;
        };
        let this = BlockPos { num: n, id };
        let prev = n
            .checked_sub(1)
            .and_then(|p| self.id(p).map(|id| BlockPos { num: p, id }));
        let block = req.fetch_block.then(|| self.block(n)).flatten();
        out.extend(protocol::encode_result_base(
            head,
            head,
            Some(this),
            prev,
            block.as_deref(),
        ));
        let wanted = [
            (req.fetch_traces, LogKind::Trace),
            (req.fetch_deltas, LogKind::ChainState),
            (req.fetch_finality_data, LogKind::Finality),
        ];
        for (want, kind) in wanted.into_iter().take(fields) {
            match want.then(|| self.payload(kind, n)).flatten() {
                Some(p) => {
                    out.extend(protocol::encode_opt_len(Some(p.len() as u64)));
                    out.extend(p);
                }
                None => out.extend(protocol::encode_opt_len(None)),
            }
        }
        out
    }
}

// --- SHiP decoding helpers ---------------------------------------------------------------------

/// One `table_delta_v0`: the table's name and its `(present, data)` rows.
type TableDelta<'a> = (String, Vec<(bool, &'a [u8])>);

/// The table deltas of a deltas payload, which must decode whole.
fn tables(deltas: &[u8]) -> anyhow::Result<Vec<TableDelta<'_>>> {
    let mut r = FcReader::new(deltas);
    let count = r.varuint32()?;
    let mut out = Vec::new();
    for _ in 0..count {
        anyhow::ensure!(r.varuint32()? == 0, "a table delta is not table_delta_v0");
        let len = r.varuint32()? as usize;
        let name = String::from_utf8(r.bytes(len)?.to_vec())?;
        let rows = r.varuint32()?;
        let mut data = Vec::new();
        for _ in 0..rows {
            let present = r.bool()?;
            let len = r.varuint32()? as usize;
            data.push((present, r.bytes(len)?));
        }
        out.push((name, data));
    }
    anyhow::ensure!(
        r.remaining() == 0,
        "{} bytes follow the table deltas",
        r.remaining()
    );
    Ok(out)
}

/// The `chain_id` of a `global_property_v1` row.
fn global_property_chain_id(row: &[u8]) -> anyhow::Result<[u8; 32]> {
    let mut r = FcReader::new(row);
    let variant = r.varuint32()?;
    anyhow::ensure!(variant == 1, "global_property_v{variant} holds no chain id");
    if r.bool()? {
        r.u32()?; // proposed_schedule_block_num
    }
    r.u32()?; // proposed_schedule.version
    for _ in 0..r.varuint32()? {
        r.u64()?; // producer_name
        anyhow::ensure!(r.varuint32()? == 0, "block_signing_authority_v0");
        r.u32()?; // threshold
        for _ in 0..r.varuint32()? {
            match r.varuint32()? {
                0 | 1 => {
                    r.bytes(33)?;
                }
                2 => {
                    r.bytes(33)?;
                    r.u8()?;
                    let len = r.varuint32()? as usize;
                    r.bytes(len)?;
                }
                k => anyhow::bail!("public key type {k}"),
            }
            r.bytes(2)?; // weight
        }
    }
    match r.varuint32()? {
        0 => r.bytes(68)?, // chain_config_v0
        1 => r.bytes(72)?, // chain_config_v1
        v => anyhow::bail!("chain_config_v{v}"),
    };
    r.id()
}

/// `this_block` and `prev_block` of a result message.
fn positions(msg: &[u8]) -> (Option<BlockPos>, Option<BlockPos>) {
    let mut at = 73;
    let mut next = || {
        let present = msg[at] == 1;
        at += 1;
        present.then(|| {
            let p = BlockPos {
                num: u32::from_le_bytes(msg[at..at + 4].try_into().unwrap()),
                id: msg[at + 4..at + 36].try_into().unwrap(),
            };
            at += 36;
            p
        })
    };
    let this = next();
    (this, next())
}

/// A status message's fields; the v0 `chain_id` only when the server sends it.
struct ParsedStatus {
    head: BlockPos,
    lib: BlockPos,
    trace: (u32, u32),
    chain_state: (u32, u32),
    chain_id: Option<[u8; 32]>,
}

fn parse_status(msg: &[u8]) -> ParsedStatus {
    assert!(
        matches!(msg[0], 0 | 3) && msg.len() >= 89,
        "not a status: {msg:02x?}"
    );
    let u32_at = |at: usize| u32::from_le_bytes(msg[at..at + 4].try_into().unwrap());
    let pos = |at: usize| BlockPos {
        num: u32_at(at),
        id: msg[at + 4..at + 36].try_into().unwrap(),
    };
    ParsedStatus {
        head: pos(1),
        lib: pos(37),
        trace: (u32_at(73), u32_at(77)),
        chain_state: (u32_at(81), u32_at(85)),
        chain_id: (msg.len() >= 121).then(|| msg[89..121].try_into().unwrap()),
    }
}

/// `got` must be `want`; a mismatch shows where, not the megabytes around it.
fn assert_same(what: &str, got: &[u8], want: &[u8]) {
    if got != want {
        let at = got
            .iter()
            .zip(want)
            .position(|(a, b)| a != b)
            .unwrap_or(got.len().min(want.len()));
        panic!(
            "{what}: {} bytes received, {} expected; they differ from byte {at}: {:02x?} vs \
             {:02x?}",
            got.len(),
            want.len(),
            &got[at..(at + 16).min(got.len())],
            &want[at..(at + 16).min(want.len())]
        );
    }
}

/// `got` must be `want` but for head and LIB, which a node reports as of now.
fn assert_same_but_head(what: &str, got: &[u8], want: &[u8]) {
    assert!(got.len() >= 73 && want.len() >= 73, "{what}: not a result");
    assert_eq!(got[0], want[0], "{what}: the result variant");
    assert_same(what, &got[73..], &want[73..]);
}

/// Ask for `req` and pass each of the `count` results to `each` with its block number, acking
/// half the credit window each time half of it has arrived, as a client acking as it goes.
async fn stream(ws: &mut Ws, req: &BlocksReq, count: u32, mut each: impl FnMut(u32, &[u8])) {
    let half = (req.max_in_flight / 2).max(1);
    send(ws, &Request::Blocks(req.clone())).await;
    let mut unacked = 0;
    for n in req.start..req.start + count {
        let msg = recv_binary(ws).await;
        each(n, &msg);
        unacked += 1;
        if unacked == half {
            send(ws, &Request::Ack(half)).await;
            unacked = 0;
        }
    }
}

/// A request for `[start, end)` with every field a server of `view` can fill, `credit` at a
/// time.
fn fetch(view: &View, v1: bool, start: u32, end: u32, credit: u32) -> BlocksReq {
    let mut req = fetch_all(blocks(start, end, credit));
    req.v1 = v1;
    req.fetch_finality_data = v1;
    req.fetch_block = view.blocks.0 < view.blocks.1;
    req
}

fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ship-archive-real-")
        .tempdir()
        .expect("temp dir")
}

fn hex32(s: &str) -> [u8; 32] {
    hex::decode(s.trim())
        .ok()
        .and_then(|v| v.try_into().ok())
        .unwrap_or_else(|| panic!("{s:?} is not 32 bytes of hex"))
}

// --- committed: the real data in the repository ------------------------------------------------

/// The real state-history entries of `crates/core/testdata/ship`: one entry's 48-byte header
/// and its payload each, named after the log kind, the encoding and the block.
const REAL_ENTRIES: [(&str, &[u8]); 7] = [
    (
        "eos-trace-deflate-50000085",
        include_bytes!("../../core/testdata/ship/eos-trace-deflate-50000085.bin"),
    ),
    (
        "eos-trace-deflate-large-100000002",
        include_bytes!("../../core/testdata/ship/eos-trace-deflate-large-100000002.bin"),
    ),
    (
        "eos-state-deflate-300000054",
        include_bytes!("../../core/testdata/ship/eos-state-deflate-300000054.bin"),
    ),
    (
        "eos-trace-stored-480000070",
        include_bytes!("../../core/testdata/ship/eos-trace-stored-480000070.bin"),
    ),
    (
        "eos-state-stored-525000000",
        include_bytes!("../../core/testdata/ship/eos-state-stored-525000000.bin"),
    ),
    (
        "wax-trace-deflate-432000128",
        include_bytes!("../../core/testdata/ship/wax-trace-deflate-432000128.bin"),
    ),
    (
        "wax-state-deflate-432001551",
        include_bytes!("../../core/testdata/ship/wax-state-deflate-432001551.bin"),
    ),
];

/// EOS mainnet block 480385480, packed, and its id as nodeos reports it.
const EOS_BLOCK_HEX: &str = include_str!("../../core/testdata/blocks/eos-mainnet-480385480.hex");
const EOS_BLOCK_ID: &str = "1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4";
const EOS_CHAIN_ID: &str = "aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906";

/// The chain id served with `--blocks none` when nothing in the logs names it: any will do.
const ANY_CHAIN_ID: [u8; 32] = [0x5a; 32];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_entries_are_served_byte_for_byte_inline_and_streamed() {
    for (name, bytes) in REAL_ENTRIES {
        let kind = if name.contains("-state-") {
            LogKind::ChainState
        } else {
            LogKind::Trace
        };
        let id: [u8; 32] = bytes[8..40].try_into().unwrap();
        let n = num_from_id(&id);
        assert_eq!(name.rsplit('-').next(), Some(n.to_string().as_str()));
        let tmp = tempdir();
        let mut log = LogBuilder::new(Encoding::SizedStored);
        log.push_encoded(id, &bytes[48..]);
        log.write(tmp.path(), kind.stem()).unwrap();

        // The payload, read directly, is what the entry declares; a Spring stored stream is
        // what the fixture encoder writes for it.
        let logs = Logs::open(&[tmp.path().to_path_buf()]);
        let view = View::new(&logs, None, false, ANY_CHAIN_ID);
        let payload = view.payload(kind, n).expect("the entry is served");
        let declared = u64::from_le_bytes(bytes[52..60].try_into().unwrap());
        assert_eq!(payload.len() as u64, declared, "{name}");
        if name.contains("-stored-") {
            assert_eq!(zlib_stored(&payload), bytes[60..], "{name}");
        }

        // Inline under the default threshold, streamed under a 1 KiB one.
        for threshold in [4 << 20, 1 << 10] {
            let mut cfg = serve_config(StoreConfig {
                dirs: vec![tmp.path().to_path_buf()],
                blocks: BlocksMode::None,
                chain_id: Some(ANY_CHAIN_ID),
                ..StoreConfig::default()
            });
            cfg.stream_threshold = threshold;
            let server = start(cfg).await;
            let mut raw = Raw::connect(server.addr).await;
            for v1 in [false, true] {
                raw.send_request(&Request::Status { v1 }).await;
                let status = joined(&raw.message().await.expect("a status"));
                assert_same(&format!("{name}: status"), &status, &view.status(v1));
            }
            for v1 in [false, true] {
                let mut req = fetch_all(blocks(n, n + 1, 1));
                req.v1 = v1;
                req.fetch_finality_data = v1;
                raw.send_request(&Request::Blocks(req.clone())).await;
                let frames = raw.message().await.expect("a result");
                let what = format!("{name}: v1 {v1}, threshold {threshold}");
                assert_same(&what, &joined(&frames), &view.result(n, &req));
                let (_, _, fields) = pieces(&frames);
                let (head, data) = &fields[kind_at(kind)];
                assert_eq!(*head, protocol::encode_opt_len(Some(declared)), "{what}");
                assert!(data.iter().all(|c| c.len() <= CHUNK), "{what}");
                assert_eq!(data.concat(), payload, "{what}");
                let other = &fields[1 - kind_at(kind)];
                assert_eq!(
                    other,
                    &(vec![0], vec![]),
                    "{what}: the other log has nothing"
                );
            }
            // Drop each client before stopping its server, which then need not wait for its Close.
            drop(raw);
            server.stop().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_block_is_served_under_the_id_its_header_hashes_to() {
    let body = hex::decode(EOS_BLOCK_HEX.trim()).unwrap();
    let id = hex32(EOS_BLOCK_ID);
    let n = num_from_id(&id);
    let previous: [u8; 32] = body[14..46].try_into().unwrap();
    assert_eq!(num_from_id(&previous), n - 1);
    let chain_id = hex32(EOS_CHAIN_ID);

    // The block alone in a v3 block log that starts at it, as a split leaves one, beside a
    // trace entry naming the block's id (its payload is made up: no real one is public).
    let write = |entry_id: [u8; 32]| {
        let tmp = tempdir();
        let block = Block {
            num: n,
            id,
            previous,
            body: body.clone(),
        };
        let preamble = Preamble::V3ChainId { first: n, chain_id };
        blocklogs::write(tmp.path(), "blocks", &preamble, &[block]).unwrap();
        let mut log = LogBuilder::new(Encoding::SizedStored);
        log.push(entry_id, &synthetic_payload(n, 300));
        log.write(tmp.path(), LogKind::Trace.stem()).unwrap();
        tmp
    };
    let tmp = write(id);
    let store = StoreConfig {
        dirs: vec![tmp.path().to_path_buf()],
        ..StoreConfig::default()
    };
    let logs = Logs::open(&[tmp.path().to_path_buf()]);
    let view = View::new(&logs, None, true, chain_id);
    assert_eq!(view.block(n).as_deref(), Some(body.as_slice()));

    let server = start(serve_config(store.clone())).await;
    let mut ws = connect(&server).await;
    for v1 in [false, true] {
        let status = status(&mut ws, v1).await;
        assert_same("status", &status, &view.status(v1));
        assert_eq!(status[89..121], chain_id, "the chain id of the block log");
    }
    let req = fetch_all(blocks(n, n + 1, 1));
    send(&mut ws, &Request::Blocks(req.clone())).await;
    let msg = recv_binary(&mut ws).await;
    assert_same("block 480385480", &msg, &view.result(n, &req));
    let (this, prev) = positions(&msg);
    assert_eq!(this, Some(BlockPos { num: n, id }));
    assert_eq!(
        prev,
        Some(BlockPos {
            num: n - 1,
            id: previous
        }),
        "the block before comes from the block's `previous`"
    );
    // The base: head, LIB, this, prev, then `block` present with a varuint32 length.
    let block_at = 1 + 72 + 2 * 37;
    assert_eq!(msg[block_at], 1);
    let len_bytes = protocol::encode_opt_len(Some(body.len() as u64));
    assert_eq!(msg[block_at..block_at + len_bytes.len()], len_bytes);
    let start = block_at + len_bytes.len();
    assert_eq!(msg[start..start + body.len()], body);
    drop(ws);
    server.stop().await.unwrap();

    // `check --hash` recomputes the id from the header.
    let report = check::check(&CheckConfig {
        store: store.clone(),
        deep: true,
        hash: true,
        warn_entry_bytes: WARN_ENTRY_BYTES,
    });
    assert_eq!(report.error_count(), 0, "{}", report.text());
    let deep = report.deep.expect("a deep check");
    assert_eq!((deep.blocks, deep.entries, deep.hashed), (1, 1, 1));

    // An entry naming another id for the block is refused at startup: the block's header
    // hashes to the real id.
    let mut other = id;
    other[31] ^= 1;
    let tmp = write(other);
    let err = Store::open(&StoreConfig {
        dirs: vec![tmp.path().to_path_buf()],
        ..StoreConfig::default()
    })
    .err()
    .expect("ids that disagree refuse startup");
    let err = format!("{err:#}");
    assert!(
        err.contains(&format!("block ids disagree at block {n}")),
        "{err}"
    );
    assert!(err.contains(EOS_BLOCK_ID), "{err}");
}

// --- ignored: real logs from SHIP_REAL_DIR -------------------------------------------------------

/// A directory of real logs, opened directly, and its chain id.
struct Real {
    dir: PathBuf,
    logs: Logs,
    chain_id: [u8; 32],
}

impl Real {
    /// The logs in `$dir_var`, with the chain id of `$chain_var`, the first chain_state entry
    /// or the block logs; `None` (the test is skipped) when `$dir_var` is not set.
    fn from_env(test: &str, dir_var: &str, chain_var: &str) -> Option<Self> {
        let Some(dir) = var(dir_var) else {
            eprintln!("SKIPPED {test}: set {dir_var} to a directory of real logs");
            return None;
        };
        let dir = PathBuf::from(dir);
        let logs = Logs::open(std::slice::from_ref(&dir));
        let sources = [
            ("the environment", var(chain_var).map(|h| hex32(&h))),
            ("the first chain_state entry", logs.genesis_chain_id()),
            (
                "the block logs",
                logs.blocks.first().map(BlockLog::chain_id),
            ),
        ];
        let known: Vec<_> = sources
            .iter()
            .filter_map(|(what, id)| id.map(|id| (*what, id)))
            .collect();
        let Some(&(_, chain_id)) = known.first() else {
            panic!("no block log and no full-state first entry name the chain id: set {chain_var}");
        };
        for (what, id) in &known {
            assert_eq!(
                hex::encode(id),
                hex::encode(chain_id),
                "the chain id from {what} differs from the one from {}",
                known[0].0
            );
        }
        let real = Self {
            dir,
            logs,
            chain_id,
        };
        eprintln!(
            "real: chain {} from {}; {}",
            hex::encode(chain_id),
            known.iter().map(|(w, _)| *w).collect::<Vec<_>>().join(", "),
            real.describe()
        );
        Some(real)
    }

    fn with_blocks(&self) -> bool {
        !self.logs.blocks.is_empty()
    }

    /// The store a server of these logs opens: `--blocks none` without block logs.
    fn store(&self, range: Option<(u32, u32)>) -> StoreConfig {
        StoreConfig {
            dirs: vec![self.dir.clone()],
            recursive: true,
            range,
            blocks: if self.with_blocks() {
                BlocksMode::Required
            } else {
                BlocksMode::None
            },
            chain_id: Some(self.chain_id),
            ..StoreConfig::default()
        }
    }

    fn view(&self, range: Option<(u32, u32)>) -> View<'_> {
        View::new(&self.logs, range, self.with_blocks(), self.chain_id)
    }

    fn describe(&self) -> String {
        let mut parts: Vec<String> = LogKind::ALL
            .iter()
            .map(|&k| {
                let files = &self.logs.ship[kind_at(k)];
                match self.logs.held(k) {
                    Some((b, e)) => format!("{} {b}..{e} in {} files", k.label(), files.len()),
                    None => format!("{} none", k.label()),
                }
            })
            .collect();
        parts.push(match self.logs.blocks_held() {
            Some((b, e)) => format!("blocks {b}..{e} in {} files", self.logs.blocks.len()),
            None => "no block log".into(),
        });
        parts.join(", ")
    }
}

/// The environment variable `name`; an empty one counts as not set.
fn var(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}

fn env_u32(name: &str, default: u32) -> u32 {
    var(name).map_or(default, |v| {
        v.parse()
            .unwrap_or_else(|_| panic!("{name}={v:?} is not a number"))
    })
}

fn env_range(name: &str) -> Option<(u32, u32)> {
    let v = var(name)?;
    let (b, e) = v
        .split_once('-')
        .unwrap_or_else(|| panic!("{name}={v:?} is not <begin>-<end>"));
    Some((b.parse().unwrap(), e.parse().unwrap()))
}

fn rate(count: u64, since: Instant) -> f64 {
    count as f64 / since.elapsed().as_secs_f64()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reads real logs from SHIP_REAL_DIR"]
async fn real_logs_stream_byte_for_byte_like_the_strict_readers() {
    let Some(real) = Real::from_env("byte for byte", "SHIP_REAL_DIR", "SHIP_REAL_CHAIN_ID") else {
        return;
    };
    let view = real.view(None);
    let (begin, end) = (view.begin(), view.end());
    let count = env_u32("SHIP_REAL_BLOCKS", 5000).min(end - begin);
    let tail = (count / 5).max(1);

    // Under the default stream threshold, and under one of 4 KiB that streams every larger
    // payload.
    for threshold in [4 << 20, 4 << 10] {
        let mut cfg = serve_config(real.store(None));
        cfg.stream_threshold = threshold;
        let server = start(cfg).await;
        let mut ws = connect(&server).await;
        for v1 in [false, true] {
            assert_same("status", &status(&mut ws, v1).await, &view.status(v1));
        }

        let started = Instant::now();
        let (mut bytes, mut rows) = (0u64, 0u64);
        // The first blocks, v0; some in the middle, v1; the last ones, asked past L*, which the
        // session stops at.
        let middle = (begin + (end - begin) / 2).min(end - tail);
        let runs = [
            (fetch(&view, false, begin, begin + count, 64), count),
            (fetch(&view, true, middle, middle + tail, 64), tail),
            (fetch(&view, false, end - tail, u32::MAX, 64), tail),
        ];
        for (req, n_blocks) in &runs {
            stream(&mut ws, req, *n_blocks, |n, msg| {
                let what = format!("block {n} (v1 {}, threshold {threshold})", req.v1);
                assert_same(&what, msg, &view.result(n, req));
                bytes += msg.len() as u64;
                if let Some(deltas) = view.payload(LogKind::ChainState, n) {
                    let t = tables(&deltas).unwrap_or_else(|e| panic!("{what}: {e:#}"));
                    rows += t.iter().map(|(_, r)| r.len() as u64).sum::<u64>();
                }
            })
            .await;
        }
        let compared: u32 = runs.iter().map(|r| r.1).sum();
        eprintln!(
            "real: threshold {threshold}: {compared} results ({:.1} MiB, {rows} delta rows) equal \
             the strict readers' in {:.2?} ({:.0} blocks/s with the comparison)",
            mib(bytes),
            started.elapsed(),
            rate(u64::from(compared), started)
        );

        // At L* the session idles, still answers status, and takes a new request.
        quiet(&mut ws).await;
        assert_same(
            "status at the end",
            &status(&mut ws, true).await,
            &view.status(true),
        );
        let req = fetch(&view, false, begin, begin + 2, 2);
        stream(&mut ws, &req, 2, |n, msg| {
            assert_same(&format!("block {n} again"), msg, &view.result(n, &req));
        })
        .await;
        // Below the first served block: an id-less result, which still costs a credit.
        if begin > 1 && view.id(begin - 1).is_none() {
            let req = fetch(&view, false, begin - 1, begin + 1, 2);
            stream(&mut ws, &req, 2, |n, msg| {
                assert_same(&format!("block {n} below"), msg, &view.result(n, &req));
            })
            .await;
        }
        drop(ws);
        server.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reads real logs from SHIP_REAL_DIR"]
async fn real_logs_stream_at_full_speed() {
    let Some(real) = Real::from_env("full speed", "SHIP_REAL_DIR", "SHIP_REAL_CHAIN_ID") else {
        return;
    };
    let view = real.view(None);
    let (begin, end) = (view.begin(), view.end());
    let external = var("SHIP_REAL_URL");
    let server = match &external {
        Some(_) => None,
        None => Some(start(serve_config(real.store(None))).await),
    };
    let url = external
        .clone()
        .unwrap_or_else(|| server.as_ref().unwrap().url());
    let (mut ws, _) = connect_async(url.as_str()).await.expect("connect");
    assert!(matches!(recv(&mut ws).await, Message::Text(_)));
    let st = parse_status(&status(&mut ws, false).await);
    assert_eq!(st.head, view.last, "{url} serves other logs");

    // Every block served, with every field, 1024 results in flight.
    let req = fetch(&view, false, begin, u32::MAX, 1024);
    reset_peak();
    let started = Instant::now();
    let (mut bytes, mut last) = (0u64, None::<BlockPos>);
    stream(&mut ws, &req, end - begin, |n, msg| {
        bytes += msg.len() as u64;
        let (this, prev) = positions(msg);
        if let Some(this) = this {
            assert_eq!(this.num, n, "the results are in order");
            if let (Some(prev), Some(last)) = (prev, last) {
                assert_eq!(prev, last, "block {n} follows the block before it");
            }
        }
        last = this;
    })
    .await;
    let elapsed = started.elapsed();
    assert_eq!(last.map(|p| p.num), Some(end - 1));
    let blocks = u64::from(end - begin);
    eprintln!(
        "real: {blocks} blocks ({begin}..{end}, {:.1} MiB of results) in {elapsed:.2?} from {}: \
         {:.0} blocks/s, {:.1} MiB/s; peak heap of this process {:.1} MiB",
        mib(bytes),
        if external.is_some() {
            "an external server"
        } else {
            "the in-process server"
        },
        rate(blocks, started),
        mib(bytes) / elapsed.as_secs_f64(),
        peak_mib()
    );
    if let Some(server) = server {
        eprintln!(
            "real: the server sent {} results and {:.1} MiB; {} reads; memory budget peak \
             {:.1} MiB of {:.0} MiB, {} builds ahead refused",
            server.metrics.results(),
            mib(server.metrics.bytes_sent()),
            server.metrics.reads(),
            mib(server.budget.peak()),
            mib(server.budget.capacity()),
            server.budget.refused()
        );
        drop(ws);
        server.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reads real logs from SHIP_REAL_DIR"]
async fn real_logs_split_by_range_meet_at_the_seams() {
    let Some(real) = Real::from_env("seams", "SHIP_REAL_DIR", "SHIP_REAL_CHAIN_ID") else {
        return;
    };
    let full = real.view(None);
    let (begin, end) = (full.begin(), full.end());
    assert!(end - begin >= 8, "too few blocks to split");
    // Two seams, at a third and two thirds; three servers as one operator would run them.
    let seams = [begin + (end - begin) / 3, begin + 2 * (end - begin) / 3 + 1];
    let ranges = [(begin, seams[0]), (seams[0], seams[1]), (seams[1], end)];
    let k = env_u32("SHIP_REAL_BLOCKS", 5000)
        .min(seams[0] - begin)
        .min(end - seams[1])
        .min(500);
    let mut servers = Vec::new();
    let mut views = Vec::new();
    for range in ranges {
        let view = real.view(Some(range));
        let report = check::check(&CheckConfig {
            store: real.store(Some(range)),
            deep: false,
            hash: false,
            warn_entry_bytes: WARN_ENTRY_BYTES,
        });
        assert_eq!(report.error_count(), 0, "{}", report.text());
        let server = start(serve_config(real.store(Some(range)))).await;
        let mut ws = connect(&server).await;
        let st = status(&mut ws, true).await;
        assert_same(&format!("status of {range:?}"), &st, &view.status(true));
        assert_eq!(view.end(), range.1, "each server serves up to its seam");
        assert_eq!(view.last.id, full.id(range.1 - 1).unwrap());
        servers.push((server, ws));
        views.push(view);
    }

    for (at, &seam) in seams.iter().enumerate() {
        let (left, right) = (&views[at], &views[at + 1]);
        // The left server: the k blocks before the seam, then nothing (it ends there).
        let req = fetch(left, false, seam - k, u32::MAX, 64);
        let mut last = None;
        let ws = &mut servers[at].1;
        stream(ws, &req, k, |n, msg| {
            assert_same(
                &format!("block {n} left of {seam}"),
                msg,
                &left.result(n, &req),
            );
            assert_same_but_head(&format!("block {n}"), msg, &full.result(n, &req));
            last = positions(msg).0;
        })
        .await;
        quiet(ws).await;
        assert_eq!(last.map(|p| p.num), Some(seam - 1));

        // The right server: from the seam on.
        let req = fetch(right, false, seam, seam + k, 64);
        let ws = &mut servers[at + 1].1;
        let mut first = true;
        stream(ws, &req, k, |n, msg| {
            assert_same(
                &format!("block {n} right of {seam}"),
                msg,
                &right.result(n, &req),
            );
            let (_, prev) = positions(msg);
            if first {
                first = false;
                if real.with_blocks() {
                    // The block log names the block before the seam: a router can check that
                    // the two servers continue one chain.
                    assert_eq!(
                        prev, last,
                        "block {n}'s prev_block is the left server's last"
                    );
                    assert_same_but_head(&format!("block {n}"), msg, &full.result(n, &req));
                } else {
                    // Without block logs no served log of the right server knows block
                    // seam - 1, so its first result has no prev_block.
                    assert_eq!(
                        prev,
                        None,
                        "block {n}: no log of the right server names {}",
                        n - 1
                    );
                    eprintln!(
                        "real: --blocks none: the first result after seam {seam} has no \
                         prev_block (the block before it is outside --range)"
                    );
                }
            } else {
                assert_same_but_head(&format!("block {n}"), msg, &full.result(n, &req));
            }
        })
        .await;
    }
    eprintln!(
        "real: seams at {seams:?}: {k} blocks on each side of each seam equal one server's \
         results"
    );
    for (server, ws) in servers {
        drop(ws);
        server.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "reads real logs from SHIP_REAL_DIR"]
async fn real_logs_first_state_entry_is_the_full_state_and_streams() {
    let Some(real) = Real::from_env("first entry", "SHIP_REAL_DIR", "SHIP_REAL_CHAIN_ID") else {
        return;
    };
    let (first, deltas) = real.logs.first_state().expect("a chain_state log");
    let len = deltas.len() as u64;
    let t = tables(&deltas).unwrap_or_else(|e| panic!("block {first}: {e:#}"));
    let rows: usize = t.iter().map(|(_, r)| r.len()).sum();
    let names: Vec<&str> = t.iter().map(|(n, _)| n.as_str()).collect();
    eprintln!(
        "real: the first chain_state entry (block {first}) is {len} bytes: {} tables, {rows} \
         rows ({})",
        t.len(),
        names.join(", ")
    );
    // The full state: every row present, and the chain's own records among them.
    assert!(t.iter().all(|(_, r)| r.iter().all(|(present, _)| *present)));
    for table in ["account", "permission", "global_property", "protocol_state"] {
        assert!(
            names.contains(&table),
            "no {table} table in the first entry"
        );
    }
    assert_eq!(real.logs.genesis_chain_id(), Some(real.chain_id));

    // As a large entry: `check` warns about it at a threshold below its size, and `--strict`
    // turns that into exit code 2.
    let view = real.view(None);
    let report = check::check(&CheckConfig {
        store: real.store(None),
        deep: false,
        hash: false,
        warn_entry_bytes: len - 1,
    });
    assert_eq!(report.error_count(), 0, "{}", report.text());
    let large = Finding::LargeEntry {
        kind: LogKind::ChainState,
        block: first,
        bytes: len,
    };
    assert!(report.findings.contains(&large), "{}", report.text());
    assert_eq!(report.exit_code(true), 2);

    // Streamed under a threshold below its size: its field is a head frame and data frames of
    // at most 1 MiB that reassemble to the entry.
    let mut cfg = serve_config(real.store(None));
    cfg.stream_threshold = len - 1;
    let server = start(cfg).await;
    let mut raw = Raw::connect(server.addr).await;
    let req = fetch(&view, false, first, first + 1, 1);
    raw.send_request(&Request::Blocks(req.clone())).await;
    let frames = raw.message().await.expect("a result");
    assert_same(
        &format!("block {first}"),
        &joined(&frames),
        &view.result(first, &req),
    );
    let (_, _, fields) = pieces(&frames);
    let (head, data) = &fields[kind_at(LogKind::ChainState)];
    assert_eq!(*head, protocol::encode_opt_len(Some(len)));
    assert!(data.iter().all(|c| c.len() <= CHUNK));
    assert_eq!(data.concat(), deltas);
    eprintln!(
        "real: block {first} streamed under a {}-byte threshold in {} data frames",
        len - 1,
        data.len()
    );
    drop(raw);
    server.stop().await.unwrap();

    // `--range` leaves it out: then no warning, and the next block is served first.
    let range = Some((first + 1, view.end()));
    let report = check::check(&CheckConfig {
        store: real.store(range),
        deep: false,
        hash: false,
        warn_entry_bytes: len - 1,
    });
    assert_eq!(report.error_count(), 0, "{}", report.text());
    assert!(!report.findings.contains(&large), "{}", report.text());
    let cut = real.view(range);
    let server = start(serve_config(real.store(range))).await;
    let mut ws = connect(&server).await;
    assert_same("status", &status(&mut ws, false).await, &cut.status(false));
    let req = fetch(&cut, false, first + 1, first + 3, 2);
    stream(&mut ws, &req, 2, |n, msg| {
        assert_same(&format!("block {n}"), msg, &cut.result(n, &req));
    })
    .await;
    drop(ws);
    server.stop().await.unwrap();
}

#[test]
#[ignore = "reads real logs from SHIP_REAL_DIR"]
fn real_logs_pass_a_deep_check() {
    let Some(real) = Real::from_env("deep check", "SHIP_REAL_DIR", "SHIP_REAL_CHAIN_ID") else {
        return;
    };
    let view = real.view(None);
    reset_peak();
    let started = Instant::now();
    let report = check::check(&CheckConfig {
        store: real.store(None),
        deep: true,
        hash: real.with_blocks(),
        warn_entry_bytes: WARN_ENTRY_BYTES,
    });
    let elapsed = started.elapsed();
    eprintln!("{}", report.text());
    assert_eq!(report.error_count(), 0);
    assert!(report.findings.iter().all(|f| !f.is_warning()));
    // A copy of a running node's logs may hold blocks its block log does not have yet.
    for w in &report.warnings {
        assert!(w.contains("the block logs do not cover the rest"), "{w}");
    }
    let deep = report.deep.expect("a deep check");
    let entries: u32 = LogKind::ALL
        .iter()
        .map(|&k| view.served(k))
        .map(|(b, e)| e - b)
        .sum();
    assert_eq!(deep.entries, u64::from(entries));
    assert_eq!(deep.blocks, u64::from(view.blocks.1 - view.blocks.0));
    let coverage = report.coverage.expect("the store opened");
    assert_eq!(coverage.last, view.last);
    assert_eq!(coverage.chain_id, real.chain_id);
    eprintln!(
        "real: deep check of {} entries ({:.1} MiB inflated) and {} blocks ({} hashed) in \
         {elapsed:.2?}: {:.0} entries/s, {:.1} MiB/s; peak heap {:.1} MiB",
        deep.entries,
        mib(deep.inflated_bytes),
        deep.blocks,
        deep.hashed,
        rate(deep.entries, started),
        mib(deep.inflated_bytes) / elapsed.as_secs_f64(),
        peak_mib()
    );
}

// --- ignored: the differential test against a real node ----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a SHiP node (SHIP_DIFF_NODE) and a copy of its logs (SHIP_DIFF_DIR)"]
async fn differential_against_a_real_node() {
    let Some(node) = var("SHIP_DIFF_NODE") else {
        eprintln!("SKIPPED differential: set SHIP_DIFF_NODE=ws://host:port and SHIP_DIFF_DIR");
        return;
    };
    let Some(real) = Real::from_env("differential", "SHIP_DIFF_DIR", "SHIP_DIFF_CHAIN_ID") else {
        return;
    };
    let range = env_range("SHIP_DIFF_RANGE");
    let view = real.view(range);
    let server = start(serve_config(real.store(range))).await;

    let (mut theirs, _) = connect_async(node.as_str())
        .await
        .unwrap_or_else(|e| panic!("connect to {node}: {e}"));
    let Message::Text(node_abi) = recv(&mut theirs).await else {
        panic!("{node} did not send its ABI first");
    };
    let (mut ours, _) = connect_async(server.url()).await.expect("connect");
    let Message::Text(our_abi) = recv(&mut ours).await else {
        panic!("no ABI");
    };
    let same_abi = node_abi.as_str() == our_abi.as_str();
    eprintln!(
        "diff: the node's ABI ({} bytes) {} the one served ({} bytes){}",
        node_abi.len(),
        if same_abi {
            "is byte-equal to"
        } else {
            "differs from"
        },
        our_abi.len(),
        if same_abi {
            ""
        } else {
            "; serve the node's with --abi"
        }
    );

    let node_status = parse_status(&status(&mut theirs, false).await);
    if let Some(id) = node_status.chain_id {
        assert_eq!(
            hex::encode(id),
            hex::encode(real.chain_id),
            "the node's chain"
        );
    }
    // What both serve: the archive's blocks that the node still has, at or below its LIB.
    let begin = view.begin();
    let end = view.end().min(node_status.lib.num + 1);
    assert!(
        begin < end,
        "the node's LIB {} is below the copy",
        node_status.lib.num
    );
    // The node must hold each log from where this server serves it.
    for (kind, theirs) in [
        (LogKind::Trace, node_status.trace),
        (LogKind::ChainState, node_status.chain_state),
    ] {
        let ours = view.served(kind);
        assert!(
            ours.0 == ours.1 || (theirs.0 <= ours.0 && theirs.0 < theirs.1),
            "the node's {} log holds {theirs:?}, this server serves {ours:?}",
            kind.label()
        );
    }
    let count = env_u32("SHIP_DIFF_BLOCKS", 10_000).min(end - begin);
    let mut versions = vec![false];
    if var("SHIP_DIFF_V1").is_some() {
        versions.push(true);
    }
    for v1 in versions {
        let req = fetch(&view, v1, begin, begin + count, 256);
        let started = Instant::now();
        let (from_node, from_us) = tokio::join!(
            collect(&mut theirs, &req, count),
            collect(&mut ours, &req, count)
        );
        let mut bytes = 0;
        for (i, (a, b)) in from_node.iter().zip(&from_us).enumerate() {
            let n = begin + i as u32;
            assert_same_but_head(&format!("block {n} (v1 {v1})"), b, a);
            assert_same(
                &format!("block {n}, the strict readers"),
                b,
                &view.result(n, &req),
            );
            bytes += a.len();
        }
        eprintln!(
            "diff: v1 {v1}: {count} results ({begin}..{}, {:.1} MiB, block {}, traces, deltas{}) \
             byte-equal to the node's but for head and LIB, in {:.2?}",
            begin + count,
            mib(bytes as u64),
            if req.fetch_block {
                "included"
            } else {
                "absent"
            },
            if v1 { ", finality" } else { "" },
            started.elapsed()
        );
    }
    eprintln!(
        "diff: node head {} LIB {}; archive L* {}",
        node_status.head.num, node_status.lib.num, view.last.num
    );
    drop((theirs, ours));
    server.stop().await.unwrap();
}

/// The `count` results answering `req`.
async fn collect(ws: &mut Ws, req: &BlocksReq, count: u32) -> Vec<Vec<u8>> {
    let mut got = Vec::with_capacity(count as usize);
    stream(ws, req, count, |_, msg| got.push(msg.to_vec())).await;
    got
}
