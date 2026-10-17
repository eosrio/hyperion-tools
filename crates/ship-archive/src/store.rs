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
//! agree across the logs (`--verify-ids seams`).
//!
//! Block ids come, in Spring's order, from the trace, chain_state and finality entry headers,
//! then from the block log, where block `n + 1`'s `previous` is the id of `n`. That last source
//! gives the first served block its `prev_block`, as the block before it is in no served log.
//!
//! [`Store::build`] reads one block result ([`Package`]) and validates all of it before the
//! writer sends a frame: entry headers, the inflated payloads (zlib framing, adler32, sizes),
//! that every log names the same block id, and that the block follows `prev_block`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use hyperion_ship::binio::FileCache;
use hyperion_ship::blocklog::BlockLog;
use hyperion_ship::catalog::{self, Catalog, Finding};
use hyperion_ship::protocol::{self, BlockPos, MAX_BLOCK_BYTES, SPRING_SHIP_ABI, Status};
use hyperion_ship::shiplog::{LogKind, ShipLog};
use tokio_tungstenite::tungstenite::{Bytes, Utf8Bytes};
use tracing::warn;

use crate::fetch::{FetchLimits, Job, Package, Part, Streamed};

/// Offset of `previous` in a packed block: after `timestamp`, `producer` and `confirmed`.
const PREVIOUS_AT: usize = 14;

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
        for kind in LogKind::ALL {
            if let Some(log) = self.ship_log(kind, n) {
                return log.id(n).map(Some);
            }
        }
        if let Some(next) = n.checked_add(1)
            && let Some(log) = self.block_log(next)
        {
            return log.previous(next).map(Some);
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

    /// Read and validate the result for `job` (blocking file I/O and inflation).
    pub fn build(&self, job: &Job, limits: &FetchLimits) -> Result<Package> {
        let n = job.n;
        let Some(id) = self.id(n)? else {
            // Spring still answers a block it has no id for, with head and LIB only; the
            // result costs a credit like any other.
            let parts = (0..2 + usize::from(job.v1)).map(|_| Part::Absent).collect();
            let base = protocol::encode_result_base(self.last, self.last, None, None, None);
            return Ok(Package {
                job: *job,
                base,
                parts,
            });
        };
        let this = BlockPos { num: n, id };
        let prev = match n.checked_sub(1) {
            Some(p) => self.id(p)?.map(|id| BlockPos { num: p, id }),
            None => None,
        };
        let block = match self.block_log(n) {
            Some(log) if job.flags.block => Some(block_body(log, this, prev)?),
            _ => None,
        };
        let base =
            protocol::encode_result_base(self.last, self.last, Some(this), prev, block.as_deref());
        drop(block);

        let mut parts = Vec::with_capacity(3);
        parts.push(self.part(job.flags.traces, LogKind::Trace, this, limits)?);
        parts.push(self.part(job.flags.deltas, LogKind::ChainState, this, limits)?);
        if job.v1 {
            parts.push(self.part(job.flags.finality, LogKind::Finality, this, limits)?);
        }
        Ok(Package {
            job: *job,
            base,
            parts,
        })
    }

    /// The `kind` payload of block `this`, when `wanted` and served. One up to the stream
    /// threshold is inflated and checked in memory; a larger one is checked by a full counting
    /// pass and left to stream. A legacy (Type B) entry declares no size, so it is first read
    /// inline, which stops one byte past the threshold; only an entry that passes it is
    /// counted and streamed.
    fn part(
        &self,
        wanted: bool,
        kind: LogKind,
        this: BlockPos,
        limits: &FetchLimits,
    ) -> Result<Part> {
        let n = this.num;
        let log = match self.ship_log(kind, n) {
            Some(log) if wanted => log,
            _ => return Ok(Part::Absent),
        };
        let h = log.header(n)?;
        ensure!(
            h.id == this.id,
            "{}: the {} entry of block {n} names id {}, the block's id is {}",
            log.log_path().display(),
            kind.label(),
            hex::encode(h.id),
            hex::encode(this.id)
        );
        let threshold = limits.stream_threshold;
        let streamed = |len| {
            Part::Streamed(Streamed {
                log: Arc::clone(log),
                header: h,
                len,
            })
        };
        if let Some(len) = h.uncompressed()
            && len > threshold
        {
            log.verify_count(&h, len)?;
            return Ok(streamed(len));
        }
        let mut out = Vec::new();
        match log.read_inline(&h, threshold, &mut out) {
            Ok(()) => Ok(Part::Inline(out)),
            Err(inline) if h.uncompressed().is_none() => {
                drop(out);
                let len = log.verify_count(&h, u64::MAX)?;
                if len <= threshold {
                    return Err(inline);
                }
                Ok(streamed(len))
            }
            Err(e) => Err(e),
        }
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

    /// Every served log that knows block `n`'s id must give the same one.
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

/// Block `this`'s stored `signed_block`, which must fit a SHiP result and follow `prev`.
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
