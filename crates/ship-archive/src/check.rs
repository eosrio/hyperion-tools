//! The offline commands: `check` validates the logs without serving them, and `index`
//! rebuilds state-history indexes the way Spring does, into a sidecar directory.
//!
//! [`check`] runs everything `serve` does before it accepts a session (discovery, every strict
//! reader, the catalogs, the served ranges, the chain id and the seam checks) and reports the
//! coverage of each log, its files and seams, the chain id and every finding. It also sizes the
//! served entries: one inflating to more than `--warn-entry-bytes` (256 MiB by default, what a
//! SHiP-aware router or proxy relays by default) is a warning, and one of 4 GiB or more is an
//! error, as no varuint32 decoder reads its length. `--deep` reads every served block instead:
//! each entry's header, trailer and whole zlib stream (adler32, size), the block log's index,
//! trailers, sizes and `previous` links, and it checks that every log agrees on every block
//! id (the block log's id of its last served block, which no next block names, is its header's
//! hash). `--hash` (which implies `--deep`) also recomputes each block's id from its header. The
//! exit code is 0 when clean, 1 on errors, and with `--strict` 2 on warnings.
//!
//! [`index`] walks each state-history log backward from its last entry through the position
//! trailers, as Spring's `check_index_on_init` does: an entry for a block at or past the end
//! (left behind by a fork) is skipped, the first entry seen from the end for a block wins, and
//! the walk stops at the log's first block. Every block must be found. When the index beside the
//! log is missing or differs (or `--out` already holds a different one), the rebuilt index is
//! checked against the log and written to `--out` as `<stem>.index`; it is never written beside
//! the log. `serve --index-dir <out>` then reads it. Block-log indexes are not rebuilt here
//! (`spring-util block-log make-index` does that).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail, ensure};
use hyperion_ship::binio::{FileCache, PosFile, num_from_id};
use hyperion_ship::blocklog::BlockLog;
use hyperion_ship::catalog::{self, Finding, PathPair, Ranged};
use hyperion_ship::protocol::{BlockPos, MAX_BLOCK_BYTES};
use hyperion_ship::shiplog::{ENTRY_OVERHEAD, HEADER_LEN, LogKind, ShipLog, unsupported_magic};
use serde_json::{Value, json};
use tracing::info;

use crate::store::{BlocksMode, PageCache, Source, Store, StoreConfig};

/// An entry inflating to this many bytes or more has a length no varuint32 decoder reads.
pub const MAX_ENTRY_BYTES: u64 = 1 << 32;
/// The default `--warn-entry-bytes`: what a SHiP-aware router or proxy relays by default.
pub const WARN_ENTRY_BYTES: u64 = 256 << 20;
/// Errors listed in a report; the rest are only counted.
pub const MAX_ERRORS: usize = 100;
/// Blocks between two progress lines of a deep check.
const PROGRESS_EVERY: u32 = 100_000;

/// What `check` checks.
#[derive(Clone, Debug)]
pub struct CheckConfig {
    pub store: StoreConfig,
    /// Read every served entry and block.
    pub deep: bool,
    /// Also recompute block ids from block headers (implies `deep`).
    pub hash: bool,
    /// Entries inflating to more bytes are warnings.
    pub warn_entry_bytes: u64,
}

/// What `check` found.
#[derive(Debug, Default)]
pub struct Report {
    /// What is served, once the store opened.
    pub coverage: Option<Coverage>,
    /// What opening the store skipped or noticed, and the large entries.
    pub findings: Vec<Finding>,
    pub warnings: Vec<String>,
    /// The first [`MAX_ERRORS`] errors.
    pub errors: Vec<String>,
    /// Errors beyond those listed.
    pub more_errors: u64,
    /// What a deep check read.
    pub deep: Option<DeepStats>,
}

/// The served logs.
#[derive(Debug)]
pub struct Coverage {
    pub chain_id: [u8; 32],
    /// Head and LIB of every status and result.
    pub last: BlockPos,
    /// trace, chain_state, finality, blocks.
    pub logs: Vec<LogCoverage>,
}

/// One log's files and what is served of them.
#[derive(Debug)]
pub struct LogCoverage {
    /// `trace`, `chain_state`, `finality` or `blocks`.
    pub log: &'static str,
    /// The blocks the files hold, `None` without files.
    pub held: Option<(u32, u32)>,
    /// `[begin, end)`, `(0, 0)` when nothing is served.
    pub served: (u32, u32),
    pub files: Vec<(PathBuf, (u32, u32))>,
    /// Where one file hands over to the next.
    pub seams: Vec<u32>,
}

/// What a deep check read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeepStats {
    /// Served blocks of the block log.
    pub blocks: u64,
    /// State-history entries, each inflated in full.
    pub entries: u64,
    pub inflated_bytes: u64,
    /// Block ids recomputed from headers by `--hash`. Without it only the last block's id is
    /// recomputed, and not counted here.
    pub hashed: u64,
}

impl Report {
    fn error(&mut self, message: String) {
        if self.errors.len() < MAX_ERRORS {
            self.errors.push(message);
        } else {
            self.more_errors += 1;
        }
    }

    pub fn error_count(&self) -> u64 {
        self.errors.len() as u64 + self.more_errors
    }

    pub fn warning_count(&self) -> usize {
        self.warnings.len() + self.findings.iter().filter(|f| f.is_warning()).count()
    }

    /// 0 when clean, 1 on errors, 2 on warnings when `strict`.
    pub fn exit_code(&self, strict: bool) -> u8 {
        if self.error_count() > 0 {
            1
        } else if strict && self.warning_count() > 0 {
            2
        } else {
            0
        }
    }

    /// The report for people.
    pub fn text(&self) -> String {
        let mut out = String::new();
        if let Some(c) = &self.coverage {
            let _ = writeln!(out, "chain id      {}", hex::encode(c.chain_id));
            let _ = writeln!(
                out,
                "head and LIB  block {} {}",
                c.last.num,
                hex::encode(c.last.id)
            );
            for log in &c.logs {
                let _ = writeln!(out);
                let Some((hb, he)) = log.held else {
                    let _ = writeln!(out, "{:<12}  no files", log.log);
                    continue;
                };
                let served = match log.served {
                    (0, 0) => "nothing".to_owned(),
                    (b, e) => format!("{b}..{e}"),
                };
                let files = match log.files.len() {
                    1 => "1 file".to_owned(),
                    n => format!("{n} files"),
                };
                let _ = writeln!(
                    out,
                    "{:<12}  serving {served} of {hb}..{he}, in {files}",
                    log.log
                );
                for (path, (b, e)) in &log.files {
                    let _ = writeln!(
                        out,
                        "{:<12}  {:<21}  {}",
                        "",
                        format!("{b}..{e}"),
                        path.display()
                    );
                }
                if !log.seams.is_empty() {
                    let seams: Vec<String> = log.seams.iter().map(u32::to_string).collect();
                    let _ = writeln!(out, "{:<12}  seams at {}", "", seams.join(", "));
                }
            }
            let _ = writeln!(out);
        }
        for f in &self.findings {
            let what = if f.is_warning() { "warning" } else { "note" };
            let _ = writeln!(out, "{what}: {f}");
        }
        for w in &self.warnings {
            let _ = writeln!(out, "warning: {w}");
        }
        for e in &self.errors {
            let _ = writeln!(out, "error: {e}");
        }
        if self.more_errors > 0 {
            let _ = writeln!(out, "error: ... and {} more errors", self.more_errors);
        }
        if let Some(d) = &self.deep {
            let _ = writeln!(
                out,
                "deep check: {} state-history entries ({} bytes inflated), {} blocks, {} block \
                 ids recomputed",
                d.entries, d.inflated_bytes, d.blocks, d.hashed
            );
        }
        let (errors, warnings) = (self.error_count(), self.warning_count());
        let _ = writeln!(
            out,
            "result: {}",
            match (errors, warnings) {
                (0, 0) => "clean".to_owned(),
                (0, w) => format!("{w} warning{}", plural(w as u64)),
                (e, w) => format!("{e} error{}, {w} warning{}", plural(e), plural(w as u64)),
            }
        );
        out
    }

    /// The report for programs.
    pub fn json(&self) -> Value {
        let range = |(b, e): (u32, u32)| json!({ "begin": b, "end": e });
        let coverage = self.coverage.as_ref().map(|c| {
            let logs: serde_json::Map<String, Value> = c
                .logs
                .iter()
                .map(|l| {
                    let files: Vec<Value> = l
                        .files
                        .iter()
                        .map(|(p, r)| json!({ "path": p.display().to_string(), "blocks": range(*r) }))
                        .collect();
                    let v = json!({
                        "held": l.held.map(range),
                        "served": range(l.served),
                        "files": files,
                        "seams": l.seams,
                    });
                    (l.log.to_owned(), v)
                })
                .collect();
            json!({
                "chain_id": hex::encode(c.chain_id),
                "last": { "num": c.last.num, "id": hex::encode(c.last.id) },
                "logs": logs,
            })
        });
        let findings: Vec<Value> = self
            .findings
            .iter()
            .map(crate::http::finding_json)
            .collect();
        json!({
            "ok": self.error_count() == 0,
            "coverage": coverage,
            "findings": findings,
            "warnings": self.warnings,
            "errors": self.errors,
            "more_errors": self.more_errors,
            "deep": self.deep.map(|d| json!({
                "entries": d.entries,
                "inflated_bytes": d.inflated_bytes,
                "blocks": d.blocks,
                "hashed": d.hashed,
            })),
        })
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Open the store as `serve` would and check it; never fails, the errors are in the report.
pub fn check(cfg: &CheckConfig) -> Report {
    let mut report = Report::default();
    let (store, findings) = match Store::open(&cfg.store) {
        Ok(opened) => opened,
        Err(e) => {
            report.error(format!("{e:#}"));
            return report;
        }
    };
    report.findings = findings;
    report.coverage = Some(coverage(&store));
    report.warnings = store.uncovered(&cfg.store);
    if cfg.hash && cfg.store.blocks == BlocksMode::None {
        report
            .warnings
            .push("--hash with --blocks none: there is no block header to hash".into());
    }
    if cfg.deep || cfg.hash {
        deep(&store, cfg, &mut report);
    } else {
        sizes(&store, cfg, &mut report);
    }
    report
}

fn coverage(store: &Store) -> Coverage {
    fn of<L: Ranged>(
        log: &'static str,
        catalog: Option<&catalog::Catalog<L>>,
        served: (u32, u32),
    ) -> LogCoverage {
        LogCoverage {
            log,
            held: catalog.map(|c| c.range()),
            served,
            files: catalog.map_or_else(Vec::new, |c| {
                c.files()
                    .iter()
                    .map(|f| (f.log_path().to_path_buf(), f.range()))
                    .collect()
            }),
            seams: catalog.map_or_else(Vec::new, |c| c.seams().collect()),
        }
    }
    let mut logs: Vec<LogCoverage> = LogKind::ALL
        .into_iter()
        .map(|k| of(k.label(), store.catalog(k), store.served.ship(k)))
        .collect();
    logs.push(of("blocks", store.blocks.as_ref(), store.served.blocks));
    Coverage {
        chain_id: store.chain_id,
        last: store.last,
        logs,
    }
}

/// The entries too large for some clients, from their headers (Type A) or a counting pass
/// (Type B entries whose stream could reach the threshold).
fn sizes(store: &Store, cfg: &CheckConfig, report: &mut Report) {
    let threshold = cfg.warn_entry_bytes.min(MAX_ENTRY_BYTES - 1);
    for kind in LogKind::ALL {
        let (Some(catalog), served) = (store.catalog(kind), store.served.ship(kind)) else {
            continue;
        };
        if served.0 == served.1 {
            continue;
        }
        match catalog::large_entries(catalog, served, threshold) {
            Ok(found) => {
                for f in found {
                    large_entry(f, cfg, report);
                }
            }
            Err(e) => report.error(format!("{e:#}")),
        }
    }
}

/// Report a [`Finding::LargeEntry`]: an error from 4 GiB, else a warning above the threshold.
fn large_entry(f: Finding, cfg: &CheckConfig, report: &mut Report) {
    if let Finding::LargeEntry { kind, block, bytes } = f {
        if bytes >= MAX_ENTRY_BYTES {
            report.error(format!(
                "the {} entry of block {block} inflates to {bytes} bytes: from 4 GiB no \
                 varuint32 decoder reads its length; exclude it with --range",
                kind.label()
            ));
        } else if bytes > cfg.warn_entry_bytes {
            report.findings.push(f);
        }
    }
}

/// What one source gives as a block's id: the source, and the id.
type View = (String, [u8; 32]);

/// Read every served block, and check that every log agrees on every block id.
fn deep(store: &Store, cfg: &CheckConfig, report: &mut Report) {
    let served: Vec<(u32, u32)> = LogKind::ALL
        .iter()
        .map(|&k| store.served.ship(k))
        .chain([store.served.blocks])
        .filter(|(b, e)| b < e)
        .collect();
    let begin = served.iter().map(|r| r.0).min().unwrap_or(0);
    let end = served.iter().map(|r| r.1).max().unwrap_or(0);
    let pages = PageCache::default();
    let mut stats = DeepStats::default();
    // The ids each log gives the block before the current one, still open to the block log's
    // `previous` of the current block.
    let mut prior: Option<(u32, Vec<View>)> = None;
    for n in begin..end {
        let mut views = Vec::new();
        for kind in LogKind::ALL {
            let Some(log) = store.ship_log(kind, n) else {
                continue;
            };
            let entry = pages
                .header(kind, log, n)
                .and_then(|h| Ok((h.id, log.verify_count(&h, u64::MAX)?)));
            match entry {
                Ok((id, bytes)) => {
                    stats.entries += 1;
                    stats.inflated_bytes += bytes;
                    views.push((format!("the {} entry", kind.label()), id));
                    let f = Finding::LargeEntry {
                        kind,
                        block: n,
                        bytes,
                    };
                    large_entry(f, cfg, report);
                }
                Err(e) => report.error(format!("{:#}", e.context(Source::Ship(kind)))),
            }
        }
        if let Some(log) = store.block_log(n) {
            stats.blocks += 1;
            // Where no served block follows to name this one's id, the block log's id for it
            // is the hash of its header, as at startup.
            let last = n
                .checked_add(1)
                .and_then(|next| store.block_log(next))
                .is_none();
            match deep_block(log, n, cfg.hash || last) {
                Ok((previous, id)) => {
                    if let Some((m, v)) = &mut prior
                        && *m + 1 == n
                    {
                        v.push((format!("block {n}'s `previous`"), previous));
                    }
                    if let Some(id) = id {
                        if cfg.hash {
                            stats.hashed += 1;
                        }
                        views.push(("the hash of its block header".to_owned(), id));
                    }
                }
                Err(e) => report.error(format!("{:#}", e.context(Source::Blocks))),
            }
        }
        if let Some((m, v)) = prior.replace((n, views)) {
            agree(m, &v, report);
        }
        let done = n - begin + 1;
        if done % PROGRESS_EVERY == 0 {
            info!("checked {done} of {} blocks", end - begin);
        }
    }
    if let Some((m, v)) = prior {
        agree(m, &v, report);
    }
    report.deep = Some(stats);
}

/// Block `n`'s `previous` and, with `hash`, its id recomputed from its header. Reading either
/// checks the index slot, the trailer and that `previous` names block `n - 1`.
fn deep_block(log: &BlockLog, n: u32, hash: bool) -> Result<([u8; 32], Option<[u8; 32]>)> {
    let (_, len) = log.bounds(n)?;
    ensure!(
        len <= MAX_BLOCK_BYTES as u64,
        "{}: block {n} is {len} bytes, more than the {MAX_BLOCK_BYTES} a SHiP result carries: \
         a request for it with fetch_block drops the connection",
        log.log_path().display()
    );
    if hash {
        let h = log.header(n)?;
        Ok((h.previous, Some(h.id)))
    } else {
        Ok((log.previous(n)?, None))
    }
}

/// Every view of block `n`'s id must be the same id.
fn agree(n: u32, views: &[View], report: &mut Report) {
    let Some(((first, id), rest)) = views.split_first() else {
        return;
    };
    for (what, other) in rest {
        if other != id {
            report.error(format!(
                "block ids disagree at block {n}: {first} has {}, {what} has {}",
                hex::encode(id),
                hex::encode(other)
            ));
        }
    }
}

/// Where `index` looks and writes.
#[derive(Clone, Debug)]
pub struct IndexConfig {
    pub dirs: Vec<PathBuf>,
    pub recursive: bool,
    /// The sidecar directory the indexes go to.
    pub out: PathBuf,
}

/// The index beside a log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Beside {
    /// The log is empty: no index is read for it.
    NotNeeded,
    Missing,
    /// It differs from the rebuilt index.
    Wrong,
    Matches,
}

/// What `index` did with one state-history log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Indexed {
    pub log: PathBuf,
    /// The blocks the log holds, `None` for an empty log.
    pub blocks: Option<(u32, u32)>,
    pub beside: Beside,
    /// The sidecar written.
    pub written: Option<PathBuf>,
}

impl Indexed {
    /// One line for people.
    pub fn line(&self) -> String {
        let log = self.log.display();
        let Some((b, e)) = self.blocks else {
            return format!("{log}: empty; no index needed");
        };
        let beside = match self.beside {
            Beside::Missing => "has no index beside it",
            Beside::Wrong => "has a wrong index beside it",
            _ => "has the right index beside it",
        };
        match &self.written {
            Some(path) => format!("{log} (blocks {b}..{e}) {beside}: wrote {}", path.display()),
            None => format!("{log} (blocks {b}..{e}) {beside}: nothing written"),
        }
    }
}

/// Rebuild the index of every state-history log in `cfg.dirs` and write the ones needed to
/// `cfg.out`, which is created and must not hold any of the logs.
pub fn index(cfg: &IndexConfig) -> Result<Vec<Indexed>> {
    ensure!(!cfg.dirs.is_empty(), "no log directory given (--dir)");
    fs::create_dir_all(&cfg.out).with_context(|| format!("create {}", cfg.out.display()))?;
    // Resolved only to compare it with the log directories; the indexes go to `--out` as given.
    let resolved =
        fs::canonicalize(&cfg.out).with_context(|| format!("resolve {}", cfg.out.display()))?;
    let found = catalog::discover(&cfg.dirs, &[], cfg.recursive)?;
    let mut names: HashMap<OsString, PathBuf> = HashMap::new();
    let mut pairs = Vec::new();
    for kind in LogKind::ALL {
        for pair in found.ship(kind) {
            let dir = pair.log.parent().map(fs::canonicalize).transpose()?;
            ensure!(
                dir.as_deref() != Some(resolved.as_path()),
                "--out {} holds {}: an index is never written beside its log; choose another \
                 directory",
                cfg.out.display(),
                pair.log.display()
            );
            let name = pair
                .index
                .file_name()
                .context("an index path has a file name")?
                .to_os_string();
            if let Some(other) = names.insert(name.clone(), pair.log.clone()) {
                bail!(
                    "{} and {} would share the sidecar index {}: index them into different \
                     --out directories",
                    other.display(),
                    pair.log.display(),
                    name.to_string_lossy()
                );
            }
            pairs.push((kind, pair));
        }
    }
    pairs
        .into_iter()
        .map(|(kind, pair)| index_one(kind, pair, &cfg.out))
        .collect()
}

fn index_one(kind: LogKind, pair: &PathPair, out: &Path) -> Result<Indexed> {
    let Some((begin, slots)) = rebuild_index(&pair.log)? else {
        return Ok(Indexed {
            log: pair.log.clone(),
            blocks: None,
            beside: Beside::NotNeeded,
            written: None,
        });
    };
    let end = begin + slots.len() as u32;
    let beside = match same_index(&pair.index, &slots)? {
        None => Beside::Missing,
        Some(false) => Beside::Wrong,
        Some(true) => Beside::Matches,
    };
    let target = out.join(pair.index.file_name().context("an index has a file name")?);
    // A different sidecar would be read instead of the right index beside the log.
    let stale = same_index(&target, &slots)? == Some(false);
    let written = if beside != Beside::Matches || stale {
        write_index(kind, &pair.log, &target, &slots)?;
        Some(target)
    } else {
        None
    };
    Ok(Indexed {
        log: pair.log.clone(),
        blocks: Some((begin, end)),
        beside,
        written,
    })
}

/// A slot not filled yet.
const UNSET: u64 = u64::MAX;

/// `log`'s index as Spring rebuilds it: its first block and one entry offset per block. `None`
/// for an empty log. Reads the log only; fails on a corrupted tail, trailers that do not chain,
/// an unsupported entry, or a block with no entry.
pub fn rebuild_index(log: &Path) -> Result<Option<(u32, Vec<u64>)>> {
    let f = PosFile::open(log)?;
    let size = f.len();
    if size == 0 {
        return Ok(None);
    }
    let name = log.display();
    ensure!(
        size >= ENTRY_OVERHEAD,
        "{name}: corrupted tail; repair the log first: {size} bytes cannot hold an entry"
    );
    let (first, _) = entry_at(&f, 0).with_context(|| format!("{name}: first entry"))?;
    let begin = num_from_id(&first);
    let last_pos = f.u64_at(size - 8)?;
    let (last, last_end) = entry_at(&f, last_pos)
        .with_context(|| format!("{name}: corrupted tail; repair the log first"))?;
    ensure!(
        last_end == size,
        "{name}: corrupted tail; repair the log first: the entry at offset {last_pos} ends at \
         byte {last_end}, not at the end of the file ({size})"
    );
    let last = num_from_id(&last);
    ensure!(
        begin <= last && last < u32::MAX,
        "{name}: corrupted tail; repair the log first: the last entry is block {last}, the first \
         block {begin}"
    );
    let end = last + 1;
    let mut slots = vec![UNSET; (end - begin) as usize];
    // One past the last byte of the entry read next: walking backward, each entry's trailer
    // gives where it starts.
    let mut entry_end = size;
    loop {
        let pos = f.u64_at(entry_end - 8)?;
        let (id, end_of_entry) = entry_at(&f, pos).with_context(|| {
            format!(
                "{name}: the trailer at offset {} names offset {pos}",
                entry_end - 8
            )
        })?;
        ensure!(
            end_of_entry == entry_end,
            "{name}: the entry at offset {pos} ends at byte {end_of_entry}, but its trailer is at \
             {}: the trailers do not chain; repair the log first",
            entry_end - 8
        );
        let n = num_from_id(&id);
        // A block at or past the end was left by a fork; the first entry from the end wins.
        if n < end {
            ensure!(
                n >= begin,
                "{name}: the entry at offset {pos} is block {n}, before the first block {begin}"
            );
            let slot = &mut slots[(n - begin) as usize];
            if *slot == UNSET {
                *slot = pos;
            }
        }
        if n == begin {
            break;
        }
        // The entry at offset 0 is block `begin`, so `pos` is past it here.
        entry_end = pos;
    }
    if let Some(missing) = slots.iter().position(|&s| s == UNSET) {
        bail!(
            "{name}: block {} has no entry between the end of the log and block {begin}: the log \
             has a hole and cannot be indexed",
            begin + missing as u32
        );
    }
    Ok(Some((begin, slots)))
}

/// The id of the entry at `pos` and the offset one past its trailer, which must lie in the log.
fn entry_at(f: &PosFile, pos: u64) -> Result<([u8; 32], u64)> {
    let size = f.len();
    ensure!(
        pos.checked_add(ENTRY_OVERHEAD).is_some_and(|e| e <= size),
        "offset {pos} leaves no room for an entry in the {size}-byte log"
    );
    let mut head = [0; HEADER_LEN as usize];
    f.read_exact_at(pos, &mut head)?;
    let magic = u64::from_le_bytes(head[..8].try_into()?);
    if let Some(why) = unsupported_magic(magic) {
        bail!("the entry at offset {pos}: {why}");
    }
    let payload = u64::from_le_bytes(head[40..48].try_into()?);
    let end = (pos + ENTRY_OVERHEAD)
        .checked_add(payload)
        .filter(|&end| end <= size)
        .with_context(|| {
            format!(
                "the entry at offset {pos} has {payload} payload bytes, past the end of the log"
            )
        })?;
    Ok((head[8..40].try_into()?, end))
}

/// Whether the index file at `path` holds exactly `slots`; `None` when there is none.
fn same_index(path: &Path, slots: &[u64]) -> Result<Option<bool>> {
    let f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("open {}", path.display())),
    };
    let len = f.metadata()?.len();
    if len != slots.len() as u64 * 8 {
        return Ok(Some(false));
    }
    let mut r = BufReader::with_capacity(1 << 20, f);
    let mut slot = [0; 8];
    for &want in slots {
        r.read_exact(&mut slot)
            .with_context(|| format!("read {}", path.display()))?;
        if u64::from_le_bytes(slot) != want {
            return Ok(Some(false));
        }
    }
    Ok(Some(true))
}

/// Write `slots` as `target`: into a temporary file beside it first, which must then open with
/// `log` under the strict reader and lead to a valid header for every block, then renamed.
fn write_index(kind: LogKind, log: &Path, target: &Path, slots: &[u64]) -> Result<()> {
    let name = target
        .file_name()
        .context("an index has a file name")?
        .to_string_lossy();
    let tmp = target.with_file_name(format!(".{name}.tmp"));
    let written = (|| -> Result<()> {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        for slot in slots {
            w.write_all(&slot.to_le_bytes())?;
        }
        w.into_inner().map_err(|e| e.into_error())?.sync_all()?;
        verify_index(kind, log, &tmp)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(e.context(format!("write {}", target.display())));
    }
    fs::rename(&tmp, target)
        .with_context(|| format!("rename {} to {}", tmp.display(), target.display()))
}

/// Open `log` with `index` and read every block's entry header through it.
fn verify_index(kind: LogKind, log: &Path, index: &Path) -> Result<()> {
    // A cache of its own, dropped (closing both files) before the index is renamed.
    let cache = FileCache::new(2);
    let opened = ShipLog::open(log, index, kind, &cache)?
        .with_context(|| format!("{} is empty", log.display()))?;
    let log = Arc::new(opened);
    let (begin, end) = log.range();
    let pages = PageCache::default();
    let mut n = begin;
    while n < end {
        pages.header(kind, &log, n)?;
        n += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyperion_ship::testkit::ship::{Encoding, LogBuilder, synthetic_id, synthetic_payload};

    fn tmp() -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix("ship-archive-check-")
            .tempdir()
            .unwrap()
    }

    #[test]
    fn the_rebuilt_index_skips_fork_leftovers_as_spring_does() {
        let dir = tmp();
        // 2 3 4 5* 6* 7* 5 6 7 8: blocks 5 to 7 were replaced by a fork.
        let mut log = LogBuilder::new(Encoding::SizedStored);
        for n in 2..8 {
            let fork = if n >= 5 { 9 } else { 0 };
            log.push(synthetic_id(n, fork), &synthetic_payload(n, 40));
        }
        for n in 5..9 {
            log.push(synthetic_id(n, 0), &synthetic_payload(n, 50));
        }
        let (path, _) = log.write(dir.path(), "trace_history").unwrap();
        let (begin, slots) = rebuild_index(&path).unwrap().unwrap();
        assert_eq!(begin, 2);
        let want: Vec<u64> = (2..9).map(|n| log.position(n)).collect();
        assert_eq!(slots, want);

        // A tail fork shorter than what it replaced: 2..=7, then 4' 5'; the end is 6.
        let mut log = LogBuilder::new(Encoding::SizedStored);
        log.push_blocks(2..8, 30);
        for n in 4..6 {
            log.push(synthetic_id(n, 2), &synthetic_payload(n, 30));
        }
        let (path, _) = log.write(dir.path(), "chain_state_history").unwrap();
        let (begin, slots) = rebuild_index(&path).unwrap().unwrap();
        let want: Vec<u64> = (2..6).map(|n| log.position(n)).collect();
        assert_eq!((begin, slots), (2, want));
    }

    #[test]
    fn a_log_that_cannot_be_walked_is_refused() {
        let dir = tmp();
        let mut log = LogBuilder::new(Encoding::SizedStored);
        log.push_blocks(2..10, 30);
        // A trailer pointing elsewhere breaks the chain.
        let mut broken = log.clone();
        let at = broken.position(6) as usize;
        let size = u64::from_le_bytes(broken.log()[at + 40..at + 48].try_into().unwrap());
        let trailer = at + 48 + size as usize;
        broken.log_mut()[trailer..trailer + 8].copy_from_slice(&3u64.to_le_bytes());
        let (path, _) = broken.write(dir.path(), "a").unwrap();
        let err = format!("{:#}", rebuild_index(&path).unwrap_err());
        assert!(err.contains("trailer"), "{err}");

        // A cut tail.
        let mut cut = log.clone();
        let len = cut.log().len();
        cut.log_mut().truncate(len - 3);
        let (path, _) = cut.write(dir.path(), "b").unwrap();
        let err = format!("{:#}", rebuild_index(&path).unwrap_err());
        assert!(err.contains("corrupted tail"), "{err}");

        // An empty log needs no index.
        std::fs::write(dir.path().join("c.log"), b"").unwrap();
        assert_eq!(rebuild_index(&dir.path().join("c.log")).unwrap(), None);
    }

    #[test]
    fn exit_codes_follow_errors_then_warnings() {
        let mut r = Report::default();
        assert_eq!((r.exit_code(false), r.exit_code(true)), (0, 0));
        r.findings.push(Finding::Empty {
            path: PathBuf::from("x"),
        });
        assert_eq!(r.exit_code(true), 0, "a note is not a warning");
        r.warnings.push("w".into());
        assert_eq!((r.exit_code(false), r.exit_code(true)), (0, 2));
        for i in 0..MAX_ERRORS + 5 {
            r.error(format!("e{i}"));
        }
        assert_eq!((r.errors.len(), r.more_errors), (MAX_ERRORS, 5));
        assert_eq!((r.exit_code(false), r.exit_code(true)), (1, 1));
        assert!(r.text().contains("and 5 more errors"));
        assert_eq!(r.json()["ok"], false);
    }
}
