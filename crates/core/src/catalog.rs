//! Discovery of flat and retained SHiP and block log files across directories, the
//! contiguous per-kind catalogs built from their contents, and the findings reported on the way.
//!
//! Spring names a node's current log `<stem>.log` and each retained (rotated or split) file
//! `<stem>-<first>-<last>.log`, with the index beside it as `<stem>.index`. The stems are
//! `trace_history`, `chain_state_history` and `finality_data_history` for state history and
//! `blocks` for the block log. [`discover`] finds these names in any number of directories and
//! pairs each log with its index: a sidecar `<index-dir>/<stem>.index` when one exists (what
//! `antelope-ship-archive index` writes for a log whose own index is missing or wrong, without
//! touching the log's directory), otherwise the one beside the log.
//!
//! [`ship_catalog`] and [`block_catalog`] open every file of one kind with the strict readers
//! and assemble them the way Spring's `log_catalog` does, by the blocks each file holds rather
//! than by its name, but read-only and failing closed:
//!   * an empty file (a head log right after a rotation, a block log holding only its
//!     preamble) is skipped: [`Finding::Empty`];
//!   * a retained name that disagrees with the contents is a warning, and the contents are
//!     used: [`Finding::NameMismatch`];
//!   * a second copy of a file, holding the same blocks with the same ids at both ends, is
//!     dropped: [`Finding::Duplicate`];
//!   * any other overlap, and any hole, is an error: a catalog is one contiguous range;
//!   * block logs must all belong to one chain.
//!
//! The `_within` variants first set aside the files holding no block of a window, which is how
//! a server restricted to one range serves one side of an overlap or a hole.
//! [`large_entries`] reports the entries too large for some clients ([`Finding::LargeEntry`]).

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};

use crate::binio::FileCache;
use crate::blocklog::BlockLog;
use crate::shiplog::{LogKind, ShipLog};

/// The stem of every block-log file name.
const BLOCKS_STEM: &str = "blocks";
/// Index slots read at a time by [`large_entries`].
const SCAN_PAGE: u32 = 1024;
/// The most a zlib stream inflates per byte of it: deflate's densest code spends two bits (a
/// one-bit length code and a one-bit distance code) on a 258-byte match.
const MAX_INFLATE_RATIO: u64 = 1032;

/// A log file that knows which blocks it holds.
pub trait Ranged {
    /// The blocks the file holds, `[begin, end)`.
    fn range(&self) -> (u32, u32);
    fn log_path(&self) -> &Path;
    /// The ids of the first and the last block: what tells two copies of one file apart from
    /// two files over the same blocks.
    fn tail_ids(&self) -> Result<([u8; 32], [u8; 32])>;
}

impl Ranged for ShipLog {
    fn range(&self) -> (u32, u32) {
        ShipLog::range(self)
    }

    fn log_path(&self) -> &Path {
        ShipLog::log_path(self)
    }

    fn tail_ids(&self) -> Result<([u8; 32], [u8; 32])> {
        let (begin, end) = ShipLog::range(self);
        Ok((self.id(begin)?, self.id(end - 1)?))
    }
}

impl Ranged for BlockLog {
    fn range(&self) -> (u32, u32) {
        BlockLog::range(self)
    }

    fn log_path(&self) -> &Path {
        BlockLog::log_path(self)
    }

    fn tail_ids(&self) -> Result<([u8; 32], [u8; 32])> {
        let (first, end) = BlockLog::range(self);
        Ok((self.id(first)?, self.id(end - 1)?))
    }
}

/// The files of one log kind, sorted by first block, each starting where the one before it
/// ends. Never empty: the builders return `None` instead.
pub struct Catalog<L> {
    files: Vec<Arc<L>>,
}

impl<L> Clone for Catalog<L> {
    fn clone(&self) -> Self {
        Self {
            files: self.files.clone(),
        }
    }
}

impl<L: fmt::Debug> fmt::Debug for Catalog<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Catalog")
            .field("files", &self.files)
            .finish()
    }
}

impl<L: Ranged> Catalog<L> {
    /// The blocks the catalog holds, `[begin, end)`.
    pub fn range(&self) -> (u32, u32) {
        (
            self.files[0].range().0,
            self.files[self.files.len() - 1].range().1,
        )
    }

    /// The file holding block `n`: the last file starting at or before `n` (an upper bound on
    /// the first blocks, as Spring's catalog looks blocks up), if it reaches `n`.
    pub fn get(&self, n: u32) -> Option<&Arc<L>> {
        let after = self.files.partition_point(|f| f.range().0 <= n);
        let file = &self.files[after.checked_sub(1)?];
        (n < file.range().1).then_some(file)
    }

    /// The files, in block order.
    pub fn files(&self) -> &[Arc<L>] {
        &self.files
    }

    /// The first block of every file but the first: where one file hands over to the next.
    pub fn seams(&self) -> impl Iterator<Item = u32> + '_ {
        self.files.iter().skip(1).map(|f| f.range().0)
    }
}

/// A log file and the index to read it with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathPair {
    pub log: PathBuf,
    pub index: PathBuf,
}

/// The log files [`discover`] found, per kind, in the order found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovered {
    pub trace: Vec<PathPair>,
    pub chain_state: Vec<PathPair>,
    pub finality: Vec<PathPair>,
    pub blocks: Vec<PathPair>,
}

impl Discovered {
    /// The state-history files of `kind`.
    pub fn ship(&self, kind: LogKind) -> &[PathPair] {
        match kind {
            LogKind::Trace => &self.trace,
            LogKind::ChainState => &self.chain_state,
            LogKind::Finality => &self.finality,
        }
    }

    /// Whether no log of any kind was found.
    pub fn is_empty(&self) -> bool {
        LogKind::ALL.iter().all(|&k| self.ship(k).is_empty()) && self.blocks.is_empty()
    }

    fn ship_mut(&mut self, kind: LogKind) -> &mut Vec<PathPair> {
        match kind {
            LogKind::Trace => &mut self.trace,
            LogKind::ChainState => &mut self.chain_state,
            LogKind::Finality => &mut self.finality,
        }
    }
}

/// Something about the input that a catalog skipped, or that an operator should know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Finding {
    /// A 0-byte state-history log or a block log holding only its preamble: skipped.
    Empty { path: PathBuf },
    /// A retained file whose name claims other blocks than it holds; the contents are used.
    /// Both ranges are `[begin, end)`: a name `<stem>-<first>-<last>` claims
    /// `[first, last + 1)` (numbers beyond `u32` shown as `u32::MAX`).
    NameMismatch {
        path: PathBuf,
        by_name: (u32, u32),
        by_content: (u32, u32),
    },
    /// A second copy of a file, holding the same blocks with the same ids at both ends. The
    /// first one found is kept.
    Duplicate { kept: PathBuf, dropped: PathBuf },
    /// An entry inflating to more than a scan's threshold ([`large_entries`]).
    LargeEntry {
        kind: LogKind,
        block: u32,
        bytes: u64,
    },
}

impl Finding {
    /// Whether the finding is a warning about the input (a name that misleads, an entry too
    /// large for some clients) rather than a record of what was skipped.
    pub fn is_warning(&self) -> bool {
        matches!(self, Self::NameMismatch { .. } | Self::LargeEntry { .. })
    }
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty { path } => write!(f, "{} is empty; skipped", path.display()),
            Self::NameMismatch {
                path,
                by_name: (nb, ne),
                by_content: (cb, ce),
            } => write!(
                f,
                "{}: the name says blocks {nb}..{ne}, the file holds {cb}..{ce}; the contents \
                 are used",
                path.display()
            ),
            Self::Duplicate { kept, dropped } => write!(
                f,
                "{} holds the same blocks as {}; dropped as a duplicate",
                dropped.display(),
                kept.display()
            ),
            Self::LargeEntry { kind, block, bytes } => write!(
                f,
                "the {} entry of block {block} inflates to {bytes} bytes",
                kind.label()
            ),
        }
    }
}

/// Find the SHiP and block logs in `dirs`, and in their sub-directories when `recursive`, each
/// paired with its index: `<stem>.index` from the first of `index_dirs` holding one, otherwise
/// `<stem>.index` beside the log, whether or not it exists (opening the log reports a missing
/// index). Names are matched exactly as Spring writes them,
/// `^(trace_history|chain_state_history|finality_data_history|blocks)(-\d+-\d+)?\.log$`, and
/// other files are ignored. Files are listed in the order of `dirs`, by name within a
/// directory, and a directory or file reached twice (listed twice, nested, or through a link)
/// is listed once.
pub fn discover(dirs: &[PathBuf], index_dirs: &[PathBuf], recursive: bool) -> Result<Discovered> {
    let mut scan = Scan {
        sidecars: sidecar_indexes(index_dirs)?,
        recursive,
        dirs_seen: HashSet::new(),
        logs_seen: HashSet::new(),
        found: Discovered::default(),
    };
    for dir in dirs {
        scan.dir(dir)?;
    }
    Ok(scan.found)
}

/// Open every state-history log of `kind` in `pairs` and assemble the contiguous catalog they
/// form, or `None` when every file is empty. Fails on a file the strict reader refuses, an
/// overlap other than an identical copy, or a hole.
pub fn ship_catalog(
    pairs: &[PathPair],
    kind: LogKind,
    cache: &Arc<FileCache>,
) -> Result<(Option<Catalog<ShipLog>>, Vec<Finding>)> {
    ship_catalog_within(pairs, kind, cache, None)
}

/// [`ship_catalog`] over the files holding some block of `window` (`[begin, end)`; `None` for
/// all of them). Every file is still opened and validated and its findings reported; only the
/// overlaps and holes between the files kept count.
pub fn ship_catalog_within(
    pairs: &[PathPair],
    kind: LogKind,
    cache: &Arc<FileCache>,
    window: Option<(u32, u32)>,
) -> Result<(Option<Catalog<ShipLog>>, Vec<Finding>)> {
    let mut findings = Vec::new();
    let mut logs = Vec::with_capacity(pairs.len());
    for p in pairs {
        match ShipLog::open(&p.log, &p.index, kind, cache)? {
            None => findings.push(Finding::Empty {
                path: p.log.clone(),
            }),
            Some(log) => {
                check_name(&p.log, log.range(), &mut findings);
                logs.push(log);
            }
        }
    }
    let catalog = assemble(kind.label(), logs, window, &mut findings)?;
    Ok((catalog, findings))
}

/// Open every block log in `pairs` and assemble the contiguous catalog they form, or `None`
/// when every file holds only its preamble. Fails as [`ship_catalog`] does, and when the files
/// do not all carry the same chain id.
pub fn block_catalog(
    pairs: &[PathPair],
    cache: &Arc<FileCache>,
) -> Result<(Option<Catalog<BlockLog>>, Vec<Finding>)> {
    block_catalog_within(pairs, cache, None)
}

/// [`block_catalog`] over the files holding some block of `window`, as in
/// [`ship_catalog_within`]. The chain id must agree across every file opened, kept or not.
pub fn block_catalog_within(
    pairs: &[PathPair],
    cache: &Arc<FileCache>,
    window: Option<(u32, u32)>,
) -> Result<(Option<Catalog<BlockLog>>, Vec<Finding>)> {
    let mut findings = Vec::new();
    let mut logs: Vec<BlockLog> = Vec::with_capacity(pairs.len());
    for p in pairs {
        let Some(log) = BlockLog::open(&p.log, &p.index, cache)? else {
            findings.push(Finding::Empty {
                path: p.log.clone(),
            });
            continue;
        };
        if let Some(first) = logs.first() {
            ensure!(
                log.chain_id() == first.chain_id(),
                "{} belongs to chain {}, but {} to chain {}: block logs must all come from one \
                 chain",
                p.log.display(),
                hex::encode(log.chain_id()),
                first.log_path().display(),
                hex::encode(first.chain_id())
            );
        }
        check_name(&p.log, log.range(), &mut findings);
        logs.push(log);
    }
    let catalog = assemble("block", logs, window, &mut findings)?;
    Ok((catalog, findings))
}

/// A [`Finding::LargeEntry`] for every entry of `catalog` among `blocks` (`[begin, end)`) that
/// inflates to more than `threshold` bytes, in block order. Reads the index and the entry
/// headers only, except that a Type B entry (EOSIO 1.8 to Leap 3) is inflated once to learn
/// its size, unless its stream is too short to reach the threshold at all (deflate's densest
/// code yields at most 1032 bytes per byte of stream). Type A sizes are as their headers
/// declare them; serving an entry checks the size when it is inflated.
pub fn large_entries(
    catalog: &Catalog<ShipLog>,
    blocks: (u32, u32),
    threshold: u64,
) -> Result<Vec<Finding>> {
    let mut found = Vec::new();
    let mut slots = Vec::new();
    for log in catalog.files() {
        let (begin, end) = log.range();
        let (mut n, to) = (begin.max(blocks.0), end.min(blocks.1));
        while n < to {
            let count = (to - n).min(SCAN_PAGE);
            log.positions(n, count, &mut slots)?;
            for (&pos, block) in slots.iter().zip(n..) {
                let h = log.header_at(block, pos)?;
                let bytes = match h.uncompressed() {
                    Some(size) => size,
                    None if h.data_len().saturating_mul(MAX_INFLATE_RATIO) <= threshold => {
                        continue;
                    }
                    None => log.verify_count(&h, u64::MAX)?,
                };
                if bytes > threshold {
                    found.push(Finding::LargeEntry {
                        kind: log.kind(),
                        block,
                        bytes,
                    });
                }
            }
            n += count;
        }
    }
    Ok(found)
}

/// Sort `logs` by range, drop identical copies and refuse any other overlap or hole between
/// the ones holding a block of `window`. `what` names the kind in errors.
fn assemble<L: Ranged>(
    what: &str,
    logs: Vec<L>,
    window: Option<(u32, u32)>,
    findings: &mut Vec<Finding>,
) -> Result<Option<Catalog<L>>> {
    let mut logs: Vec<L> = logs
        .into_iter()
        .filter(|l| {
            let (begin, end) = l.range();
            window.is_none_or(|(wb, we)| begin < we && wb < end)
        })
        .collect();
    // Stable, so of two copies of one file the one found first is kept.
    logs.sort_by_key(|l| l.range());

    let mut files: Vec<Arc<L>> = Vec::with_capacity(logs.len());
    let mut problems = Vec::new();
    // The file reaching furthest so far: the last one kept, unless an overlap reached further.
    let mut reach: Option<Arc<L>> = None;
    for log in logs {
        let log = Arc::new(log);
        let (begin, end) = log.range();
        if let Some(prev) = reach.clone() {
            let (pb, pe) = prev.range();
            if (pb, pe) == (begin, end) {
                if prev.tail_ids()? == log.tail_ids()? {
                    findings.push(Finding::Duplicate {
                        kept: prev.log_path().to_path_buf(),
                        dropped: log.log_path().to_path_buf(),
                    });
                } else {
                    problems.push(format!(
                        "{} and {} both hold blocks {begin}..{end}, with different block ids: \
                         remove one",
                        prev.log_path().display(),
                        log.log_path().display()
                    ));
                }
                continue;
            }
            if begin < pe {
                problems.push(format!(
                    "{} (blocks {pb}..{pe}) overlaps {} (blocks {begin}..{end}): use --range or \
                     remove one",
                    prev.log_path().display(),
                    log.log_path().display()
                ));
                if end > pe {
                    reach = Some(log);
                }
                continue;
            }
            if begin > pe {
                problems.push(format!(
                    "blocks {pe}..{begin} are in no file (a hole between {} and {}): run one \
                     server per contiguous range",
                    prev.log_path().display(),
                    log.log_path().display()
                ));
            }
        }
        files.push(Arc::clone(&log));
        reach = Some(log);
    }
    if !problems.is_empty() {
        bail!(
            "the {what} logs do not form one contiguous range: {}",
            problems.join("; ")
        );
    }
    Ok((!files.is_empty()).then_some(Catalog { files }))
}

/// Records a [`Finding::NameMismatch`] when `path` is a retained name claiming other blocks
/// than the `held` ones.
fn check_name(path: &Path, held: (u32, u32), findings: &mut Vec<Finding>) {
    let claimed = path
        .file_name()
        .and_then(OsStr::to_str)
        .and_then(classify)
        .and_then(|(_, claimed)| claimed);
    let Some((first, last)) = claimed else {
        return;
    };
    let by_name = (first, last.saturating_add(1));
    if by_name != (u64::from(held.0), u64::from(held.1)) {
        let clamp = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
        findings.push(Finding::NameMismatch {
            path: path.to_path_buf(),
            by_name: (clamp(by_name.0), clamp(by_name.1)),
            by_content: held,
        });
    }
}

/// Which log a file name belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LogFile {
    Ship(LogKind),
    Blocks,
}

/// What a log file name says: the log it belongs to and, for a retained name, the first and
/// last blocks it claims, as written (so possibly beyond `u32`). `None` for any other name.
fn classify(name: &str) -> Option<(LogFile, Option<(u64, u64)>)> {
    let stem = name.strip_suffix(".log")?;
    let (file, rest) = LogKind::ALL
        .into_iter()
        .map(|k| (LogFile::Ship(k), k.stem()))
        .chain([(LogFile::Blocks, BLOCKS_STEM)])
        .find_map(|(file, prefix)| Some((file, stem.strip_prefix(prefix)?)))?;
    if rest.is_empty() {
        return Some((file, None));
    }
    let (first, last) = rest.strip_prefix('-')?.split_once('-')?;
    Some((file, Some((block_number(first)?, block_number(last)?))))
}

/// A run of ASCII digits (`\d` in Spring's regex), saturating past `u64`.
fn block_number(digits: &str) -> Option<u64> {
    let valid = !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit());
    valid.then(|| digits.parse().unwrap_or(u64::MAX))
}

/// Every `*.index` file in `index_dirs` by file name, the first directory holding a name
/// winning.
fn sidecar_indexes(index_dirs: &[PathBuf]) -> Result<HashMap<OsString, PathBuf>> {
    let mut found = HashMap::new();
    for dir in index_dirs {
        let context = || format!("read index directory {}", dir.display());
        for entry in fs::read_dir(dir).with_context(context)? {
            let path = entry.with_context(context)?.path();
            let is_index = path.extension().is_some_and(|e| e == "index")
                && fs::metadata(&path).is_ok_and(|m| m.is_file());
            if let (true, Some(name)) = (is_index, path.file_name()) {
                found.entry(name.to_os_string()).or_insert(path);
            }
        }
    }
    Ok(found)
}

/// The state of one [`discover`] walk.
struct Scan {
    sidecars: HashMap<OsString, PathBuf>,
    recursive: bool,
    /// Canonical paths, so that a directory or file reached twice is listed once.
    dirs_seen: HashSet<PathBuf>,
    logs_seen: HashSet<PathBuf>,
    found: Discovered,
}

impl Scan {
    fn dir(&mut self, dir: &Path) -> Result<()> {
        let context = || format!("read log directory {}", dir.display());
        let real = fs::canonicalize(dir).with_context(context)?;
        ensure!(real.is_dir(), "{} is not a directory", dir.display());
        if !self.dirs_seen.insert(real) {
            return Ok(());
        }
        let mut entries = Vec::new();
        for entry in fs::read_dir(dir).with_context(context)? {
            let entry = entry.with_context(context)?;
            entries.push((entry.file_name(), entry.path()));
        }
        entries.sort();

        let mut subdirs = Vec::new();
        for (name, path) in entries {
            let log = name.to_str().and_then(|n| Some((n, classify(n)?.0)));
            match log {
                Some((name, file)) => self.log(file, dir, name, path),
                // A link to a directory is followed; a cycle ends at a directory already seen.
                None if self.recursive && fs::metadata(&path).is_ok_and(|m| m.is_dir()) => {
                    subdirs.push(path);
                }
                None => {}
            }
        }
        for sub in subdirs {
            self.dir(&sub)?;
        }
        Ok(())
    }

    fn log(&mut self, file: LogFile, dir: &Path, name: &str, path: PathBuf) {
        // A dangling link has no canonical path; opening it reports the problem.
        let real = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !self.logs_seen.insert(real) {
            return;
        }
        let index_name = format!("{}.index", name.strip_suffix(".log").unwrap_or(name));
        let index = match self.sidecars.get(OsStr::new(&index_name)) {
            Some(sidecar) => sidecar.clone(),
            None => dir.join(&index_name),
        };
        let pair = PathPair { log: path, index };
        match file {
            LogFile::Ship(kind) => self.found.ship_mut(kind).push(pair),
            LogFile::Blocks => self.found.blocks.push(pair),
        }
    }
}

/// The fixtures come from the testkit writers, so these run with `--features testkit` (as CI
/// does).
#[cfg(all(test, feature = "testkit"))]
mod tests {
    use std::fmt::Debug;
    use std::ops::Range;

    use super::*;
    use crate::binio::TestDir;
    use crate::testkit::blocks::{self as kit_blocks, Preamble};
    use crate::testkit::ship::{self as kit_ship, Encoding, LogBuilder};
    use crate::testkit::{FileName, SyntheticChain};

    fn cache() -> Arc<FileCache> {
        FileCache::new(16)
    }

    fn err<T: Debug>(r: Result<T>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    fn names(pairs: &[PathPair]) -> Vec<String> {
        pairs
            .iter()
            .map(|p| p.log.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn ranges<L: Ranged>(c: &Catalog<L>) -> Vec<(u32, u32)> {
        c.files().iter().map(|f| f.range()).collect()
    }

    fn ships(pairs: &[PathPair], kind: LogKind) -> (Catalog<ShipLog>, Vec<Finding>) {
        let (c, findings) = ship_catalog(pairs, kind, &cache()).unwrap();
        (c.expect("a catalog"), findings)
    }

    fn trace(chain: &SyntheticChain, dir: &Path, blocks: Range<u32>, name: FileName) -> PathPair {
        chain
            .write_ship(dir, LogKind::Trace, blocks, Encoding::SizedStored, name)
            .unwrap()
    }

    /// Blocks 2..71 as a node with every stride at 20 leaves them: the head logs (the last ten
    /// blocks) in `state`, retained state history in `retained`, retained block logs in
    /// `blocks`. Returns the directories in that order.
    fn node_layout(dir: &TestDir, chain: &SyntheticChain) -> Vec<PathBuf> {
        let dirs: Vec<PathBuf> = ["state", "retained", "blocks"]
            .map(|d| dir.path().join(d))
            .into();
        for (kind, encoding) in LogKind::ALL.into_iter().zip(Encoding::ALL) {
            chain
                .write_ship_partitioned(&dirs[0], &dirs[1], kind, 2..71, 20, encoding)
                .unwrap();
        }
        chain.write_blocks_split(&dirs[2], 2..61, 20).unwrap();
        chain
            .write_blocks(&dirs[0], 61..71, FileName::Head)
            .unwrap();
        dirs
    }

    #[test]
    fn file_names_are_matched_as_spring_writes_them() {
        use LogFile::{Blocks, Ship};
        let trace = Ship(LogKind::Trace);
        for (name, want) in [
            ("trace_history.log", Some((trace, None))),
            (
                "chain_state_history.log",
                Some((Ship(LogKind::ChainState), None)),
            ),
            (
                "finality_data_history.log",
                Some((Ship(LogKind::Finality), None)),
            ),
            ("blocks.log", Some((Blocks, None))),
            ("trace_history-2-20.log", Some((trace, Some((2, 20))))),
            ("blocks-1-1000.log", Some((Blocks, Some((1, 1000))))),
            ("trace_history-0002-020.log", Some((trace, Some((2, 20))))),
            (
                "trace_history-1-99999999999.log",
                Some((trace, Some((1, 99_999_999_999)))),
            ),
            (
                "trace_history-1-999999999999999999999999.log",
                Some((trace, Some((1, u64::MAX)))),
            ),
            // Spring matches the shape only; the contents decide.
            ("trace_history-30-20.log", Some((trace, Some((30, 20))))),
        ] {
            assert_eq!(classify(name), want, "{name}");
        }
        for name in [
            "trace_history.index",
            "trace_history",
            "trace-history-2-20.log",
            "trace_history-2.log",
            "trace_history-2-.log",
            "trace_history--2-20.log",
            "trace_history-2-20-40.log",
            "trace_history-a-b.log",
            "trace_history-+2-20.log",
            "trace_history-\u{0663}-\u{0664}.log",
            "trace_history_2_20.log",
            "trace_history-2-20.log.bak",
            "trace_history.log.1",
            "Trace_history.log",
            "xtrace_history.log",
            "trace_historyx.log",
            "blocks-.log",
            "blocks.index",
            "blocks_1_2.log",
            "chain_state-2-20.log",
            ".log",
        ] {
            assert_eq!(classify(name), None, "{name}");
        }
    }

    #[test]
    fn head_and_retained_files_across_directories_form_one_catalog() {
        let dir = TestDir::new("catalog-layout");
        let chain = SyntheticChain::new(2, 69);
        let dirs = node_layout(&dir, &chain);
        let found = discover(&dirs, &[], false).unwrap();
        assert_eq!(
            names(&found.trace),
            [
                "trace_history.log",
                "trace_history-2-20.log",
                "trace_history-21-40.log",
                "trace_history-41-60.log"
            ]
        );
        assert_eq!(names(&found.finality)[0], "finality_data_history.log");
        assert_eq!(
            names(&found.blocks),
            [
                "blocks.log",
                "blocks-2-20.log",
                "blocks-21-40.log",
                "blocks-41-60.log"
            ]
        );
        for p in found.trace.iter().chain(&found.blocks) {
            assert_eq!(p.index, p.log.with_extension("index"), "beside the log");
        }
        assert!(!found.is_empty() && Discovered::default().is_empty());

        let want = vec![(2, 21), (21, 41), (41, 61), (61, 71)];
        for kind in LogKind::ALL {
            let (c, findings) = ships(found.ship(kind), kind);
            assert_eq!(findings, [], "{kind:?}");
            assert_eq!((ranges(&c), c.range()), (want.clone(), (2, 71)));
            assert_eq!(c.seams().collect::<Vec<_>>(), [21, 41, 61]);
            for n in 2..71 {
                let log = c.get(n).unwrap();
                assert_eq!((log.kind(), log.id(n).unwrap()), (kind, chain.id(n)));
            }
        }
        let (blocks, findings) = block_catalog(&found.blocks, &cache()).unwrap();
        let blocks = blocks.unwrap();
        assert_eq!(findings, []);
        assert_eq!((ranges(&blocks), blocks.range()), (want, (2, 71)));
        for n in 2..71 {
            let log = blocks.get(n).unwrap();
            assert_eq!(log.id(n).unwrap(), chain.id(n));
            assert_eq!(log.previous(n).unwrap(), chain.id(n - 1));
        }
        assert_eq!(
            blocks.files()[0].tail_ids().unwrap(),
            (chain.id(2), chain.id(20))
        );
    }

    #[test]
    fn lookups_at_seams_find_the_file_holding_the_block() {
        let dir = TestDir::new("catalog-seams");
        let chain = SyntheticChain::new(2, 69);
        let dirs = node_layout(&dir, &chain);
        let found = discover(&dirs, &[], false).unwrap();
        let (c, _) = ships(&found.trace, LogKind::Trace);
        let file_of = |n| {
            c.get(n)
                .map(|f| c.files().iter().position(|g| Arc::ptr_eq(f, g)).unwrap())
        };
        for (n, want) in [
            (0, None),
            (1, None),
            (2, Some(0)),
            (20, Some(0)),
            (21, Some(1)),
            (40, Some(1)),
            (41, Some(2)),
            (60, Some(2)),
            (61, Some(3)),
            (70, Some(3)),
            (71, None),
            (u32::MAX, None),
        ] {
            assert_eq!(file_of(n), want, "block {n}");
        }
        for seam in c.seams() {
            assert_eq!(c.get(seam - 1).unwrap().range().1, seam);
            assert_eq!(c.get(seam).unwrap().range().0, seam);
        }
        let copy = c.clone();
        assert!(Arc::ptr_eq(&copy.files()[2], &c.files()[2]));
        assert!(format!("{copy:?}").starts_with("Catalog { files: [ShipLog {"));

        // One file: the catalog is that file, and it has no seams.
        let (one, _) = ships(&found.trace[1..2], LogKind::Trace);
        assert_eq!((one.range(), one.seams().count()), ((2, 21), 0));
        assert!(one.get(1).is_none() && one.get(21).is_none());
        assert!(Arc::ptr_eq(one.get(2).unwrap(), one.get(20).unwrap()));
    }

    #[test]
    fn discovery_descends_only_when_recursive_and_ignores_other_files() {
        let dir = TestDir::new("catalog-recursive");
        let chain = SyntheticChain::new(2, 69);
        let state = dir.path().join("state");
        chain
            .write_ship_partitioned(
                &state,
                &state.join("retained"),
                LogKind::Trace,
                2..71,
                20,
                Encoding::SizedStored,
            )
            .unwrap();
        chain
            .write_ship(
                &state.join("retained").join("archive"),
                LogKind::ChainState,
                2..21,
                Encoding::SizedStored,
                FileName::Retained,
            )
            .unwrap();
        for other in [
            "notes.txt",
            "trace_history.log.bak",
            "trace_history-2-20.log.old",
            "blocks_1_2.log",
        ] {
            fs::write(state.join(other), b"not a log").unwrap();
        }

        let flat = discover(std::slice::from_ref(&state), &[], false).unwrap();
        assert_eq!(names(&flat.trace), ["trace_history.log"]);
        assert!(flat.chain_state.is_empty() && flat.blocks.is_empty());

        let deep = discover(std::slice::from_ref(&state), &[], true).unwrap();
        assert_eq!(
            names(&deep.trace),
            [
                "trace_history.log",
                "trace_history-2-20.log",
                "trace_history-21-40.log",
                "trace_history-41-60.log"
            ]
        );
        assert_eq!(names(&deep.chain_state), ["chain_state_history-2-20.log"]);
        let (c, _) = ships(&deep.trace, LogKind::Trace);
        assert_eq!(c.range(), (2, 71));
    }

    #[test]
    fn a_directory_or_file_reached_twice_is_listed_once() {
        let dir = TestDir::new("catalog-twice");
        let chain = SyntheticChain::new(2, 69);
        let dirs = node_layout(&dir, &chain);
        let once = discover(&dirs, &[], false).unwrap();
        let mut twice = dirs.clone();
        twice.extend(dirs.iter().cloned());
        twice.push(dir.path().join("state").join(".").join("..").join("state"));
        assert_eq!(discover(&twice, &[], false).unwrap(), once);
        // Nested directories with a recursive walk: the inner one is reached from the outer.
        let nested = [dir.path().to_path_buf(), dirs[1].clone()];
        let found = discover(&nested, &[], true).unwrap();
        assert_eq!((found.trace.len(), found.blocks.len()), (4, 4));
    }

    #[cfg(unix)]
    #[test]
    fn links_are_followed_without_listing_a_file_twice_or_looping() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::new("catalog-links");
        let chain = SyntheticChain::new(2, 19);
        let real = dir.path().join("real");
        let pair = trace(&chain, &real, 2..21, FileName::Retained);
        // A linked file elsewhere, a link from the directory to itself, and a dangling link.
        let other = dir.path().join("other");
        fs::create_dir_all(&other).unwrap();
        symlink(&pair.log, other.join("trace_history-2-20.log")).unwrap();
        symlink(&pair.index, other.join("trace_history-2-20.index")).unwrap();
        symlink(&real, real.join("loop")).unwrap();
        symlink(real.join("missing"), other.join("blocks.log")).unwrap();

        let found = discover(&[real, other.clone()], &[], true).unwrap();
        assert_eq!(found.trace, [pair], "one file, reached once");
        assert_eq!(
            names(&found.blocks),
            ["blocks.log"],
            "dangling, still listed"
        );
        assert!(err(block_catalog(&found.blocks, &cache())).contains("blocks.log"));

        let linked = discover(std::slice::from_ref(&other), &[], false).unwrap();
        let (c, findings) = ships(&linked.trace, LogKind::Trace);
        assert_eq!((c.range(), findings), ((2, 21), vec![]));
    }

    #[test]
    fn a_sidecar_index_takes_precedence_over_the_one_beside_the_log() {
        let dir = TestDir::new("catalog-sidecar");
        let chain = SyntheticChain::new(2, 39);
        let logs = dir.path().join("logs");
        let a = trace(&chain, &logs, 2..21, FileName::Retained);
        let b = trace(&chain, &logs, 21..41, FileName::Retained);
        // The index beside `a` is cut short and a rebuilt copy sits in a sidecar directory; a
        // second sidecar directory holds stale ones, which lose to the first.
        let good = fs::read(&a.index).unwrap();
        fs::write(&a.index, &good[..good.len() - 8]).unwrap();
        let (side, stale) = (dir.path().join("side"), dir.path().join("stale"));
        for d in [&side, &stale] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(side.join("trace_history-2-20.index"), &good).unwrap();
        fs::write(stale.join("trace_history-2-20.index"), b"stale").unwrap();
        fs::write(stale.join("trace_history-21-40.index"), b"stale").unwrap();
        fs::create_dir_all(side.join("trace_history-21-40.index")).unwrap();

        let plain = discover(std::slice::from_ref(&logs), &[], false).unwrap();
        let e = err(ship_catalog(&plain.trace, LogKind::Trace, &cache()));
        assert!(
            e.contains("index mismatch") && e.contains("trace_history-2-20"),
            "{e}"
        );

        let both = [side.clone(), stale.clone()];
        let found = discover(std::slice::from_ref(&logs), &both, false).unwrap();
        assert_eq!(found.trace[0].index, side.join("trace_history-2-20.index"));
        // A directory named like an index is not one; the next sidecar directory has the file.
        assert_eq!(
            found.trace[1].index,
            stale.join("trace_history-21-40.index")
        );
        let e = err(ship_catalog(&found.trace, LogKind::Trace, &cache()));
        assert!(
            e.contains("index mismatch") && e.contains("trace_history-21-40"),
            "{e}"
        );

        let found = discover(
            std::slice::from_ref(&logs),
            std::slice::from_ref(&side),
            false,
        )
        .unwrap();
        assert_eq!(found.trace[1].index, b.index);
        let (c, findings) = ships(&found.trace, LogKind::Trace);
        assert_eq!((c.range(), findings), ((2, 41), vec![]));
        assert_eq!(
            c.get(5).unwrap().index_path(),
            side.join("trace_history-2-20.index")
        );
    }

    #[test]
    fn missing_directories_are_errors() {
        let dir = TestDir::new("catalog-missing");
        let missing = dir.path().join("missing");
        let e = err(discover(std::slice::from_ref(&missing), &[], false));
        assert!(
            e.contains("read log directory") && e.contains("missing"),
            "{e}"
        );
        let e = err(discover(&[], std::slice::from_ref(&missing), false));
        assert!(
            e.contains("read index directory") && e.contains("missing"),
            "{e}"
        );
        let file = dir.write("trace_history.log", &[]);
        let e = err(discover(std::slice::from_ref(&file), &[], false));
        assert!(e.contains("is not a directory"), "{e}");
        assert!(discover(&[], &[], false).unwrap().is_empty());
    }

    #[test]
    fn a_name_that_disagrees_with_the_contents_is_a_warning() {
        let dir = TestDir::new("catalog-names");
        let chain = SyntheticChain::new(2, 39);
        let lying = trace(
            &chain,
            dir.path(),
            2..21,
            FileName::Custom("trace_history-2-25"),
        );
        let huge = trace(
            &chain,
            dir.path(),
            21..31,
            FileName::Custom("trace_history-21-99999999999"),
        );
        let honest = trace(&chain, dir.path(), 31..41, FileName::Retained);
        let (c, findings) = ships(&[lying.clone(), huge.clone(), honest], LogKind::Trace);
        assert_eq!(c.range(), (2, 41), "the contents are used");
        assert_eq!(
            findings,
            [
                Finding::NameMismatch {
                    path: lying.log.clone(),
                    by_name: (2, 26),
                    by_content: (2, 21)
                },
                Finding::NameMismatch {
                    path: huge.log,
                    by_name: (21, u32::MAX),
                    by_content: (21, 31)
                },
            ]
        );
        assert!(findings.iter().all(Finding::is_warning));
        let text = findings[0].to_string();
        assert!(
            text.contains("trace_history-2-25.log")
                && text.contains("the name says blocks 2..26, the file holds 2..21"),
            "{text}"
        );

        let blocks = chain
            .write_blocks(dir.path(), 2..21, FileName::Custom("blocks-1-20"))
            .unwrap();
        let (c, findings) = block_catalog(std::slice::from_ref(&blocks), &cache()).unwrap();
        assert_eq!(c.unwrap().range(), (2, 21));
        assert_eq!(
            findings,
            [Finding::NameMismatch {
                path: blocks.log,
                by_name: (1, 21),
                by_content: (2, 21)
            }]
        );
    }

    #[test]
    fn an_identical_copy_is_dropped() {
        let dir = TestDir::new("catalog-duplicate");
        let chain = SyntheticChain::new(2, 39);
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let first = trace(&chain, &a, 2..21, FileName::Retained);
        trace(&chain, &a, 21..41, FileName::Retained);
        let copy = trace(&chain, &b, 2..21, FileName::Retained);
        chain.write_blocks(&a, 2..21, FileName::Retained).unwrap();
        chain.write_blocks(&b, 2..21, FileName::Retained).unwrap();

        let found = discover(&[a.clone(), b.clone()], &[], false).unwrap();
        let (c, findings) = ships(&found.trace, LogKind::Trace);
        assert_eq!(ranges(&c), [(2, 21), (21, 41)]);
        assert_eq!(
            findings,
            [Finding::Duplicate {
                kept: first.log.clone(),
                dropped: copy.log.clone()
            }]
        );
        assert!(!findings[0].is_warning());
        assert!(findings[0].to_string().contains("dropped as a duplicate"));
        assert_eq!(c.get(2).unwrap().log_path(), first.log);

        let (blocks, findings) = block_catalog(&found.blocks, &cache()).unwrap();
        assert_eq!(ranges(&blocks.unwrap()), [(2, 21)]);
        assert_eq!(
            findings,
            [Finding::Duplicate {
                kept: a.join("blocks-2-20.log"),
                dropped: b.join("blocks-2-20.log")
            }]
        );
    }

    #[test]
    fn a_partial_overlap_is_an_error_naming_both_files() {
        let dir = TestDir::new("catalog-overlap");
        let chain = SyntheticChain::new(2, 39);
        let a = trace(&chain, dir.path(), 2..21, FileName::Retained);
        let b = trace(&chain, dir.path(), 15..31, FileName::Retained);
        let c = trace(&chain, dir.path(), 31..41, FileName::Retained);
        let pairs = [a.clone(), b, c];
        let e = err(ship_catalog(&pairs, LogKind::Trace, &cache()));
        for want in [
            "the trace logs do not form one contiguous range",
            "trace_history-2-20.log (blocks 2..21) overlaps",
            "trace_history-15-30.log (blocks 15..31): use --range or remove one",
        ] {
            assert!(e.contains(want), "{want:?} in {e}");
        }
        // A window that leaves one of them out serves the other side.
        let (cat, _) =
            ship_catalog_within(&pairs, LogKind::Trace, &cache(), Some((21, 41))).unwrap();
        assert_eq!(ranges(&cat.unwrap()), [(15, 31), (31, 41)]);

        // The same blocks with other ids (another fork, another chain) are no duplicate.
        let mut fork = LogBuilder::new(Encoding::SizedStored);
        for n in 2..21 {
            fork.push(
                kit_ship::synthetic_id(n, 1),
                &chain.payload(LogKind::Trace, n),
            );
        }
        let (log, index) = fork
            .write(&dir.path().join("fork"), "trace_history-2-20")
            .unwrap();
        let e = err(ship_catalog(
            &[a, PathPair { log, index }],
            LogKind::Trace,
            &cache(),
        ));
        assert!(
            e.contains("both hold blocks 2..21, with different block ids: remove one"),
            "{e}"
        );
    }

    #[test]
    fn a_hole_is_an_error_naming_the_range() {
        let dir = TestDir::new("catalog-hole");
        let chain = SyntheticChain::new(2, 89);
        let pairs = [(2, 21), (41, 61), (81, 91)]
            .map(|(b, e)| trace(&chain, dir.path(), b..e, FileName::Retained));
        let e = err(ship_catalog(&pairs, LogKind::Trace, &cache()));
        for want in [
            "the trace logs do not form one contiguous range",
            "blocks 21..41 are in no file (a hole between",
            "trace_history-2-20.log and",
            "trace_history-41-60.log): run one server per contiguous range",
            "blocks 61..81 are in no file",
        ] {
            assert!(e.contains(want), "{want:?} in {e}");
        }
        let (cat, _) =
            ship_catalog_within(&pairs, LogKind::Trace, &cache(), Some((41, 61))).unwrap();
        assert_eq!(ranges(&cat.unwrap()), [(41, 61)]);
        for window in [(100, 200), (30, 30), (21, 41)] {
            let (cat, findings) =
                ship_catalog_within(&pairs, LogKind::Trace, &cache(), Some(window)).unwrap();
            assert!(cat.is_none() && findings.is_empty(), "{window:?}");
        }
    }

    #[test]
    fn empty_files_are_skipped() {
        let dir = TestDir::new("catalog-empty");
        let chain = SyntheticChain::new(2, 59);
        let (state, retained) = (dir.path().join("state"), dir.path().join("retained"));
        // Block 60 is a stride multiple, so the head log has just rotated and is 0 bytes.
        let files = chain
            .write_ship_partitioned(
                &state,
                &retained,
                LogKind::Trace,
                2..61,
                20,
                Encoding::SizedStored,
            )
            .unwrap();
        let head = files.last().unwrap().clone();
        assert_eq!(fs::metadata(&head.log).unwrap().len(), 0);
        // And the split block log leaves a preamble-only `blocks.log`.
        chain
            .write_blocks_split(&dir.path().join("blocks"), 2..61, 20)
            .unwrap();
        let preamble_only = chain.write_blocks(&state, 61..61, FileName::Head).unwrap();

        let dirs = [state, retained, dir.path().join("blocks")];
        let found = discover(&dirs, &[], false).unwrap();
        let (c, findings) = ships(&found.trace, LogKind::Trace);
        assert_eq!(c.range(), (2, 61));
        assert_eq!(
            findings,
            [Finding::Empty {
                path: head.log.clone()
            }]
        );
        assert!(!findings[0].is_warning());
        assert!(findings[0].to_string().ends_with("is empty; skipped"));
        let (blocks, findings) = block_catalog(&found.blocks, &cache()).unwrap();
        assert_eq!(blocks.unwrap().range(), (2, 61));
        assert_eq!(
            findings,
            [Finding::Empty {
                path: preamble_only.log.clone()
            }]
        );

        // Nothing but empty files: no catalog.
        let (c, findings) =
            ship_catalog(std::slice::from_ref(&head), LogKind::Trace, &cache()).unwrap();
        assert!(c.is_none());
        assert_eq!(findings, [Finding::Empty { path: head.log }]);
        let (c, _) = block_catalog(&[preamble_only], &cache()).unwrap();
        assert!(c.is_none());
        let (c, findings) = ship_catalog(&[], LogKind::Finality, &cache()).unwrap();
        assert!(c.is_none() && findings.is_empty());
    }

    #[test]
    fn block_logs_must_share_one_chain_id() {
        let dir = TestDir::new("catalog-chain-id");
        let chain = SyntheticChain::new(2, 39);
        let ours = chain
            .write_blocks(dir.path(), 2..21, FileName::Retained)
            .unwrap();
        let foreign = Preamble::V3ChainId {
            first: 21,
            chain_id: [7; 32],
        };
        let theirs =
            kit_blocks::write(dir.path(), "blocks-21-40", &foreign, &chain.blocks()[19..]).unwrap();
        let pairs = [
            ours,
            PathPair {
                log: theirs.log,
                index: theirs.index,
            },
        ];
        for window in [None, Some((2, 21))] {
            let e = err(block_catalog_within(&pairs, &cache(), window));
            for want in [
                "blocks-21-40.log belongs to chain 0707",
                "blocks-2-20.log to chain aca376f2",
                "block logs must all come from one chain",
            ] {
                assert!(e.contains(want), "{want:?} in {e}");
            }
        }
    }

    #[test]
    fn lru_eviction_with_two_open_files_still_reads_correctly() {
        let dir = TestDir::new("catalog-lru");
        let chain = SyntheticChain::new(2, 69);
        let dirs = node_layout(&dir, &chain);
        let found = discover(&dirs, &[], false).unwrap();
        let cache = FileCache::new(2);
        let (traces, _) = ship_catalog(&found.trace, LogKind::Trace, &cache).unwrap();
        let (deltas, _) = ship_catalog(&found.chain_state, LogKind::ChainState, &cache).unwrap();
        let (blocks, _) = block_catalog(&found.blocks, &cache).unwrap();
        let (traces, deltas, blocks) = (traces.unwrap(), deltas.unwrap(), blocks.unwrap());

        let (mut raw, mut body) = (Vec::new(), Vec::new());
        // Hop between files and kinds, so nearly every read reopens an evicted file.
        for i in 0..3 * 69 {
            let n = 2 + (i * 37 % 69);
            let t = traces.get(n).unwrap();
            t.read_inline(&t.header(n).unwrap(), u64::MAX, &mut raw)
                .unwrap();
            assert_eq!(raw, chain.payload(LogKind::Trace, n), "trace {n}");
            let d = deltas.get(n).unwrap();
            d.read_inline(&d.header(n).unwrap(), u64::MAX, &mut raw)
                .unwrap();
            assert_eq!(raw, chain.payload(LogKind::ChainState, n), "deltas {n}");
            blocks.get(n).unwrap().body(n, &mut body).unwrap();
            assert_eq!(body, chain.block(n).body, "block {n}");
            assert!(cache.open_files() <= 2);
        }
    }

    #[test]
    fn large_entries_are_found_by_declared_or_inflated_size() {
        let dir = TestDir::new("catalog-large");
        let mut chain = SyntheticChain::new(2, 19);
        chain
            .set_payload_len(LogKind::Trace, 4, 5000)
            .set_payload_len(LogKind::Trace, 6, 7000)
            .set_payload_len(LogKind::Trace, 7, 9000);
        // Block n in encoding n % 4: 4 stored (Type A), 6 and 7 Type B, 10 Type B and small.
        let mut b = LogBuilder::new(Encoding::SizedStored);
        for n in 2..21 {
            let raw = chain.payload(LogKind::Trace, n);
            b.push_with(chain.id(n), &raw, Encoding::ALL[n as usize % 4]);
        }
        let (log, index) = b.write(dir.path(), "trace_history").unwrap();
        let pair = PathPair { log, index };
        let (c, _) = ships(std::slice::from_ref(&pair), LogKind::Trace);

        let large = |(b, e): (u32, u32)| {
            large_entries(&c, (b, e), 4000)
                .unwrap()
                .iter()
                .map(|f| match f {
                    Finding::LargeEntry { kind, block, bytes } => (*kind, *block, *bytes),
                    other => panic!("{other:?}"),
                })
                .collect::<Vec<_>>()
        };
        let t = LogKind::Trace;
        assert_eq!(
            large((0, u32::MAX)),
            [(t, 4, 5000), (t, 6, 7000), (t, 7, 9000)]
        );
        assert_eq!(large((5, 7)), [(t, 6, 7000)]);
        assert_eq!(large((7, 5)), []);
        let found = large_entries(&c, (4, 5), 4000).unwrap();
        assert!(found[0].is_warning());
        assert_eq!(
            found[0].to_string(),
            "the trace entry of block 4 inflates to 5000 bytes"
        );

        // A legacy entry whose stream cannot reach the threshold is not inflated: a bad
        // checksum in it goes unseen, until the threshold is low enough to need its size.
        b.corrupt_checksum(10);
        let (log, index) = b.write(&dir.path().join("bad"), "trace_history").unwrap();
        let (bad, _) = ships(&[PathPair { log, index }], LogKind::Trace);
        assert!(
            large_entries(&bad, (0, u32::MAX), 1 << 20)
                .unwrap()
                .is_empty()
        );
        let e = err(large_entries(&bad, (0, u32::MAX), 100));
        assert!(e.contains("block 10"), "{e}");
    }
}
