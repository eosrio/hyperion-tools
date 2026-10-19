//! Sources, reconstruction, review bundles, and the offline apply.
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::codec::Codec;
use crate::format::{
    Blocks, Kind, Log, block_header, container_disk, encode_ship_with, orphans, ship_marker,
    ship_payload,
};
use crate::io::{Disk, MAX_BYTES, Stamp, create_dir, create_file, hash, media_error, sync_dir};
use crate::report::{Out, arg, bytes, duration};
use crate::target::{
    Extent, Located, ORPHANS_UNREPAIRABLE, Region, Target, analyze, describe, first_unreadable,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// At least one record is unreadable; every readable record already matches.
    Unreadable,
    /// Fully readable and byte-identical to the replacement.
    Identical,
    /// A readable record differs from the replacement. Never overwritten.
    Differs,
}

pub fn extent_state(disk: &Disk, record_size: u64, e: &Extent, expected: &[u8]) -> Result<State> {
    match disk.read(e.offset, e.len) {
        Ok(have) => {
            return Ok(if have == expected {
                State::Identical
            } else {
                State::Differs
            });
        }
        Err(err) if media_error(&err) => {}
        Err(err) => {
            return Err(err).context("read damaged extent (only EIO/ENODATA authorize a repair)");
        }
    }
    let mut unreadable = false;
    let mut pos = e.offset;
    while pos < e.end() {
        let at = (pos - e.offset) as usize;
        match disk.read(pos, record_size) {
            Ok(have) if have == expected[at..at + record_size as usize] => {}
            Ok(_) => return Ok(State::Differs),
            Err(err) if media_error(&err) => unreadable = true,
            Err(err) => return Err(err).context("read damaged record"),
        }
        pos += record_size;
    }
    Ok(if unreadable {
        State::Unreadable
    } else {
        State::Identical
    })
}

/// The replacement bytes for each extent, in plan order.
fn pieces<'a>(t: &'a Target, replacement: &'a [u8]) -> Vec<(&'a Extent, &'a [u8])> {
    let mut at = 0;
    t.extents()
        .map(|e| {
            let piece = &replacement[at..at + e.len as usize];
            at += e.len as usize;
            (e, piece)
        })
        .collect()
}

fn records(t_record_size: u64, e: &Extent) -> [u64; 2] {
    [e.offset / t_record_size, e.end() / t_record_size - 1]
}

fn record_text(record_size: u64, e: &Extent) -> String {
    let [a, b] = records(record_size, e);
    if a == b {
        format!("{a}")
    } else {
        format!("{a}-{b}")
    }
}

/// Anchor ID without reading a SHIP payload: a target's first entry may be a full-state
/// delta larger than any entry read.
fn anchor_id(log: &Log, n: u32) -> Result<[u8; 32]> {
    if log.kind == Kind::Blocks {
        log.id(n, &log.body(n)?)
    } else {
        log.ship_entry_id(n)
    }
}

fn region_json(t_record_size: u64, r: &Region) -> Value {
    json!({
        "first_block": r.first_block,
        "last_block": r.last_block,
        "source_block_range": r.source_range(),
        "span": [r.span_start, r.span_end],
        "extents": r.extents.iter().map(|e| json!({
            "offset": e.offset,
            "len": e.len,
            "records": records(t_record_size, e),
        })).collect::<Vec<_>>(),
    })
}

fn region_line(record_size: u64, r: &Region) -> String {
    let damaged: u64 = r.extents.iter().map(|e| e.len).sum();
    let recs: Vec<String> = r
        .extents
        .iter()
        .map(|e| record_text(record_size, e))
        .collect();
    format!(
        "blocks {}..{}: {} in {} extent(s), record(s) {}",
        r.first_block,
        r.last_block,
        bytes(damaged),
        r.extents.len(),
        recs.join(", ")
    )
}

/// Selection flags that name the found damage exactly, so later steps need no rescan.
pub fn target_flags(log: &Log, record_size: u64, extents: &[Extent]) -> String {
    let mut s = format!(
        "--log {} --record-size {record_size}",
        arg(log.log.path.to_string_lossy())
    );
    for e in extents {
        write!(s, " --record-index {}", record_text(record_size, e)).unwrap();
    }
    s
}

/// `scan`: where the damage is, whether each region can be repaired, and what to run next.
pub fn scan_report(log: &Log, located: &Located) -> Result<Out> {
    let rs = located.record_size;
    let mut text = String::new();
    match &located.scan {
        Some(s) => writeln!(
            text,
            "Scanned {} ({}.log): {} in {}, record size {}, {} failed read(s).",
            log.log.path.display(),
            log.kind.stem(),
            bytes(s.to.min(log.size) - s.from),
            duration(s.seconds),
            bytes(rs),
            s.failed_reads
        )?,
        None => writeln!(
            text,
            "Checked {} explicit record(s) of {} ({}.log), record size {}.",
            located.extents.iter().map(|e| e.len / rs).sum::<u64>(),
            log.log.path.display(),
            log.kind.stem(),
            bytes(rs)
        )?,
    }
    let base = json!({
        "ok": true,
        "kind": log.kind,
        "log": log.log.path,
        "index": log.index.path,
        "log_blocks": [log.first, log.last],
        "record_size": rs,
        "scan": located.scan,
    });
    if located.extents.is_empty() {
        text.push_str("No unreadable data found. Nothing to repair.\n");
        let mut json = base;
        json["damaged"] = json!(false);
        json["regions"] = json!([]);
        return Ok(Out { json, text });
    }
    let findings = analyze(log, rs, &located.extents)?;
    let mut regions = Vec::new();
    let mut repairable = Vec::new();
    let total: u64 = located.extents.iter().map(|e| e.len).sum();
    writeln!(
        text,
        "Found {} damaged area(s), {} in {} extent(s):",
        findings.len(),
        bytes(total),
        located.extents.len()
    )?;
    for (i, f) in findings.iter().enumerate() {
        let mut v = match &f.region {
            Some(r) => region_json(rs, r),
            None => json!({"extents": f.extents}),
        };
        // Explicit records may be readable (for example on a copy under review).
        if located.scan.is_none() {
            let readable: Vec<bool> = f
                .extents
                .iter()
                .map(|e| Ok(first_unreadable(&log.log, e.offset, e.end())?.is_none()))
                .collect::<Result<_>>()?;
            v["extents_readable"] = json!(readable);
        }
        v["repairable"] = json!(f.problem.is_none());
        v["problem"] = json!(f.problem);
        let line = match &f.region {
            Some(r) => region_line(rs, r),
            None => describe(&f.extents),
        };
        match (&f.problem, &f.region) {
            (None, Some(r)) => {
                let (a, b) = r.source_range();
                v["anchor_before_id"] = json!(hex::encode(anchor_id(log, a)?));
                v["anchor_after_id"] = json!(hex::encode(anchor_id(log, b)?));
                writeln!(text, "  [{}] {line}: repairable", i + 1)?;
                repairable.push(r.clone());
            }
            (Some(p), _) => writeln!(text, "  [{}] {line}: NOT repairable: {p}", i + 1)?,
            (None, None) => unreachable!("findings without a region always carry a problem"),
        }
        regions.push(v);
    }
    let mut json = base;
    json["damaged"] = json!(true);
    json["regions"] = json!(regions);
    if repairable.is_empty() {
        text.push_str("\nNo region can be repaired by this tool.\n");
        return Ok(Out { json, text });
    }
    let extents: Vec<Extent> = repairable.iter().flat_map(|r| r.extents.clone()).collect();
    let flags = target_flags(log, rs, &extents);
    let first = repairable
        .iter()
        .map(|r| r.first_block)
        .min()
        .unwrap_or_default();
    let next = format!("antelope-log-repair prepare {flags} --ship ws://HOST:PORT --out DIR");
    let alternative = if log.kind == Kind::Blocks {
        "Instead of --ship, --blocks-log PATH uses an intact blocks.log from another node or a \
         backup."
            .to_string()
    } else {
        json["snapshot_latest_head_block"] = json!(first - 2);
        format!(
            "Instead of --ship, regenerate history with --nodeos, --snapshot, --snapshot-block \
             (at most {}), and --blocks-dir.",
            first - 2
        )
    };
    writeln!(
        text,
        "\nNext (nothing is written, but stop nodeos first: the plan pins the log's size and \
         modification time, which a running node changes with every block):\n  {next}\n{alternative}"
    )?;
    json["next_command"] = json!(next);
    Ok(Out { json, text })
}

/// A log file used as a source, with its index (default: alongside, `.index`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LogFile {
    pub log: PathBuf,
    pub index: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sources {
    /// P2PBLK01 containers.
    #[serde(default)]
    pub blocks: Vec<PathBuf>,
    /// Intact blocks.log copies (another node, a backup). Together with `blocks` they must
    /// cover every region's source range.
    #[serde(default)]
    pub block_logs: Vec<LogFile>,
    /// SHIP donor logs of the target's kind; one must cover each region's source range.
    #[serde(default)]
    pub donors: Vec<LogFile>,
}

pub struct Loaded {
    containers: Blocks,
    block_logs: Vec<Log>,
    pub donors: Vec<Log>,
}

impl Loaded {
    /// Packed block `n` from every source that holds it; all of them must agree.
    pub fn block(&self, n: u32) -> Result<Option<Vec<u8>>> {
        let mut found = self.containers.get(&n).cloned();
        for log in self
            .block_logs
            .iter()
            .filter(|l| (l.first..=l.last).contains(&n))
        {
            let body = log
                .body(n)
                .with_context(|| format!("read block {n} from {}", log.log.path.display()))?;
            match &found {
                Some(seen) => ensure!(
                    seen == &body,
                    "block sources disagree on block {n}; refusing all of them"
                ),
                None => found = Some(body),
            }
        }
        Ok(found)
    }
}

impl Sources {
    pub fn canonicalize(&self) -> Result<Self> {
        let canon = |p: &PathBuf| {
            p.canonicalize()
                .with_context(|| format!("resolve source {}", p.display()))
        };
        let files = |list: &[LogFile]| {
            list.iter()
                .map(|f| {
                    Ok(LogFile {
                        log: canon(&f.log)?,
                        index: f.index.as_ref().map(canon).transpose()?,
                    })
                })
                .collect::<Result<Vec<_>>>()
        };
        Ok(Self {
            blocks: self.blocks.iter().map(canon).collect::<Result<_>>()?,
            block_logs: files(&self.block_logs)?,
            donors: files(&self.donors)?,
        })
    }

    /// Sources recorded by `fetch-ship` or `replay-ship` in DIR/source.json.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let disk = Disk::open(&dir.join("source.json"), false)
            .with_context(|| format!("{} is not a fetch-ship/replay-ship output", dir.display()))?;
        let size = disk.stamp()?.size;
        ensure!(size <= 1 << 20, "source.json exceeds 1 MiB");
        let report: Value = serde_json::from_slice(&disk.read(0, size)?)?;
        ensure!(
            report["target_compatible"] == true,
            "{} holds sources that do not fit the target; see its source.json",
            dir.display()
        );
        let relative: Sources = serde_json::from_value(report["sources"].clone())
            .context("source.json has no valid sources")?;
        relative.under(dir).canonicalize()
    }

    /// Resolve paths recorded relative to `dir`.
    pub fn under(&self, dir: &Path) -> Self {
        let files = |list: &[LogFile]| {
            list.iter()
                .map(|f| LogFile {
                    log: dir.join(&f.log),
                    index: f.index.as_ref().map(|p| dir.join(p)),
                })
                .collect()
        };
        Self {
            blocks: self.blocks.iter().map(|p| dir.join(p)).collect(),
            block_logs: files(&self.block_logs),
            donors: files(&self.donors),
        }
    }

    pub fn extend(&mut self, other: Sources) {
        self.blocks.extend(other.blocks);
        self.block_logs.extend(other.block_logs);
        self.donors.extend(other.donors);
    }

    pub fn load(&self, kind: Kind) -> Result<Loaded> {
        self.load_with(kind, |path| Disk::open(path, false))
    }

    pub fn load_with(
        &self,
        kind: Kind,
        mut open_disk: impl FnMut(&Path) -> Result<Disk>,
    ) -> Result<Loaded> {
        ensure!(
            !self.blocks.is_empty() || !self.block_logs.is_empty(),
            "no block sources; pass --sources DIR, --blocks FILE, or --blocks-log FILE"
        );
        let mut containers = Blocks::new();
        let mut total = 0u64;
        for path in &self.blocks {
            for (n, body) in
                container_disk(&open_disk(path)?).with_context(|| format!("{}", path.display()))?
            {
                total += body.len() as u64;
                ensure!(total <= 4 * MAX_BYTES, "block sources exceed 1 GiB");
                match containers.get(&n) {
                    Some(seen) => ensure!(
                        seen == &body,
                        "block sources disagree on block {n}; refusing all of them"
                    ),
                    None => {
                        containers.insert(n, body);
                    }
                }
            }
        }
        let mut open = |list: &[LogFile]| {
            list.iter()
                .map(|f| {
                    Log::from_disks(
                        Kind::from_path(&f.log)?,
                        open_disk(&f.log)?,
                        open_disk(
                            &f.index
                                .clone()
                                .unwrap_or_else(|| f.log.with_extension("index")),
                        )?,
                    )
                })
                .collect::<Result<Vec<_>>>()
        };
        let block_logs = open(&self.block_logs)?;
        for l in &block_logs {
            ensure!(
                l.kind == Kind::Blocks,
                "{} is not a blocks.log",
                l.log.path.display()
            );
        }
        let donors = open(&self.donors)?;
        if kind == Kind::Blocks {
            ensure!(
                donors.is_empty(),
                "blocks.log repairs use block sources only, not SHIP donor logs"
            );
        } else {
            ensure!(
                !donors.is_empty(),
                "SHIP repairs need a donor log; pass --sources DIR or --donor-log FILE"
            );
            for d in &donors {
                ensure!(
                    d.kind == kind,
                    "donor {} is a {}.log but the target is a {}.log",
                    d.log.path.display(),
                    d.kind.stem(),
                    kind.stem()
                );
            }
        }
        Ok(Loaded {
            containers,
            block_logs,
            donors,
        })
    }
}

pub struct Rebuilt {
    /// Replacement bytes for every extent, concatenated in plan order.
    pub replacement: Vec<u8>,
    pub evidence_sha256: String,
    /// How each region's entries were produced from the sources.
    pub encodings: Vec<String>,
}

/// Reconstruct every region's entries at TARGET offsets, rebasing source trailers.
pub fn rebuild(log: &Log, t: &Target, src: &Loaded) -> Result<Rebuilt> {
    for b in src.block_logs.iter().chain(&src.donors) {
        ensure!(
            !same_file::is_same_file(&b.log.path, &log.log.path)?,
            "{} is the damaged target itself, not an independent source",
            b.log.path.display()
        );
    }
    let mut digest = Sha256::new();
    // All trusted bytes consulted are pinned in the plan, including framing and index window.
    digest.update(serde_json::to_vec(t)?);
    digest.update(
        log.log
            .read(0, if log.kind == Kind::Blocks { 8 } else { 48 })?,
    );
    let mut replacement = Vec::with_capacity(t.damaged_bytes() as usize);
    let mut encodings = Vec::new();
    for r in &t.regions {
        encodings.push(
            rebuild_region(log, r, src, &mut digest, &mut replacement)
                .with_context(|| format!("region blocks {}..{}", r.first_block, r.last_block))?,
        );
    }
    Ok(Rebuilt {
        replacement,
        evidence_sha256: hex::encode(digest.finalize()),
        encodings,
    })
}

fn rebuild_region(
    log: &Log,
    r: &Region,
    src: &Loaded,
    digest: &mut Sha256,
    out: &mut Vec<u8>,
) -> Result<String> {
    let (left, right) = r.source_range();
    let after = if log.kind == Kind::Blocks {
        Some(log.body(right)?)
    } else {
        None
    };
    let left_id = anchor_id(log, left)?;
    let right_id = anchor_id(log, right)?;
    digest.update(left_id);
    digest.update(right_id);
    let donor = if log.kind == Kind::Blocks {
        None
    } else {
        let d = src
            .donors
            .iter()
            .find(|d| d.first <= left && right <= d.last)
            .with_context(|| format!("no donor log covers blocks {left}..{right}"))?;
        for (n, id) in [(left, left_id), (right, right_id)] {
            let body = src
                .block(n)?
                .with_context(|| format!("block sources lack SHIP anchor {n}"))?;
            ensure!(
                block_header(&body)?.id == id,
                "SHIP anchor {n} belongs to another chain or fork"
            );
            ensure!(
                d.ship_entry_id(n)? == id,
                "donor anchor {n} belongs to another fork"
            );
        }
        Some(d)
    };
    // The id chain from the left anchor to the right one, over the source blocks.
    let mut expected = left_id;
    let mut source_bodies = Vec::new();
    for n in r.first_block..=right {
        let packed = match &after {
            Some(after) if n == right => after.clone(),
            _ => src
                .block(n)?
                .with_context(|| format!("block sources lack block {n}"))?,
        };
        let h = block_header(&packed)?;
        ensure!(
            h.number == n && h.previous == expected,
            "id chain broken at block {n}"
        );
        expected = h.id;
        digest.update(n.to_le_bytes());
        digest.update((packed.len() as u64).to_le_bytes());
        digest.update(&packed);
        let (a, b) = log.bounds(n)?;
        digest.update(a.to_le_bytes());
        digest.update(b.to_le_bytes());
        if n == right {
            ensure!(h.id == right_id, "right anchor mismatch");
            break;
        }
        source_bodies.push(match donor {
            Some(d) => {
                let body = d.body(n)?;
                ensure!(d.id(n, &body)? == h.id, "SHIP block id mismatch at {n}");
                body
            }
            None => packed,
        });
    }
    // Every readable byte of the affected entries, which the rebuild must match exactly.
    let mut readable = Vec::new();
    let mut cursor = r.span_start;
    for e in r.extents.iter().copied().chain([Extent {
        offset: r.span_end,
        len: 0,
    }]) {
        if e.offset > cursor {
            readable.push((cursor, log.log.read(cursor, e.offset - cursor)?));
        }
        cursor = e.end();
    }

    // The source's entries as they are; for SHIP logs, failing that, the same payloads
    // re-encoded the way the target's own anchors are encoded.
    let (encoding, bodies, span) = match assemble(log, r, &source_bodies, &readable) {
        Ok(span) => ("source bytes as they are".to_string(), source_bodies, span),
        Err(as_is) if log.kind != Kind::Blocks => {
            translate(log, r, &source_bodies, &readable, as_is)?
        }
        Err(e) => return Err(e),
    };
    for body in &bodies {
        digest.update((body.len() as u64).to_le_bytes());
        digest.update(body);
    }
    digest.update(encoding.as_bytes());
    for (_, have) in &readable {
        digest.update(have);
    }
    for e in &r.extents {
        let at = (e.offset - r.span_start) as usize;
        out.extend_from_slice(&span[at..at + e.len as usize]);
    }
    Ok(encoding)
}

/// Lay `bodies` (entries `first_block..=last_block`, trailers excluded) out at the target's
/// offsets and check them: lengths, orphaned fork entries, and every readable byte.
fn assemble(
    log: &Log,
    r: &Region,
    bodies: &[Vec<u8>],
    readable: &[(u64, Vec<u8>)],
) -> Result<Vec<u8>> {
    let mut span = Vec::new();
    for (n, body) in (r.first_block..).zip(bodies) {
        let (a, b) = log.bounds(n)?;
        let own = body.len() as u64 + 8;
        let mismatch = || {
            format!(
                "entry {n} length {own} differs from target {}; cannot safely splice this \
                 encoding; use a matching archive or replay build/configuration",
                b - a
            )
        };
        ensure!(
            own == b - a || (log.kind != Kind::Blocks && own < b - a),
            "{}",
            mismatch()
        );
        // A shorter SHIP entry leaves orphaned fork entries before the next index position.
        // They are kept as they are: readable, outside every damaged extent, and framed.
        let gap = if own < b - a {
            let (from, to) = (a + own, b);
            if let Some(e) = r.extents.iter().find(|e| e.offset < to && from < e.end()) {
                bail!(
                    "block {n} is followed by orphaned fork entries at bytes {from}..{to}, and \
                     damage at bytes {}..{} reaches them; {ORPHANS_UNREPAIRABLE}",
                    e.offset,
                    e.end()
                );
            }
            let gap = log.log.read(from, to - from)?;
            orphans(&gap, from).with_context(|| {
                format!(
                    "{}, and the bytes after it are not orphaned fork entries",
                    mismatch()
                )
            })?;
            gap
        } else {
            Vec::new()
        };
        span.extend_from_slice(body);
        span.extend_from_slice(&a.to_le_bytes());
        span.extend_from_slice(&gap);
    }
    ensure!(
        span.len() as u64 == r.span_end - r.span_start,
        "rebuilt span length mismatch"
    );
    for (from, have) in readable {
        let at = (from - r.span_start) as usize;
        ensure!(
            span[at..at + have.len()] == have[..],
            "rebuilt bytes {from}..{} differ from the readable target bytes; the source does \
             not match this log",
            from + have.len() as u64
        );
    }
    Ok(span)
}

/// Re-encode the source payloads in every encoding that reproduces the target's readable
/// anchors byte for byte, and keep the one result that fits. Several different results that
/// all fit, or none, are refused.
fn translate(
    log: &Log,
    r: &Region,
    source_bodies: &[Vec<u8>],
    readable: &[(u64, Vec<u8>)],
    as_is: anyhow::Error,
) -> Result<(String, Vec<Vec<u8>>, Vec<u8>)> {
    let (left, right) = r.source_range();
    let (marker, codecs) = calibrate(log, left, right)
        .with_context(|| format!("the source does not fit as it is ({as_is:#})"))?;
    let payloads = source_bodies
        .iter()
        .map(|body| ship_payload(body))
        .collect::<Result<Vec<_>>>()?;
    let mut fits: Vec<(Codec, Vec<Vec<u8>>, Vec<u8>)> = Vec::new();
    let mut misses = Vec::new();
    for codec in codecs {
        let bodies = payloads
            .iter()
            .map(|(id, raw)| encode_ship_with(id, raw, marker, &codec))
            .collect::<Result<Vec<_>>>()?;
        match assemble(log, r, &bodies, readable) {
            Ok(span) => {
                if !fits.iter().any(|(_, _, s)| *s == span) {
                    fits.push((codec, bodies, span));
                }
            }
            Err(e) => misses.push(format!("{}: {e:#}", codec.name())),
        }
    }
    match fits.len() {
        1 => {
            let (codec, bodies, span) = fits.pop().expect("one fit");
            Ok((format!("re-encoded as {}", codec.name()), bodies, span))
        }
        0 => bail!(
            "the source does not fit as it is ({as_is:#}), and no encoding that reproduces the \
             anchors fits either: {}",
            misses.join("; ")
        ),
        _ => bail!(
            "several encodings reproduce the anchors and fit this region with different bytes \
             ({}); the damaged bytes cannot be told apart, so the region is refused",
            fits.iter()
                .map(|(c, _, _)| c.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The payload preamble and the encodings that reproduce every readable anchor of a region
/// byte for byte. An anchor too large to read whole (a full chain state) is skipped.
fn calibrate(log: &Log, left: u32, right: u32) -> Result<(u32, Vec<Codec>)> {
    let mut marker = None;
    let mut fitting: Option<Vec<Codec>> = None;
    let mut per_anchor = Vec::new();
    for n in [left, right] {
        let Ok(body) = log.body(n) else { continue };
        let (id, raw) = ship_payload(&body)?;
        let m = ship_marker(&body);
        ensure!(
            marker.is_none_or(|prev| prev == m),
            "the SHIP payload preamble differs between anchors {left} and {right}: the node \
             was upgraded inside this region, which is not supported"
        );
        marker = Some(m);
        let fits: Vec<Codec> = Codec::candidates()
            .into_iter()
            .filter(|c| encode_ship_with(&id, &raw, m, c).is_ok_and(|e| e == body))
            .collect();
        ensure!(
            !fits.is_empty(),
            "no known encoding reproduces anchor block {n}. Its node may have used zlib-ng or \
             another non-stock zlib (check `ldd $(which nodeos)` on that host); use a donor log \
             whose entries are byte-identical"
        );
        per_anchor.push(n);
        fitting = Some(match fitting {
            None => fits,
            Some(prev) => prev.into_iter().filter(|c| fits.contains(c)).collect(),
        });
    }
    let fitting = fitting.with_context(|| {
        format!("neither anchor ({left}, {right}) can be read whole to calibrate the encoding")
    })?;
    ensure!(
        !fitting.is_empty(),
        "anchors {left} and {right} were written with different encodings: the node was \
         upgraded inside this region (for example to Spring), which is not supported"
    );
    Ok((marker.expect("an anchor was read"), fitting))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub schema: u32,
    pub tool_version: String,
    pub log: PathBuf,
    pub index: PathBuf,
    pub kind: Kind,
    pub target: Target,
    pub log_stamp: Stamp,
    pub index_stamp: Stamp,
    pub sources: Sources,
    pub replacement_sha256: String,
    pub evidence_sha256: String,
    pub source_trust: String,
}

const PLAN_SCHEMA: u32 = 2;
const SOURCE_TRUST: &str = "Block ids authenticate block headers, not transaction bodies or \
    SHIP payloads. Approve only sources from trusted independent operators or a controlled replay.";

pub fn build(log: &Log, t: &Target, sources: &Sources, out: &Path) -> Result<Out> {
    let sources = sources.canonicalize()?;
    let log_stamp = log.log.stamp()?;
    let index_stamp = log.index.stamp()?;
    let loaded = sources.load(log.kind)?;
    let rebuilt = rebuild(log, t, &loaded)?;
    let mut states = Vec::new();
    for (e, piece) in pieces(t, &rebuilt.replacement) {
        let state = extent_state(&log.log, t.record_size, e, piece)?;
        ensure!(
            state != State::Differs,
            "bytes {}..{} are readable and differ from the rebuilt replacement; this tool never \
             overwrites readable data",
            e.offset,
            e.end()
        );
        states.push(state);
    }
    ensure!(
        log.log.stamp()? == log_stamp && log.index.stamp()? == index_stamp,
        "target changed during build; stop the node and rebuild"
    );
    log.log.check_path()?;
    log.index.check_path()?;
    let plan = Plan {
        schema: PLAN_SCHEMA,
        tool_version: env!("CARGO_PKG_VERSION").into(),
        log: log.log.path.clone(),
        index: log.index.path.clone(),
        kind: log.kind,
        target: t.clone(),
        log_stamp,
        index_stamp,
        sources,
        replacement_sha256: hash(&rebuilt.replacement),
        evidence_sha256: rebuilt.evidence_sha256,
        source_trust: SOURCE_TRUST.into(),
    };
    let json = serde_json::to_vec_pretty(&plan)?;
    create_dir(out)?;
    create_file(&out.join("replacement.bin"), &rebuilt.replacement)?;
    create_file(&out.join("plan.json"), &json)?;
    sync_dir(out)?;
    let plan_sha256 = hash(&json);
    let bundle = out.canonicalize()?;
    let apply = format!(
        "antelope-log-repair apply --bundle {} --approve {plan_sha256} --node-stopped",
        arg(bundle.to_string_lossy())
    );
    let unreadable = states.iter().filter(|s| **s == State::Unreadable).count();
    let mut text = String::new();
    writeln!(text, "Built repair bundle {}", bundle.display())?;
    writeln!(
        text,
        "  log:      {} ({}.log)",
        log.log.path.display(),
        log.kind.stem()
    )?;
    writeln!(
        text,
        "  damage:   {} region(s), {} extent(s), {} (record size {})",
        t.regions.len(),
        states.len(),
        bytes(t.damaged_bytes()),
        bytes(t.record_size)
    )?;
    for (i, (r, encoding)) in t.regions.iter().zip(&rebuilt.encodings).enumerate() {
        writeln!(text, "  [{}] {}", i + 1, region_line(t.record_size, r))?;
        writeln!(text, "      entries: {encoding}")?;
    }
    writeln!(
        text,
        "  state:    {unreadable} extent(s) unreadable (will be written), {} already identical",
        states.len() - unreadable
    )?;
    writeln!(
        text,
        "  sources:  {} P2PBLK01 file(s), {} blocks.log copy(ies), {} SHIP donor log(s)",
        plan.sources.blocks.len(),
        plan.sources.block_logs.len(),
        plan.sources.donors.len()
    )?;
    writeln!(text, "  plan_sha256: {plan_sha256}")?;
    writeln!(text, "\nTrust: {SOURCE_TRUST}")?;
    writeln!(
        text,
        "\nReview plan.json and the source provenance, stop nodeos, then:\n  {apply}"
    )?;
    let json = json!({
        "ok": true,
        "bundle": bundle,
        "encodings": rebuilt.encodings,
        "plan_sha256": plan_sha256,
        "replacement_sha256": plan.replacement_sha256,
        "regions": t.regions.iter().map(|r| region_json(t.record_size, r)).collect::<Vec<_>>(),
        "extent_states": states,
        "source_trust": SOURCE_TRUST,
        "next_command": apply,
    });
    Ok(Out { json, text })
}

pub fn read_plan(bundle: &Path, approval: &str) -> Result<(Plan, Vec<u8>)> {
    let plan_disk = Disk::open(&bundle.join("plan.json"), false)?;
    ensure!(plan_disk.stamp()?.size <= 1 << 20, "plan exceeds 1 MiB");
    let json = plan_disk.read(0, plan_disk.stamp()?.size)?;
    ensure!(
        hash(&json).eq_ignore_ascii_case(approval.trim()),
        "plan hash does not match --approve"
    );
    let plan: Plan = serde_json::from_slice(&json).context("plan.json is not a schema-2 plan")?;
    ensure!(plan.schema == PLAN_SCHEMA, "unsupported plan schema");
    let len = plan.target.damaged_bytes();
    ensure!(len <= MAX_BYTES, "plan replacement exceeds 256 MiB");
    let disk = Disk::open(&bundle.join("replacement.bin"), false)?;
    ensure!(
        disk.stamp()?.size == len,
        "replacement artifact length mismatch"
    );
    let replacement = disk.read(0, len)?;
    ensure!(
        hash(&replacement) == plan.replacement_sha256,
        "replacement artifact hash mismatch"
    );
    Ok((plan, replacement))
}

pub fn apply(bundle: &Path, approval: &str, node_stopped: bool) -> Result<Out> {
    ensure!(
        node_stopped,
        "stop nodeos and pass --node-stopped; advisory locks cannot exclude nodeos"
    );
    let (plan, replacement) = read_plan(bundle, approval)?;
    let log = Log::open(&plan.log, Some(&plan.index), cfg!(unix))?;
    apply_open(&log, &plan, &replacement, bundle)
}

pub fn apply_open(log: &Log, plan: &Plan, replacement: &[u8], bundle: &Path) -> Result<Out> {
    #[cfg(not(unix))]
    let _ = bundle;
    // Locks coordinate repair processes only. nodeos must be stopped by the operator.
    fs2::FileExt::try_lock_exclusive(&log.log.file)
        .context("another process holds the log lock")?;
    fs2::FileExt::try_lock_shared(&log.index.file).context("index is locked")?;
    // nodeos ignores advisory locks; on Linux, also refuse while any process has the files open.
    #[cfg(target_os = "linux")]
    for disk in [&log.log, &log.index] {
        let pids = disk.other_openers()?;
        ensure!(
            pids.is_empty(),
            "process(es) {pids:?} have {} open; stop nodeos and every other reader/writer",
            disk.path.display()
        );
    }
    let extents: Vec<Extent> = plan.target.extents().copied().collect();
    let t = Target::new(log, plan.target.record_size, &extents)?;
    ensure!(
        t == plan.target && log.kind == plan.kind,
        "target layout differs from approved plan"
    );
    let index_now = log.index.stamp()?;
    ensure!(
        index_now == plan.index_stamp,
        "index changed since build ({}); build and review a new plan",
        index_now.changes(&plan.index_stamp)
    );
    log.log.check_path()?;
    log.index.check_path()?;
    let pieces = pieces(&t, replacement);
    let mut states = Vec::new();
    for (e, piece) in &pieces {
        let state = extent_state(&log.log, t.record_size, e, piece)?;
        if state == State::Differs {
            bail!(
                "bytes {}..{} are readable and differ from the approved replacement; refusing \
                 to overwrite readable data",
                e.offset,
                e.end()
            );
        }
        states.push(state);
    }
    let before = log.log.stamp()?;
    let pending: Vec<usize> = (0..states.len())
        .filter(|&i| states[i] == State::Unreadable)
        .collect();
    if !pending.is_empty() {
        ensure!(
            before == plan.log_stamp,
            "log changed since build ({}); rebuild the plan",
            before.changes(&plan.log_stamp)
        );
    }
    let loaded = plan.sources.load(log.kind)?;
    let rebuilt = rebuild(log, &t, &loaded)?;
    ensure!(
        rebuilt.replacement == replacement && rebuilt.evidence_sha256 == plan.evidence_sha256,
        "sources or target evidence changed since build"
    );
    ensure!(
        log.log.stamp()? == before && log.index.stamp()? == plan.index_stamp,
        "files changed during verification"
    );
    if pending.is_empty() {
        return Ok(Out {
            json: json!({
                "ok": true,
                "applied": false,
                "detail": "every extent already matches; anchors, source bytes, and index reverified",
            }),
            text: "Nothing to write: every extent already matches the approved replacement \
                   (anchors, sources, and index reverified).\n"
                .into(),
        });
    }
    #[cfg(not(unix))]
    bail!("in-place media repair is supported on Unix only; use scan/build on this platform");
    #[cfg(unix)]
    {
        let planned: Vec<Extent> = pending.iter().map(|&i| *pieces[i].0).collect();
        // A durable, non-overwriting intent journal exists BEFORE any target mutation.
        let intent = json!({
            "plan": plan,
            "extent_states_before": states,
            "extents_to_write": planned,
            "original_bytes_backup": "unavailable: EIO/ENODATA",
            "write_status": "intent only; consult receipt.json or scan the target before recovery",
        });
        create_file(
            &bundle.join("apply-intent.json"),
            &serde_json::to_vec_pretty(&intent)?,
        )?;
        log.log.check_path()?;
        log.index.check_path()?;
        ensure!(
            log.log.stamp()? == before && log.index.stamp()? == plan.index_stamp,
            "files changed before write"
        );
        for (i, (e, piece)) in pieces.iter().enumerate() {
            ensure!(
                extent_state(&log.log, t.record_size, e, piece)? == states[i],
                "extent state changed before write; no write performed"
            );
        }
        let mut written = Vec::new();
        for &i in &pending {
            let (e, piece) = pieces[i];
            log.log.write_extent(e.offset, piece).with_context(|| {
                format!(
                    "write bytes {}..{} ({} of {} extents already written: {written:?}); \
                     inspect apply-intent.json and do not retry automatically. The page cache \
                     may now serve the replacement bytes, so a scan can look clean while the \
                     device still fails: check the device or pool and drop caches (or remount) \
                     before scanning again",
                    e.offset,
                    e.end(),
                    written.len(),
                    pending.len()
                )
            })?;
            written.push(*e);
        }
        ensure!(
            log.log.stamp()?.size == before.size,
            "log size changed during write"
        );
        ensure!(
            log.index.stamp()? == plan.index_stamp,
            "index changed during write"
        );
        for (e, piece) in &pieces {
            ensure!(
                extent_state(&log.log, t.record_size, e, piece)? == State::Identical,
                "read-back mismatch at bytes {}..{} after write; inspect apply-intent.json",
                e.offset,
                e.end()
            );
        }
        let verified = rebuild(log, &t, &loaded)?;
        ensure!(
            verified.evidence_sha256 == plan.evidence_sha256 && verified.replacement == replacement,
            "post-write verification failed"
        );
        // Parse repaired entries from DISK, including zlib checks for SHIP.
        for r in &t.regions {
            let (left, right) = r.source_range();
            let mut prev = anchor_id(log, left)?;
            for n in r.first_block..=right {
                let body = log.body(n)?;
                let id = log.id(n, &body)?;
                if log.kind == Kind::Blocks {
                    ensure!(
                        block_header(&body)?.previous == prev,
                        "post-write chain broken at {n}"
                    );
                } else {
                    ensure!(
                        Some(id)
                            == loaded
                                .block(n)?
                                .map(|b| block_header(&b).map(|h| h.id))
                                .transpose()?,
                        "post-write SHIP id mismatch at {n}"
                    );
                }
                prev = id;
            }
        }
        log.log.check_path()?;
        log.index.check_path()?;
        let receipt = json!({
            "ok": true,
            "applied": true,
            "log": plan.log,
            "written_extents": written,
            "replacement_sha256": plan.replacement_sha256,
            "detail": "each unreadable extent written once and fsynced; read-back (may be \
                served from cache) and anchored entries reverified; run a filesystem scrub to \
                verify media",
        });
        create_file(
            &bundle.join("receipt.json"),
            &serde_json::to_vec_pretty(&receipt)?,
        )?;
        let text = format!(
            "Repaired {}: wrote {} extent(s), {}; read-back and anchored entries verified.\n\
             Receipt: {}\n\nNext: verify the media (ZFS: zpool scrub, then zpool clear once \
             clean), restart nodeos, and check the repaired blocks through its API/SHIP.\n",
            plan.log.display(),
            written.len(),
            bytes(written.iter().map(|e| e.len).sum()),
            bundle.join("receipt.json").display()
        );
        Ok(Out {
            json: receipt,
            text,
        })
    }
}
