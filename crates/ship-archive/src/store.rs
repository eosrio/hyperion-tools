//! The store: one contiguous range of frozen logs, opened and validated once at startup, and the
//! block results built from it.
//!
//! [`Store::open`] finds the logs in the configured directories, opens every file with the strict
//! readers and assembles one catalog per log kind. What is served is then:
//!   1. each kind's catalog range,
//!   2. intersected with `--range` when one is given,
//!   3. with `--blocks required` (the default), the state-history ranges intersected with the
//!      block-log range, so that every served block has its `block`,
//!   4. `0,0` for a kind left empty.
//!
//! The last servable block `L*` is one below the highest served end. Frozen data is
//! irreversible, so status replies and every result report `L*` as both head and LIB. Before
//! anything is served, the ids at every file seam and at the edges of the served ranges must
//! agree across the logs (`--verify-ids seams`). Where the block log serves no next block to
//! name a block's id (at its served end), its id there is the hash of the block's header.
//!
//! Block ids come, in Spring's order, from the trace, chain_state and finality entry headers,
//! then from the block log, where block `n + 1`'s `previous` is the id of `n`. That last source
//! gives the first served block its `prev_block`, as the block before it is in no served log.
//!
//! A block result ([`Package`]) is built in two steps, both blocking. [`Store::plan`] finds
//! everything block `n`'s result needs and validates the entry headers it reads, without
//! reading a payload: that is enough to bound the memory the result will hold, which is what a
//! prefetch reserves. [`Store::read`] then reads the block and the payloads and validates all
//! of it before the writer sends a frame: the inflated payloads (zlib framing, adler32, sizes),
//! that every log names the same block id, and that the block follows `prev_block` and hashes
//! to `this_block`.
//! [`Store::build`] does both. A session reads entry offsets through its [`PageCache`], one
//! 1024-slot index page per log kind, so consecutive blocks cost one index read per page.
//!
//! Every read error carries a [`Source`] naming the log it came from, for the
//! `read_errors_total` metric.

use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result, bail, ensure};
use hyperion_ship::binio::FileCache;
use hyperion_ship::blocklog::{BlockLog, block_header};
use hyperion_ship::catalog::{self, Catalog, Finding};
use hyperion_ship::protocol::{self, BlockPos, MAX_BLOCK_BYTES, SPRING_SHIP_ABI, Status};
use hyperion_ship::shiplog::{EntryHeader, INPUT_WINDOW, LogKind, ShipLog};
use tokio_tungstenite::tungstenite::{Bytes, Utf8Bytes};
use tracing::warn;

use crate::fetch::{FetchLimits, Job, Package, Part, Streamed};

/// Offset of `previous` in a packed block: after `timestamp`, `producer` and `confirmed`.
const PREVIOUS_AT: usize = 14;
/// Index slots in one page of a [`PageCache`].
pub const INDEX_PAGE: u32 = 1024;
/// The longest result base without its block: head, LIB, `this_block`, `prev_block`, and the
/// presence byte and varuint32 length of `block`.
const BASE_BYTES: u64 = 2 * 36 + 2 * 37 + 1 + 5;
/// What reading one payload needs besides its output, one payload at a time: the inflater's
/// input window, its state, and the scratch of a counting pass.
const READ_SCRATCH: u64 = INPUT_WINDOW as u64 + (512 << 10);
/// The first and largest growth steps of the buffer a legacy (Type B) payload is inflated into.
const LEGACY_STEP_MIN: usize = 64 << 10;
const LEGACY_STEP_MAX: usize = 1 << 20;

/// The log a read error came from: attached to every error of a read as its context, so that
/// it can be counted per log (`downcast_ref::<Source>()`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    Ship(LogKind),
    Blocks,
}

impl Source {
    pub const ALL: [Self; 4] = [
        Self::Ship(LogKind::Trace),
        Self::Ship(LogKind::ChainState),
        Self::Ship(LogKind::Finality),
        Self::Blocks,
    ];

    /// `trace`, `chain_state`, `finality` or `blocks`, as metric labels name the logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Ship(kind) => kind.label(),
            Self::Blocks => "blocks",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ship(kind) => write!(f, "reading the {} log", kind.label()),
            Self::Blocks => f.write_str("reading the block log"),
        }
    }
}

/// One session's cache of index slots: for each log kind, the [`INDEX_PAGE`] slots around the
/// block it read last, read with one `positions` call. Builds of one session may run at once,
/// so the pages sit behind a lock; a page is read outside it.
#[derive(Debug, Default)]
pub struct PageCache {
    pages: Mutex<[Option<Page>; 3]>,
}

#[derive(Debug)]
struct Page {
    log: Arc<ShipLog>,
    first: u32,
    slots: Vec<u64>,
}

impl PageCache {
    /// The header of block `n`'s entry in `log` (a `kind` file holding `n`).
    pub fn header(&self, kind: LogKind, log: &Arc<ShipLog>, n: u32) -> Result<EntryHeader> {
        let pos = self.slot(kind, log, n)?;
        log.header_at(n, pos)
    }

    fn slot(&self, kind: LogKind, log: &Arc<ShipLog>, n: u32) -> Result<u64> {
        let at = kind_slot(kind);
        if let Some(p) = &self.lock()[at]
            && Arc::ptr_eq(&p.log, log)
            && n >= p.first
            && n - p.first < p.slots.len() as u32
        {
            return Ok(p.slots[(n - p.first) as usize]);
        }
        let (begin, end) = log.range();
        ensure!(
            (begin..end).contains(&n),
            "{}: block {n} is outside {begin}..{end}",
            log.log_path().display()
        );
        let first = begin + (n - begin) / INDEX_PAGE * INDEX_PAGE;
        let mut slots = Vec::new();
        log.positions(first, (end - first).min(INDEX_PAGE), &mut slots)?;
        let pos = slots[(n - first) as usize];
        self.lock()[at] = Some(Page {
            log: Arc::clone(log),
            first,
            slots,
        });
        Ok(pos)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, [Option<Page>; 3]> {
        // A page is replaced whole, so a poisoned lock still holds valid pages.
        self.pages.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn kind_slot(kind: LogKind) -> usize {
    match kind {
        LogKind::Trace => 0,
        LogKind::ChainState => 1,
        LogKind::Finality => 2,
    }
}

/// Everything block `job.n`'s result needs, found and its entry headers validated, before any
/// payload or block is read.
#[derive(Debug)]
pub struct Plan {
    job: Job,
    this: Option<BlockPos>,
    prev: Option<BlockPos>,
    /// The block log holding the block, and the block's length, when the result carries it.
    block: Option<(Arc<BlockLog>, u64)>,
    /// `traces`, `deltas` and, for a v1 request, `finality_data`: the entry to read, if the
    /// field is requested and served.
    fields: Vec<Option<(Arc<ShipLog>, EntryHeader)>>,
}

impl Plan {
    /// The most bytes reading this plan holds at once: the base, the block, every payload read
    /// inline, and the scratch of reading one payload. A payload above the stream threshold
    /// costs only the scratch here: the writer streams it.
    pub fn bound(&self, limits: &FetchLimits) -> u64 {
        let threshold = limits.stream_threshold;
        let block = self.block.as_ref().map_or(0, |(_, len)| *len);
        let inline: u64 = self
            .fields
            .iter()
            .flatten()
            .map(|(_, h)| match h.uncompressed() {
                Some(len) if len <= threshold => len,
                Some(_) => 0,
                // At most one byte past the threshold, and at most what the stream can inflate
                // to plus one growth step.
                None => threshold.saturating_add(1).min(
                    h.data_len()
                        .saturating_mul(MAX_INFLATE_RATIO)
                        .saturating_add(LEGACY_STEP_MAX as u64),
                ),
            })
            .sum();
        BASE_BYTES + block + inline + READ_SCRATCH
    }
}

/// The most a zlib stream inflates per byte of it (deflate's densest code).
const MAX_INFLATE_RATIO: u64 = 1032;

/// Whether results carry `block`, and so whether block logs are required.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum BlocksMode {
    /// The block logs: only the blocks they cover are served, each with its `block`.
    #[default]
    Required,
    /// Nowhere: no block logs are read and `block` is always absent (needs --chain-id).
    None,
}

/// Which block ids are cross-checked at startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum VerifyIds {
    /// Refuse to start unless the logs agree on the ids at every file seam and at the edges
    /// of every served range.
    #[default]
    Seams,
    /// Skip the check.
    Off,
}

/// Where the logs are and what to serve of them.
#[derive(Clone, Debug)]
pub struct StoreConfig {
    /// Directories holding state-history and block logs, flat or retained.
    pub dirs: Vec<PathBuf>,
    /// Also scan the directories' sub-directories.
    pub recursive: bool,
    /// Sidecar index directories, preferred over the index beside a log.
    pub index_dirs: Vec<PathBuf>,
    /// Serve only `[begin, end)`.
    pub range: Option<(u32, u32)>,
    pub blocks: BlocksMode,
    /// Required with [`BlocksMode::None`]; must match the block logs otherwise.
    pub chain_id: Option<[u8; 32]>,
    pub verify_ids: VerifyIds,
    pub max_open_files: usize,
    /// An ABI text to send instead of the vendored Spring SHiP ABI.
    pub abi: Option<PathBuf>,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            dirs: Vec::new(),
            recursive: false,
            index_dirs: Vec::new(),
            range: None,
            blocks: BlocksMode::Required,
            chain_id: None,
            verify_ids: VerifyIds::Seams,
            max_open_files: 256,
            abi: None,
        }
    }
}

/// The served range of each log, `[begin, end)`, or `(0, 0)` for a log not served.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Served {
    pub trace: (u32, u32),
    pub chain_state: (u32, u32),
    pub finality: (u32, u32),
    pub blocks: (u32, u32),
}

impl Served {
    pub fn ship(&self, kind: LogKind) -> (u32, u32) {
        match kind {
            LogKind::Trace => self.trace,
            LogKind::ChainState => self.chain_state,
            LogKind::Finality => self.finality,
        }
    }
}

/// The logs of one contiguous range, immutable once open.
pub struct Store {
    pub trace: Option<Catalog<ShipLog>>,
    pub chain_state: Option<Catalog<ShipLog>>,
    pub finality: Option<Catalog<ShipLog>>,
    pub blocks: Option<Catalog<BlockLog>>,
    pub served: Served,
    /// `{L*, id(L*)}`: the head and LIB of every status and result.
    pub last: BlockPos,
    pub chain_id: [u8; 32],
    abi: Utf8Bytes,
    status_v0: Bytes,
    status_v1: Bytes,
}

impl Store {
    /// Discover, open and validate the logs `cfg` names. Fails closed: on any file a strict
    /// reader refuses, on overlaps and holes, on disagreeing chain ids, and (with
    /// `--verify-ids seams`) on disagreeing block ids at seams. The findings are what was
    /// skipped or should be known (empty files, misleading names, duplicates).
    pub fn open(cfg: &StoreConfig) -> Result<(Self, Vec<Finding>)> {
        ensure!(!cfg.dirs.is_empty(), "no log directory given (--dir)");
        if let Some((begin, end)) = cfg.range {
            ensure!(begin < end, "--range {begin}-{end} holds no block");
        }
        let abi = load_abi(cfg.abi.as_deref())?;
        let cache = FileCache::new(cfg.max_open_files);
        let found = catalog::discover(&cfg.dirs, &cfg.index_dirs, cfg.recursive)?;

        let mut findings = Vec::new();
        let mut ship = |kind: LogKind| -> Result<Option<Catalog<ShipLog>>> {
            let (catalog, f) =
                catalog::ship_catalog_within(found.ship(kind), kind, &cache, cfg.range)?;
            findings.extend(f);
            Ok(catalog)
        };
        let trace = ship(LogKind::Trace)?;
        let chain_state = ship(LogKind::ChainState)?;
        let finality = ship(LogKind::Finality)?;

        let blocks = match cfg.blocks {
            BlocksMode::Required => {
                let (catalog, f) = catalog::block_catalog_within(&found.blocks, &cache, cfg.range)?;
                findings.extend(f);
                let what = if found.blocks.is_empty() {
                    "no block log (blocks.log or blocks-<first>-<last>.log) is in the log \
                     directories"
                } else if cfg.range.is_some() {
                    "no block log holds a block of --range"
                } else {
                    "every block log holds only its preamble"
                };
                Some(catalog.with_context(|| {
                    format!(
                        "{what}: block logs give results their `block`; serve without it with \
                         `--blocks none --chain-id <HEX>`"
                    )
                })?)
            }
            BlocksMode::None => {
                if !found.blocks.is_empty() {
                    warn!(
                        "--blocks none: ignoring the {} block logs found",
                        found.blocks.len()
                    );
                }
                None
            }
        };

        let chain_id = match (&blocks, cfg.chain_id) {
            (Some(b), want) => {
                let have = b.files()[0].chain_id();
                if let Some(want) = want {
                    ensure!(
                        have == want,
                        "the block logs belong to chain {}, not to --chain-id {}",
                        hex::encode(have),
                        hex::encode(want)
                    );
                }
                have
            }
            (None, Some(id)) => id,
            (None, None) => bail!(
                "--chain-id is required with --blocks none: without block logs nothing gives the \
                 chain id"
            ),
        };

        let blocks_range = blocks.as_ref().map(|b| clip(b.range(), cfg.range));
        let serve = |c: &Option<Catalog<ShipLog>>| {
            c.as_ref().map_or((0, 0), |c| {
                let r = clip(c.range(), cfg.range);
                blocks_range.map_or(r, |b| clip(r, Some(b)))
            })
        };
        let served = Served {
            trace: serve(&trace),
            chain_state: serve(&chain_state),
            finality: serve(&finality),
            blocks: blocks_range.unwrap_or((0, 0)),
        };
        let end = [served.trace, served.chain_state, served.finality]
            .iter()
            .map(|r| r.1)
            .max()
            .unwrap_or(0);
        if end == 0 {
            bail!(
                "{}",
                nothing_served(cfg, [&trace, &chain_state, &finality], &blocks)
            );
        }
        // A real log never holds block 0, so `L*` (head and LIB on the wire) is above 0.
        ensure!(end > 1, "the state-history logs claim to hold only block 0");

        let mut store = Self {
            trace,
            chain_state,
            finality,
            blocks,
            served,
            last: BlockPos::default(),
            chain_id,
            abi,
            status_v0: Bytes::new(),
            status_v1: Bytes::new(),
        };
        let last = end - 1;
        let id = store
            .id(last)?
            .with_context(|| format!("block {last}, the last served, has no id"))?;
        store.last = BlockPos { num: last, id };
        if cfg.verify_ids == VerifyIds::Seams {
            store.verify_seams()?;
        }
        store.status_v0 = Bytes::from(protocol::encode_status(false, &store.status()));
        store.status_v1 = Bytes::from(protocol::encode_status(true, &store.status()));
        Ok((store, findings))
    }

    /// A warning for each state-history log that holds blocks it does not serve because the
    /// block logs do not cover them (`--blocks required`, no `--range`).
    pub fn uncovered(&self, cfg: &StoreConfig) -> Vec<String> {
        if cfg.blocks != BlocksMode::Required || cfg.range.is_some() {
            return Vec::new();
        }
        let show = |(begin, end): (u32, u32)| {
            if begin == end {
                "nothing".to_owned()
            } else {
                format!("{begin}..{end}")
            }
        };
        LogKind::ALL
            .into_iter()
            .filter_map(|kind| {
                let (held, served) = (self.catalog(kind)?.range(), self.served.ship(kind));
                (held != served).then(|| {
                    format!(
                        "the {} logs hold {}..{} but only {} is served: the block logs do not \
                         cover the rest",
                        kind.label(),
                        held.0,
                        held.1,
                        show(served)
                    )
                })
            })
            .collect()
    }

    /// The catalog of `kind`, if any file of it is open.
    pub fn catalog(&self, kind: LogKind) -> Option<&Catalog<ShipLog>> {
        match kind {
            LogKind::Trace => self.trace.as_ref(),
            LogKind::ChainState => self.chain_state.as_ref(),
            LogKind::Finality => self.finality.as_ref(),
        }
    }

    /// The `kind` log file serving block `n`, if `n` is in that kind's served range.
    pub fn ship_log(&self, kind: LogKind, n: u32) -> Option<&Arc<ShipLog>> {
        if !contains(self.served.ship(kind), n) {
            return None;
        }
        self.catalog(kind)?.get(n)
    }

    /// The block log file serving block `n`, if `n` is in the served block range.
    pub fn block_log(&self, n: u32) -> Option<&Arc<BlockLog>> {
        if !contains(self.served.blocks, n) {
            return None;
        }
        self.blocks.as_ref()?.get(n)
    }

    /// Block `n`'s id, if the served logs know it: from the trace, chain_state or finality entry
    /// header, else from the `previous` field of block `n + 1` in the block log.
    pub fn id(&self, n: u32) -> Result<Option<[u8; 32]>> {
        self.id_with(n, None)
    }

    /// [`Self::id`], reading entry offsets through `pages` when given.
    fn id_with(&self, n: u32, pages: Option<&PageCache>) -> Result<Option<[u8; 32]>> {
        for kind in LogKind::ALL {
            if let Some(log) = self.ship_log(kind, n) {
                return header(kind, log, n, pages).map(|h| Some(h.id));
            }
        }
        if let Some(next) = n.checked_add(1)
            && let Some(log) = self.block_log(next)
        {
            return log.previous(next).context(Source::Blocks).map(Some);
        }
        Ok(None)
    }

    /// What a status reply reports: head and LIB at `L*`, the served ranges, the chain id.
    pub fn status(&self) -> Status {
        Status {
            head: self.last,
            lib: self.last,
            trace: self.served.trace,
            chain_state: self.served.chain_state,
            finality: self.served.finality,
            chain_id: self.chain_id,
        }
    }

    /// The whole status message answering a `get_status_request_v0` or, with `v1`, a
    /// `get_status_request_v1` (encoded once: the store never changes).
    pub fn status_bytes(&self, v1: bool) -> Bytes {
        if v1 {
            self.status_v1.clone()
        } else {
            self.status_v0.clone()
        }
    }

    /// The ABI text sent as each session's first message.
    pub fn abi(&self) -> &str {
        &self.abi
    }

    pub(crate) fn abi_message(&self) -> Utf8Bytes {
        self.abi.clone()
    }

    /// Read and validate the result for `job` (blocking file I/O and inflation): [`Self::plan`]
    /// then [`Self::read`].
    pub fn build(&self, job: &Job, limits: &FetchLimits) -> Result<Package> {
        self.read(self.plan(job, None)?, limits)
    }

    /// Find what block `job.n`'s result needs and validate the entry headers on the way, reading
    /// entry offsets through `pages` when given (blocking). Reads the headers of the requested
    /// fields and of the log `this_block`'s id comes from, and the bounds of the block; no
    /// payload. Every header read must name the block's id.
    pub fn plan(&self, job: &Job, pages: Option<&PageCache>) -> Result<Plan> {
        let n = job.n;
        let wanted = [
            job.flags.traces,
            job.flags.deltas,
            job.v1 && job.flags.finality,
        ];
        let mut headers: [Option<(&Arc<ShipLog>, EntryHeader)>; 3] = [None, None, None];
        let mut id = None;
        for (at, kind) in LogKind::ALL.into_iter().enumerate() {
            let Some(log) = self.ship_log(kind, n) else {
                continue;
            };
            // The first served log gives the id; the others are read only when requested.
            if id.is_some() && !wanted[at] {
                continue;
            }
            let h = header(kind, log, n, pages)?;
            let id = *id.get_or_insert(h.id);
            ensure!(
                h.id == id,
                "{}: the {} entry of block {n} names id {}, the block's id is {}",
                log.log_path().display(),
                kind.label(),
                hex::encode(h.id),
                hex::encode(id)
            );
            headers[at] = Some((log, h));
        }
        let id = match id {
            Some(id) => Some(id),
            None => self.id_with(n, pages)?,
        };
        let fields = 2 + usize::from(job.v1);
        let Some(id) = id else {
            // Spring still answers a block it has no id for, with head and LIB only; the
            // result costs a credit like any other.
            return Ok(Plan {
                job: *job,
                this: None,
                prev: None,
                block: None,
                fields: (0..fields).map(|_| None).collect(),
            });
        };
        let this = BlockPos { num: n, id };
        let prev = match n.checked_sub(1) {
            Some(p) => self.id_with(p, pages)?.map(|id| BlockPos { num: p, id }),
            None => None,
        };
        let block = match self.block_log(n) {
            Some(log) if job.flags.block => {
                let (_, len) = log.bounds(n).context(Source::Blocks)?;
                ensure!(
                    len <= MAX_BLOCK_BYTES as u64,
                    "{}: block {n} is {len} bytes, more than the {MAX_BLOCK_BYTES} a SHiP result \
                     carries",
                    log.log_path().display()
                );
                Some((Arc::clone(log), len))
            }
            _ => None,
        };
        let fields = headers
            .into_iter()
            .zip(wanted)
            .take(fields)
            .map(|(h, wanted)| h.filter(|_| wanted).map(|(log, h)| (Arc::clone(log), h)))
            .collect();
        Ok(Plan {
            job: *job,
            this: Some(this),
            prev,
            block,
            fields,
        })
    }

    /// Read and validate what `plan` found (blocking): the block, which must follow
    /// `prev_block`, and each payload. One up to the stream threshold is inflated and checked in
    /// memory; a larger one is checked by a full counting pass and left to stream.
    pub fn read(&self, plan: Plan, limits: &FetchLimits) -> Result<Package> {
        let Plan {
            job,
            this,
            prev,
            block,
            fields,
        } = plan;
        let mut base = protocol::encode_result_base(self.last, self.last, this, prev, None);
        let block = match (block, this) {
            (Some((log, _)), Some(this)) => {
                let body = block_body(&log, this, prev).context(Source::Blocks)?;
                // `block` is the base's last field: present, then its varuint32 length. The
                // body follows in frames of its own, so it is never copied into the base.
                base.pop();
                base.reserve_exact(1 + 5);
                base.push(1);
                put_varuint32(&mut base, body.len() as u32);
                Some(body)
            }
            _ => None,
        };
        let parts = fields
            .into_iter()
            .map(|field| match field {
                None => Ok(Part::Absent),
                Some((log, h)) => {
                    let kind = log.kind();
                    payload(log, h, limits).context(Source::Ship(kind))
                }
            })
            .collect::<Result<_>>()?;
        Ok(Package {
            job,
            base,
            block,
            parts,
        })
    }

    /// Check that every log agrees on the block ids at the seams between its files, at the
    /// edges of every served range, and across the block log's own files.
    fn verify_seams(&self) -> Result<()> {
        let mut points = BTreeSet::new();
        for kind in LogKind::ALL {
            let (begin, end) = self.served.ship(kind);
            if begin == end {
                continue;
            }
            points.extend([begin, end - 1]);
            if let Some(c) = self.catalog(kind) {
                for s in c.seams().filter(|&s| begin < s && s < end) {
                    points.extend([s - 1, s]);
                }
            }
        }
        if let Some(c) = &self.blocks {
            let (begin, end) = self.served.blocks;
            for s in c.seams().filter(|&s| begin < s && s < end) {
                points.extend([s - 1, s]);
                let (before, after) = (served_file(c, s - 1)?, served_file(c, s)?);
                let (id, follows) = (before.id(s - 1)?, after.previous(s)?);
                ensure!(
                    id == follows,
                    "the block logs do not link at block {s}: block {} in {} is {}, but block {s} \
                     in {} follows {}",
                    s - 1,
                    before.log_path().display(),
                    hex::encode(id),
                    after.log_path().display(),
                    hex::encode(follows)
                );
            }
        }
        for n in points {
            self.check_ids(n)?;
        }
        Ok(())
    }

    /// Every served log that knows block `n`'s id must give the same one. The block log gives
    /// it as block `n + 1`'s `previous` or, where it serves no block `n + 1` (at the end of
    /// what it serves, which `--range` or the end of the copied log puts at `L*`), as the hash
    /// of block `n`'s header: the id Spring checks before it attaches a block to a result.
    fn check_ids(&self, n: u32) -> Result<()> {
        let mut views = Vec::new();
        for kind in LogKind::ALL {
            if let Some(log) = self.ship_log(kind, n) {
                let what = format!("the {} log {}", kind.label(), log.log_path().display());
                views.push((what, log.id(n)?));
            }
        }
        if let Some(next) = n.checked_add(1)
            && let Some(log) = self.block_log(next)
        {
            let what = format!(
                "the block log {} (block {next} follows it)",
                log.log_path().display()
            );
            views.push((what, log.previous(next)?));
        } else if let Some(log) = self.block_log(n) {
            let what = format!(
                "the block log {} (the hash of block {n}'s header)",
                log.log_path().display()
            );
            views.push((what, log.id(n)?));
        }
        if let Some(((first, id), rest)) = views.split_first() {
            for (what, other) in rest {
                ensure!(
                    other == id,
                    "block ids disagree at block {n}: {first} has {}, {what} has {}; the logs \
                     must come from one chain and hold only irreversible blocks",
                    hex::encode(id),
                    hex::encode(other)
                );
            }
        }
        Ok(())
    }
}

/// The header of block `n`'s entry in `log`, a `kind` file holding `n`: through `pages` when
/// given, else through the index directly. Errors name the log.
fn header(
    kind: LogKind,
    log: &Arc<ShipLog>,
    n: u32,
    pages: Option<&PageCache>,
) -> Result<EntryHeader> {
    match pages {
        Some(pages) => pages.header(kind, log, n),
        None => log.header(n),
    }
    .context(Source::Ship(kind))
}

/// The payload of entry `h` of `log`. One up to the stream threshold is inflated and checked in
/// memory; a larger one is checked by a full counting pass and left to stream. A legacy (Type B)
/// entry declares no size, so it is first read inline, which stops one byte past the threshold;
/// only an entry that passes it is counted and streamed.
fn payload(log: Arc<ShipLog>, h: EntryHeader, limits: &FetchLimits) -> Result<Part> {
    let threshold = limits.stream_threshold;
    match h.uncompressed() {
        Some(len) if len > threshold => {
            log.verify_count(&h, len)?;
            Ok(Part::Streamed(Streamed {
                log,
                header: h,
                len,
            }))
        }
        Some(_) => {
            let mut out = Vec::new();
            log.read_inline(&h, threshold, &mut out)?;
            Ok(Part::Inline(out))
        }
        None => match read_legacy(&log, &h, threshold) {
            Ok(out) => Ok(Part::Inline(out)),
            Err(inline) => {
                let len = log.verify_count(&h, u64::MAX)?;
                if len <= threshold {
                    return Err(inline);
                }
                Ok(Part::Streamed(Streamed {
                    log,
                    header: h,
                    len,
                }))
            }
        },
    }
}

/// Inflate legacy (Type B) entry `h`, whose size is known only once it is inflated, refusing
/// one over `limit` bytes. The buffer grows by what each step needs and never holds more than
/// `limit + 1` bytes, so the payload a result holds stays within the stream threshold.
fn read_legacy(log: &ShipLog, h: &EntryHeader, limit: u64) -> Result<Vec<u8>> {
    let mut stream = log.stream(h)?;
    let mut out = Vec::new();
    let mut step = usize::try_from(h.data_len().saturating_mul(4))
        .unwrap_or(usize::MAX)
        .clamp(LEGACY_STEP_MIN, LEGACY_STEP_MAX);
    loop {
        // At most one byte past the limit, which is enough to tell that it is exceeded.
        let room = limit
            .saturating_add(1)
            .saturating_sub(out.len() as u64)
            .min(step as u64) as usize;
        out.reserve_exact(room);
        let more = stream.next_chunk(&mut out, room)?;
        ensure!(
            out.len() as u64 <= limit,
            "{}: block {} at offset {}: the payload inflates to more than the {limit}-byte limit",
            log.log_path().display(),
            h.num(),
            h.pos
        );
        if !more {
            out.shrink_to_fit();
            return Ok(out);
        }
        step = step.saturating_mul(2).min(LEGACY_STEP_MAX);
    }
}

/// fc's `unsigned_int`: a canonical varuint32.
fn put_varuint32(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Block `this`'s stored `signed_block`, which must fit a SHiP result, follow `prev` and be
/// block `this` itself: its header must hash to `this.id`. Spring attaches a block only when
/// its id is the result's `this_block`; here coverage honesty promises the block, so a block
/// of another fork fails the read instead, before the first frame.
fn block_body(log: &BlockLog, this: BlockPos, prev: Option<BlockPos>) -> Result<Vec<u8>> {
    let n = this.num;
    let mut body = Vec::new();
    log.body(n, &mut body)?;
    ensure!(
        body.len() <= MAX_BLOCK_BYTES,
        "{}: block {n} is {} bytes, more than the {MAX_BLOCK_BYTES} a SHiP result carries",
        log.log_path().display(),
        body.len()
    );
    if let Some(prev) = prev {
        // The reader guarantees a whole block header, so `previous` is there.
        let follows = &body[PREVIOUS_AT..PREVIOUS_AT + 32];
        ensure!(
            follows == prev.id,
            "{}: block {n} follows {}, but block {} is {}",
            log.log_path().display(),
            hex::encode(follows),
            prev.num,
            hex::encode(prev.id)
        );
    }
    // A sha256 of the header bytes already read: no more I/O.
    let id = block_header(&body)
        .with_context(|| format!("{}: block {n}", log.log_path().display()))?
        .id;
    ensure!(
        id == this.id,
        "{}: block {n}'s header hashes to {}, but the logs give block {n} the id {}: the logs \
         must come from one chain and hold only irreversible blocks",
        log.log_path().display(),
        hex::encode(id),
        hex::encode(this.id)
    );
    Ok(body)
}

/// The file of `catalog` holding block `n`, which the served range guarantees.
fn served_file<L: catalog::Ranged>(catalog: &Catalog<L>, n: u32) -> Result<&Arc<L>> {
    catalog
        .get(n)
        .with_context(|| format!("block {n} is in no file of the catalog"))
}

/// The ABI text to send: the vendored Spring literal, or the `--abi` file, which must be JSON.
fn load_abi(path: Option<&Path>) -> Result<Utf8Bytes> {
    let Some(path) = path else {
        return Ok(Utf8Bytes::from_static(SPRING_SHIP_ABI));
    };
    let text =
        fs::read_to_string(path).with_context(|| format!("read --abi {}", path.display()))?;
    serde_json::from_str::<serde_json::Value>(&text)
        .with_context(|| format!("--abi {} is not a JSON ABI", path.display()))?;
    Ok(Utf8Bytes::from(text))
}

/// Why no state-history block is served, naming what was found.
fn nothing_served(
    cfg: &StoreConfig,
    ship: [&Option<Catalog<ShipLog>>; 3],
    blocks: &Option<Catalog<BlockLog>>,
) -> String {
    let dirs: Vec<String> = cfg.dirs.iter().map(|d| d.display().to_string()).collect();
    if ship.iter().all(|c| c.is_none()) {
        return format!(
            "no state-history log (trace_history, chain_state_history or finality_data_history) \
             holds a block{} in {}",
            if cfg.range.is_some() {
                " of --range"
            } else {
                ""
            },
            dirs.join(", ")
        );
    }
    let mut held = Vec::new();
    for (kind, c) in LogKind::ALL.into_iter().zip(ship) {
        if let Some(c) = c {
            let (b, e) = c.range();
            held.push(format!("{} {b}..{e}", kind.label()));
        }
    }
    let blocks = blocks.as_ref().map_or(String::new(), |c| {
        let (b, e) = c.range();
        format!(", the block logs {b}..{e}")
    });
    let range = cfg
        .range
        .map_or(String::new(), |(b, e)| format!(", --range {b}-{e}"));
    format!(
        "no state-history block is served: the logs hold {}{blocks}{range}, which do not overlap",
        held.join(", ")
    )
}

/// `[begin, end)` cut to `window`; `(0, 0)` when nothing is left.
fn clip(r: (u32, u32), window: Option<(u32, u32)>) -> (u32, u32) {
    let (begin, end) = window.map_or(r, |(wb, we)| (r.0.max(wb), r.1.min(we)));
    if begin < end { (begin, end) } else { (0, 0) }
}

fn contains(r: (u32, u32), n: u32) -> bool {
    r.0 <= n && n < r.1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::Flags;
    use hyperion_ship::testkit::ship::Encoding;
    use hyperion_ship::testkit::{FileName, SyntheticChain};

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ship-archive-store-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn index_pages_give_the_offsets_the_index_holds() {
        let dir = tmp();
        let chain = SyntheticChain::new(2, 3400);
        let cache = FileCache::new(4);
        let open = |blocks: std::ops::Range<u32>, name| {
            let pair = chain
                .write_ship(
                    dir.path(),
                    LogKind::Trace,
                    blocks,
                    Encoding::SizedStored,
                    name,
                )
                .unwrap();
            let log = ShipLog::open(&pair.log, &pair.index, LogKind::Trace, &cache).unwrap();
            Arc::new(log.unwrap())
        };
        // A retained file of three pages and a short fourth, then the head log after it.
        let (retained, head) = (
            open(2..3302, FileName::Retained),
            open(3302..3402, FileName::Head),
        );
        let pages = PageCache::default();
        let page = |pages: &PageCache| pages.lock()[0].as_ref().map(|p| (p.first, p.slots.len()));
        for n in (2..3302).chain([1500, 3, 3301, 1027]) {
            let h = pages.header(LogKind::Trace, &retained, n).unwrap();
            assert_eq!(h, retained.header(n).unwrap(), "block {n}");
        }
        // Pages are 1024 slots from the file's first block; the last one is shorter.
        assert_eq!(page(&pages), Some((1026, 1024)));
        pages.header(LogKind::Trace, &retained, 3301).unwrap();
        assert_eq!(page(&pages), Some((3074, 228)));
        // Another file of the kind replaces the page; going back reads the old one again.
        for n in [3302, 3401, 3300, 3350] {
            let log = if n < 3302 { &retained } else { &head };
            assert_eq!(
                pages.header(LogKind::Trace, log, n).unwrap(),
                log.header(n).unwrap()
            );
        }
        assert_eq!(page(&pages), Some((3302, 100)));
        // Outside the file: an error, and the page stays.
        assert!(pages.header(LogKind::Trace, &head, 3402).is_err());
        assert!(pages.header(LogKind::Trace, &head, 3301).is_err());
        assert_eq!(page(&pages), Some((3302, 100)));
    }

    #[test]
    fn a_plan_reserves_at_least_what_its_package_holds() {
        let dir = tmp();
        // Payloads of 0 to 120 KiB around a 64 KiB stream threshold, in three framings.
        let mut chain = SyntheticChain::new(2, 30);
        for n in 2..32 {
            for (kind, shift) in LogKind::ALL.into_iter().zip(0..) {
                chain.set_payload_len(kind, n, (n as usize + shift) % 7 * (20 << 10));
            }
        }
        let encodings = [
            Encoding::SizedStored,
            Encoding::LegacyLength,
            Encoding::LegacyZero,
        ];
        for (kind, encoding) in LogKind::ALL.into_iter().zip(encodings) {
            chain
                .write_ship(dir.path(), kind, 2..32, encoding, FileName::Head)
                .unwrap();
        }
        chain
            .write_blocks(dir.path(), 2..32, FileName::Head)
            .unwrap();
        let cfg = StoreConfig {
            dirs: vec![dir.path().to_path_buf()],
            ..StoreConfig::default()
        };
        let (store, _) = Store::open(&cfg).unwrap();
        let limits = FetchLimits {
            stream_threshold: 64 << 10,
        };
        let max_block = (2..32).map(|n| chain.block(n).body.len()).max().unwrap() as u64;
        let pages = PageCache::default();
        let (mut inline, mut streamed) = (0, 0);
        // Blocks 1 and 32 have no id: their results hold the base only.
        for n in 1..33 {
            for v1 in [false, true] {
                let job = Job {
                    generation: 1,
                    n,
                    v1,
                    flags: Flags {
                        block: true,
                        traces: true,
                        deltas: true,
                        finality: v1,
                    },
                };
                let plan = store.plan(&job, Some(&pages)).unwrap();
                let bound = plan.bound(&limits);
                let package = store.read(plan, &limits).unwrap();
                let held = package.held() as u64;
                assert!(
                    held + READ_SCRATCH <= bound,
                    "block {n}: {held} bytes held, {bound} reserved"
                );
                assert!(held <= limits.package_bound(max_block), "block {n}");
                for part in &package.parts {
                    match part {
                        Part::Inline(_) => inline += 1,
                        Part::Streamed(_) => streamed += 1,
                        Part::Absent => {}
                    }
                }
            }
        }
        assert!(
            inline > 0 && streamed > 0,
            "{inline} inline, {streamed} streamed"
        );
    }

    #[test]
    fn clipping_keeps_the_overlap_or_nothing() {
        assert_eq!(clip((10, 20), None), (10, 20));
        assert_eq!(clip((10, 20), Some((15, 30))), (15, 20));
        assert_eq!(clip((10, 20), Some((0, 12))), (10, 12));
        assert_eq!(clip((10, 20), Some((20, 30))), (0, 0));
        assert_eq!(clip((0, 0), Some((0, 30))), (0, 0));
        assert!(contains((10, 20), 10) && contains((10, 20), 19));
        assert!(!contains((10, 20), 20) && !contains((0, 0), 0));
    }
}
