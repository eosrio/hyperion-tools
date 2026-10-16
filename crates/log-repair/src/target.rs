//! Damage geometry and detection.
//!
//! Unreadable bytes are described as extents: contiguous spans aligned to the record size.
//! Extents touching the same or directly neighboring entries form one region, which is
//! rebuilt between two readable anchor entries. Scanning finds extents with positional
//! reads; nothing here writes.
use std::time::Instant;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::format::{Log, MAX_BLOCKS};
use crate::io::{Disk, MAX_BYTES, media_error};

pub const MIN_RECORD: u64 = 512;
pub const MAX_RECORD: u64 = 16 * 1024 * 1024;
/// Regions (independent damaged areas) in one plan.
pub const MAX_REGIONS: usize = 64;
/// Large sequential reads; a failing chunk is re-probed one record at a time.
const SCAN_CHUNK: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Extent {
    pub offset: u64,
    pub len: u64,
}
impl Extent {
    pub fn end(&self) -> u64 {
        self.offset + self.len
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Region {
    /// First and last block whose entries overlap a damaged extent.
    pub first_block: u32,
    pub last_block: u32,
    /// Byte span of those entries, including position trailers.
    pub span_start: u64,
    pub span_end: u64,
    pub extents: Vec<Extent>,
}
impl Region {
    /// Blocks a source must supply: the affected entries plus both anchors.
    pub fn source_range(&self) -> (u32, u32) {
        (self.first_block - 1, self.last_block + 1)
    }

    fn check(&self, log: &Log) -> Result<()> {
        ensure!(
            self.first_block > log.first && self.last_block < log.last,
            "touches the log's first or head entry; a readable anchor entry is required on \
             both sides"
        );
        ensure!(
            self.last_block - self.first_block < MAX_BLOCKS - 2,
            "affects {} blocks; the limit is {}",
            self.last_block - self.first_block + 1,
            MAX_BLOCKS - 2
        );
        ensure!(
            self.span_end - self.span_start <= MAX_BYTES,
            "affected entries span {} bytes; the limit is 256 MiB",
            self.span_end - self.span_start
        );
        for n in self.first_block - 1..=self.last_block + 1 {
            log.bounds(n)?;
        }
        // Everything the rebuild compares against must be readable: both anchor entries
        // and every byte of the affected entries outside the damaged extents.
        let (from, _) = log.bounds(self.first_block - 1)?;
        let (_, to) = log.bounds(self.last_block + 1)?;
        let mut cursor = from;
        for e in self
            .extents
            .iter()
            .copied()
            .chain([Extent { offset: to, len: 0 }])
        {
            if let Some(bad) = first_unreadable(&log.log, cursor, e.offset)? {
                bail!(
                    "byte {bad} near this damage is unreadable too, outside the detected \
                     extents; scan a wider range (--block-range, or a full scan)"
                );
            }
            cursor = e.end();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub record_size: u64,
    pub regions: Vec<Region>,
}

/// One group of damaged extents and whether it can be repaired.
pub struct Finding {
    pub extents: Vec<Extent>,
    pub region: Option<Region>,
    pub problem: Option<String>,
}

impl Target {
    /// Validated, repairable geometry. Every finding must be repairable.
    pub fn new(log: &Log, record_size: u64, extents: &[Extent]) -> Result<Self> {
        let findings = analyze(log, record_size, extents)?;
        let problems: Vec<String> = findings
            .iter()
            .filter_map(|f| {
                f.problem
                    .as_ref()
                    .map(|p| format!("{}: {p}", describe(&f.extents)))
            })
            .collect();
        ensure!(
            problems.is_empty(),
            "damage cannot be repaired by this tool:\n  {}",
            problems.join("\n  ")
        );
        let regions: Vec<Region> = findings.into_iter().filter_map(|f| f.region).collect();
        ensure!(
            regions.len() <= MAX_REGIONS,
            "{} damaged regions; the limit per plan is {MAX_REGIONS}; repair a --block-range \
             at a time",
            regions.len()
        );
        let t = Self {
            record_size,
            regions,
        };
        ensure!(
            t.damaged_bytes() <= MAX_BYTES,
            "{} damaged bytes; the limit per plan is 256 MiB",
            t.damaged_bytes()
        );
        Ok(t)
    }

    pub fn extents(&self) -> impl Iterator<Item = &Extent> {
        self.regions.iter().flat_map(|r| &r.extents)
    }
    pub fn damaged_bytes(&self) -> u64 {
        self.extents().map(|e| e.len).sum()
    }
    pub fn first_block(&self) -> u32 {
        self.regions[0].first_block
    }
    pub fn last_block(&self) -> u32 {
        self.regions[self.regions.len() - 1].last_block
    }
}

pub fn describe(extents: &[Extent]) -> String {
    match extents {
        [e] => format!("bytes {}..{}", e.offset, e.end()),
        [first, .., last] => format!(
            "{} extents in bytes {}..{}",
            extents.len(),
            first.offset,
            last.end()
        ),
        [] => "no extents".into(),
    }
}

pub fn check_record_size(size: u64) -> Result<()> {
    ensure!(
        size.is_power_of_two() && (MIN_RECORD..=MAX_RECORD).contains(&size),
        "record size {size} must be a power of two between 512 B and 16 MiB"
    );
    Ok(())
}

/// Probe and write granularity: explicit, else the file's `st_blksize`.
pub fn record_size(disk: &Disk, explicit: Option<u64>) -> Result<u64> {
    let size = match explicit {
        Some(size) => size,
        None => disk
            .block_size()
            .context("this platform reports no filesystem block size; pass --record-size")?,
    };
    check_record_size(size).context("pass --record-size with the filesystem's record size")?;
    Ok(size)
}

/// Inclusive record index ranges to extents: sorted, overlapping or adjacent ranges joined.
pub fn extents_from_ranges(record_size: u64, ranges: &[(u64, u64)]) -> Result<Vec<Extent>> {
    let mut ranges = ranges.to_vec();
    ranges.sort_unstable();
    let mut out: Vec<Extent> = Vec::new();
    for (a, b) in ranges {
        ensure!(a <= b, "record range {a}-{b} is reversed");
        let offset = a
            .checked_mul(record_size)
            .context("record offset overflow")?;
        let end = b
            .checked_add(1)
            .and_then(|n| n.checked_mul(record_size))
            .context("record end overflow")?;
        match out.last_mut() {
            Some(e) if offset <= e.end() => e.len = e.len.max(end - e.offset),
            _ => out.push(Extent {
                offset,
                len: end - offset,
            }),
        }
    }
    Ok(out)
}

/// The block whose entry (including its trailer) contains `offset`.
fn containing(log: &Log, offset: u64) -> Result<u32> {
    let (mut lo, mut hi) = (log.first, log.last);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if log.pos(mid)? <= offset {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let (a, b) = log.bounds(lo)?;
    ensure!(
        a <= offset && offset < b,
        "index does not contain offset {offset}"
    );
    Ok(lo)
}

/// Group extents into regions and report, per group, why it cannot be repaired (if so).
pub fn analyze(log: &Log, record_size: u64, extents: &[Extent]) -> Result<Vec<Finding>> {
    check_record_size(record_size)?;
    ensure!(!extents.is_empty(), "no damaged extents");
    for e in extents {
        ensure!(
            e.len > 0
                && e.offset.is_multiple_of(record_size)
                && e.len.is_multiple_of(record_size)
                && e.offset.checked_add(e.len).is_some(),
            "extent {e:?} is not aligned to the {record_size}-byte record size"
        );
    }
    for w in extents.windows(2) {
        ensure!(
            w[0].end() < w[1].offset,
            "extents must be sorted, disjoint, and not adjacent"
        );
    }
    let header_end = log.pos(log.first)?;
    let mut findings = Vec::new();
    let mut regions: Vec<Region> = Vec::new();
    for e in extents {
        let problem = if e.offset < header_end {
            Some("overlaps the log header")
        } else if e.end() > log.size {
            Some("reaches past the last entry; the head entry is out of scope")
        } else {
            None
        };
        if let Some(p) = problem {
            findings.push(Finding {
                extents: vec![*e],
                region: None,
                problem: Some(p.into()),
            });
            continue;
        }
        let (first, last) = (containing(log, e.offset)?, containing(log, e.end() - 1)?);
        match regions.last_mut() {
            // Overlapping or directly adjacent entries: no readable anchor between them.
            Some(r) if first <= r.last_block + 1 => {
                r.last_block = r.last_block.max(last);
                r.extents.push(*e);
            }
            _ => regions.push(Region {
                first_block: first,
                last_block: last,
                span_start: 0,
                span_end: 0,
                extents: vec![*e],
            }),
        }
    }
    for mut r in regions {
        r.span_start = log.pos(r.first_block)?;
        r.span_end = log.pos(r.last_block + 1)?;
        let problem = r.check(log).err().map(|e| format!("{e:#}"));
        findings.push(Finding {
            extents: r.extents.clone(),
            region: Some(r),
            problem,
        });
    }
    findings.sort_by_key(|f| f.extents[0].offset);
    Ok(findings)
}

/// First unreadable byte offset (to record granularity) in `from..to`, if any.
pub fn first_unreadable(disk: &Disk, from: u64, to: u64) -> Result<Option<u64>> {
    let mut pos = from;
    while pos < to {
        let len = (to - pos).min(SCAN_CHUNK);
        match disk.read(pos, len) {
            Ok(_) => pos += len,
            Err(e) if media_error(&e) => return Ok(Some(pos)),
            Err(e) => return Err(e).with_context(|| format!("read {}", disk.path.display())),
        }
    }
    Ok(None)
}

/// Where to look for damage.
#[derive(Debug, Clone)]
pub enum Selection {
    /// Explicit inclusive record index ranges (a ZFS `zio_blkid` is a record index);
    /// nothing is scanned.
    Records(Vec<(u64, u64)>),
    /// Scan the records holding one block's entry, following damage outward.
    Block(u32),
    /// Scan the records holding these blocks' entries, following damage outward.
    Blocks(u32, u32),
    /// Scan the whole file.
    Whole,
}

#[derive(Debug, Clone, Serialize)]
pub struct Scan {
    pub from: u64,
    pub to: u64,
    pub bytes_read: u64,
    pub failed_reads: u32,
    pub seconds: f64,
}

pub struct Located {
    pub record_size: u64,
    pub extents: Vec<Extent>,
    /// `None` when the extents were given explicitly.
    pub scan: Option<Scan>,
}

pub fn locate(
    log: &Log,
    record_size: Option<u64>,
    selection: &Selection,
    max_failed_reads: u32,
) -> Result<Located> {
    let record_size = self::record_size(&log.log, record_size)?;
    let (from, to) = match selection {
        Selection::Records(ranges) => {
            return Ok(Located {
                record_size,
                extents: extents_from_ranges(record_size, ranges)?,
                scan: None,
            });
        }
        Selection::Block(n) => (*n, *n),
        Selection::Blocks(a, b) => (*a, *b),
        Selection::Whole => {
            let scan = scan(
                &log.log,
                log.size,
                record_size,
                0,
                log.size,
                max_failed_reads,
            )?;
            return Ok(Located {
                record_size,
                extents: scan.1,
                scan: Some(scan.0),
            });
        }
    };
    ensure!(
        from <= to && log.first <= from && to <= log.last,
        "block range {from}..{to} is outside the log's {}..{}",
        log.first,
        log.last
    );
    let (start, end) = (log.pos(from)?, log.pos(to + 1)?);
    let (scan, extents) = scan(
        &log.log,
        log.size,
        record_size,
        start,
        end,
        max_failed_reads,
    )?;
    Ok(Located {
        record_size,
        extents,
        scan: Some(scan),
    })
}

/// Read `start..end` (widened to whole records) in large chunks; re-probe failing chunks
/// one record at a time. Damage touching either end of a partial range is followed
/// outward so every extent is complete. Read-only.
pub fn scan(
    disk: &Disk,
    size: u64,
    record_size: u64,
    start: u64,
    end: u64,
    max_failed_reads: u32,
) -> Result<(Scan, Vec<Extent>)> {
    let started = Instant::now();
    let limit = size.next_multiple_of(record_size);
    let from = start - start % record_size;
    let to = end.next_multiple_of(record_size).min(limit);
    let chunk = SCAN_CHUNK.max(record_size);
    let mut bad: Vec<u64> = Vec::new();
    let mut failed = 0u32;
    let mut bytes_read = 0u64;
    let probe = |offset: u64, len: u64, failed: &mut u32, bytes_read: &mut u64| {
        let len = len.min(size - offset);
        *bytes_read += len;
        match disk.read(offset, len) {
            Ok(_) => Ok(true),
            Err(e) if media_error(&e) => {
                *failed += 1;
                ensure!(
                    *failed <= max_failed_reads,
                    "stopped after {max_failed_reads} failed reads. Damage this widespread is \
                     beyond a record repair: check the device and pool health first. Raise \
                     --max-bad-reads to continue scanning."
                );
                Ok(false)
            }
            Err(e) => Err(anyhow::Error::from(e).context(format!(
                "read {} at {offset} (only EIO/ENODATA indicate damage)",
                disk.path.display()
            ))),
        }
    };
    let progress_every = 4u64 << 30;
    let mut next_progress = progress_every;
    let mut pos = from;
    while pos < to {
        let len = chunk.min(to - pos);
        if !probe(pos, len, &mut failed, &mut bytes_read)? {
            let mut r = pos;
            while r < pos + len {
                if !probe(r, record_size, &mut failed, &mut bytes_read)? {
                    bad.push(r);
                }
                r += record_size;
            }
        }
        pos += len;
        if to - from > progress_every && pos - from >= next_progress {
            next_progress += progress_every;
            eprintln!(
                "scanned {} of {} ({:.0}%), {} unreadable records so far",
                crate::report::bytes(pos - from),
                crate::report::bytes(to - from),
                (pos - from) as f64 * 100.0 / (to - from) as f64,
                bad.len()
            );
        }
    }
    // Follow damage that runs past either end of a partial scan.
    if let Some(&first) = bad.first()
        && first == from
    {
        let mut r = from;
        while r >= record_size
            && !probe(r - record_size, record_size, &mut failed, &mut bytes_read)?
        {
            r -= record_size;
            bad.insert(0, r);
            ensure!(from - r <= MAX_BYTES, "contiguous damage exceeds 256 MiB");
        }
    }
    if let Some(&last) = bad.last()
        && last + record_size == to
    {
        let mut r = to;
        while r < limit && !probe(r, record_size, &mut failed, &mut bytes_read)? {
            bad.push(r);
            r += record_size;
            ensure!(r - to <= MAX_BYTES, "contiguous damage exceeds 256 MiB");
        }
    }
    let ranges: Vec<(u64, u64)> = bad
        .iter()
        .map(|o| (o / record_size, o / record_size))
        .collect();
    let extents = extents_from_ranges(record_size, &ranges)?;
    let scan = Scan {
        from: bad.first().copied().unwrap_or(from).min(from),
        to: bad.last().map_or(to, |l| (l + record_size).max(to)),
        bytes_read,
        failed_reads: failed,
        seconds: started.elapsed().as_secs_f64(),
    };
    Ok((scan, extents))
}
