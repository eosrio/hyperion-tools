//! Recover crash-damaged tails to new files, independently of the target index.
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::format::{Header, Kind, Log, num, ship_payload, u64le, unsupported_ship_magic};
use crate::framing::{self, Incomplete};
use crate::io::{self, Disk, MAX_BYTES, Stamp};
use crate::repair::{Loaded, Sources};
use crate::report::Out;

pub mod trim;

#[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Restore,
    Salvage,
}

pub struct Options<'a> {
    pub log: &'a Path,
    pub index: Option<&'a Path>,
    pub sources: &'a Sources,
    pub through: Option<u32>,
    pub mode: Mode,
    pub out: Option<&'a Path>,
    pub node_stopped: bool,
}

struct Pin {
    path: PathBuf,
    disk: Option<Disk>,
    stamp: Option<Stamp>,
}
impl Pin {
    fn new(path: &Path, optional: bool) -> Result<Self> {
        if optional && !path.try_exists()? {
            return Ok(Self {
                path: path.to_path_buf(),
                disk: None,
                stamp: None,
            });
        }
        let disk = Disk::open(path, false)?;
        Ok(Self {
            path: disk.path.clone(),
            stamp: Some(disk.stamp()?),
            disk: Some(disk),
        })
    }
    fn check(&self) -> Result<()> {
        if let Some(disk) = &self.disk {
            disk.check_path()?;
            ensure!(
                Some(disk.stamp()?) == self.stamp,
                "input changed during recovery: {}",
                self.path.display()
            );
        } else {
            ensure!(
                !self.path.try_exists()?,
                "index appeared during recovery: {}",
                self.path.display()
            );
        }
        Ok(())
    }
}

fn pinned_disk(pins: &[(PathBuf, Pin)], path: &Path) -> Result<Disk> {
    pins.iter()
        .find(|(name, _)| name == path)
        .with_context(|| format!("source was not pinned: {}", path.display()))?
        .1
        .disk
        .as_ref()
        .context("source is missing")?
        .try_clone()
}

fn hash_range(disk: &Disk, mut from: u64, to: u64, hash: &mut Sha256) -> Result<()> {
    while from < to {
        let n = (to - from).min(16 << 20);
        hash.update(disk.read(from, n)?);
        from += n;
    }
    Ok(())
}

fn hash_suffix(disk: &Disk, mut at: u64, end: u64, all: &mut Sha256) -> Result<String> {
    let mut tail = Sha256::new();
    while at < end {
        let count = (end - at).min(16 << 20);
        let bytes = disk.read(at, count)?;
        all.update(&bytes);
        tail.update(&bytes);
        at += count;
    }
    Ok(hex::encode(tail.finalize()))
}

#[cfg(test)]
fn disk_hash(disk: &Disk) -> Result<String> {
    let mut hash = Sha256::new();
    hash_range(disk, 0, disk.stamp()?.size, &mut hash)?;
    Ok(hex::encode(hash.finalize()))
}

#[derive(PartialEq, Eq)]
struct Start {
    first: u32,
    bytes: Vec<u8>,
    chain_id: Option<[u8; 32]>,
}

fn start(disk: &Disk, kind: Kind) -> Result<Start> {
    let size = disk.stamp()?.size;
    if kind == Kind::Blocks {
        // A bound on the preamble, including a WebAuthn genesis key's variable rpid.
        let mut bytes = disk.read(0, size.min(1 << 20))?;
        let p = framing::preamble(&bytes).context("invalid or unsupported blocks.log preamble")?;
        bytes.truncate(p.end);
        Ok(Start {
            first: p.first,
            bytes,
            chain_id: Some(p.chain_id),
        })
    } else {
        ensure!(
            size >= 48,
            "no complete SHIP header; cannot establish the first block or format"
        );
        let header = disk.read(0, 48)?;
        if let Some(problem) = unsupported_ship_magic(u64le(&header)) {
            bail!("{problem}");
        }
        let first = u32::from_be_bytes(header[8..12].try_into()?);
        ensure!(first > 0 && first < u32::MAX, "invalid first block");
        Ok(Start {
            first,
            bytes: Vec::new(),
            chain_id: None,
        })
    }
}

struct Entry {
    offset: u64,
    bytes: Vec<u8>, // includes the original position trailer
    id: [u8; 32],
    previous: Option<[u8; 32]>,
}
impl Entry {
    fn body(&self) -> &[u8] {
        &self.bytes[..self.bytes.len() - 8]
    }
    fn number(&self) -> u32 {
        num(&self.id)
    }
}

fn full_block(body: &[u8]) -> Result<Header> {
    let (header, end) = framing::signed_block(body)?;
    ensure!(end == body.len(), "bytes after signed_block");
    Ok(header)
}

struct ReadAhead<'a> {
    disk: &'a Disk,
    offset: u64,
    bytes: Vec<u8>,
}
impl ReadAhead<'_> {
    fn load(&mut self, at: u64, len: u64) -> Result<()> {
        self.bytes = self.disk.read(at, len)?;
        self.offset = at;
        Ok(())
    }
    fn from(&self, at: u64) -> &[u8] {
        &self.bytes[(at - self.offset) as usize..]
    }
    fn index_position(&mut self, at: u64, size: u64) -> Result<u64> {
        ensure!(
            at.checked_add(8).is_some_and(|end| end <= size),
            "incomplete index slot"
        );
        if at < self.offset || at + 8 > self.offset + self.bytes.len() as u64 {
            self.load(at, (size - at).min(64 << 10))?;
        }
        Ok(u64le(self.from(at)))
    }
    fn entry(&mut self, at: u64, size: u64, kind: Kind) -> Result<Entry> {
        if at < self.offset || at >= self.offset + self.bytes.len() as u64 {
            self.load(at, (size - at).min(64 << 10))?;
        }
        let (bytes, id, previous) = if kind == Kind::Blocks {
            let available = size - at;
            loop {
                let bytes = self.from(at);
                let len = bytes.len() as u64;
                match framing::signed_block(bytes) {
                    Ok((h, end)) if end + 8 <= bytes.len() => {
                        break (bytes[..end + 8].to_vec(), h.id, Some(h.previous));
                    }
                    Ok(_) => (),
                    Err(e) if e.is::<Incomplete>() => (),
                    Err(e) => return Err(e),
                }
                ensure!(
                    len < available,
                    "incomplete signed_block or position trailer"
                );
                ensure!(len < MAX_BYTES, "entry exceeds 256 MiB safety limit");
                self.load(at, (len * 2).max(64 << 10).min(available).min(MAX_BYTES))?;
            }
        } else {
            ensure!(size - at >= 48, "incomplete SHIP entry header");
            if self.from(at).len() < 48 {
                self.load(at, (size - at).min(64 << 10))?;
            }
            let h = &self.from(at)[..48];
            if let Some(problem) = unsupported_ship_magic(u64le(h)) {
                bail!("{problem}");
            }
            let len = u64le(&h[40..])
                .checked_add(56)
                .context("SHIP entry length overflow")?;
            ensure!(len <= MAX_BYTES, "entry exceeds 256 MiB safety limit");
            ensure!(
                len <= size - at,
                "incomplete SHIP payload or position trailer"
            );
            if (self.from(at).len() as u64) < len {
                self.load(at, len.max(64 << 10).min(size - at))?;
            }
            let bytes = self.from(at)[..len as usize].to_vec();
            let (id, _) = ship_payload(&bytes[..bytes.len() - 8])?;
            (bytes, id, None)
        };
        ensure!(
            u64le(&bytes[bytes.len() - 8..]) == at,
            "position trailer mismatch"
        );
        Ok(Entry {
            offset: at,
            bytes,
            id,
            previous,
        })
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Failure {
    offset: u64,
    next_block: u32,
    reason: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Summary {
    first_block: u32,
    last_block: Option<u32>,
    entries: u64,
    physical_prefix_end: u64,
    physical_prefix_sha256: String,
    excluded_suffix_sha256: String,
    excluded_suffix_bytes: u64,
    omitted_fork_entries: u64,
    reference_checked_entries: u64,
    input_sha256: String,
    reference_evidence_sha256: String,
    index_present: bool,
    index_bytes: u64,
    index_partial_bytes: u64,
    index_discrepancies: u64,
    index_sha256: Option<String>,
    index_prefix_sha256: Option<String>,
    index_suffix_sha256: Option<String>,
    indexed_head_hint: Option<u32>,
    failure: Option<Failure>,
}

/// Walk physical entries, choosing canonical SHIP entries only when block evidence is
/// available. The callback and report use the same parser, so output cannot use a weaker
/// definition of a complete entry than inspection does. Memory is bounded per entry.
fn walk(
    disk: &Disk,
    index: Option<&Disk>,
    kind: Kind,
    begin: &Start,
    refs: Option<&Loaded>,
    phase: &str,
    mut accept: impl FnMut(&Entry) -> Result<()>,
) -> Result<Summary> {
    let actual = start(disk, kind)?;
    ensure!(actual == *begin, "input preamble changed during recovery");
    let size = disk.stamp()?.size;
    eprintln!(
        "recover-tail [{phase}]: checking {}",
        crate::report::bytes(size)
    );
    let index_bytes = index
        .map(|i| i.stamp().map(|s| s.size))
        .transpose()?
        .unwrap_or(0);
    let mut hash = Sha256::new();
    hash.update(&actual.bytes);
    let mut evidence = Sha256::new();
    let mut at = begin.bytes.len() as u64;
    let (mut next, mut entries, mut orphans, mut compared, mut discrepancies) =
        (begin.first, 0u64, 0u64, 0u64, 0u64);
    let mut previous = None;
    let mut failure = None;
    let mut progress = 1u64 << 30;
    let mut reader = ReadAhead {
        disk,
        offset: 0,
        bytes: Vec::new(),
    };
    let mut index_reader = index.map(|disk| ReadAhead {
        disk,
        offset: 0,
        bytes: Vec::new(),
    });
    while at < size {
        let e = match reader.entry(at, size, kind) {
            Ok(e) => e,
            Err(e) => {
                // Media/read failures are a separate repair workflow. Do not silently
                // classify a failed read or concurrent truncation as a torn append.
                if e.downcast_ref::<std::io::Error>().is_some() {
                    return Err(e);
                }
                failure = Some(Failure {
                    offset: at,
                    next_block: next,
                    reason: format!("{e:#}"),
                });
                break;
            }
        };
        let n = e.number();
        let mut link = e.previous;
        let reference = refs.map(|r| r.block(n)).transpose()?.flatten();
        if let Some(body) = &reference {
            let h = full_block(body).with_context(|| format!("invalid reference block {n}"))?;
            ensure!(h.number == n, "reference block number mismatch");
            evidence.update(n.to_le_bytes());
            evidence.update((body.len() as u64).to_le_bytes());
            evidence.update(body);
            if kind != Kind::Blocks && h.id != e.id {
                // A structurally intact, externally identified noncanonical fork entry.
                hash.update(&e.bytes);
                at += e.bytes.len() as u64;
                orphans += 1;
                continue;
            }
            link = Some(h.previous);
        } else if kind != Kind::Blocks && refs.is_some() {
            failure = Some(Failure {
                offset: at,
                next_block: next,
                reason: format!(
                    "canonical block evidence lacks block {n}; remaining entries are unclassified"
                ),
            });
            break;
        }
        let problem = if n != next {
            Some(format!(
                "expected block {next}, found {n}; fork/gap selection is ambiguous without sufficient canonical evidence"
            ))
        } else if link.is_some() && previous.is_some() && link != previous {
            Some(format!("block {n} does not link to the preceding block"))
        } else if kind == Kind::Blocks
            && reference.as_ref().is_some_and(|b| b.as_slice() != e.body())
        {
            Some(format!("block {n} differs from the trusted block source"))
        } else {
            None
        };
        if let Some(reason) = problem {
            failure = Some(Failure {
                offset: at,
                next_block: next,
                reason,
            });
            break;
        }
        if reference.is_some() {
            compared += 1;
        }
        let slot = u64::from(n - begin.first) * 8;
        if let Some(index) = &mut index_reader {
            if slot + 8 > index_bytes || index.index_position(slot, index_bytes)? != e.offset {
                discrepancies += 1;
            }
        } else {
            discrepancies += 1;
        }
        accept(&e)?;
        hash.update(&e.bytes);
        at += e.bytes.len() as u64;
        entries += 1;
        next = n.checked_add(1).context("block number overflow")?;
        previous = Some(e.id);
        if at >= progress {
            eprintln!(
                "recover-tail [{phase}]: checked {} of {}",
                crate::report::bytes(at),
                crate::report::bytes(size)
            );
            progress = at.saturating_add(1 << 30);
        }
    }
    // Include unclassified bytes in the input fingerprint; they remain in the original.
    let physical_prefix_sha256 = hex::encode(hash.clone().finalize());
    let excluded_suffix_sha256 = hash_suffix(disk, at, size, &mut hash)?;
    let index_hashes = index
        .map(|disk| -> Result<_> {
            let mut hash = Sha256::new();
            let cut = (entries * 8).min(index_bytes);
            hash_range(disk, 0, cut, &mut hash)?;
            let prefix = hex::encode(hash.clone().finalize());
            let suffix = hash_suffix(disk, cut, index_bytes, &mut hash)?;
            Ok((hex::encode(hash.finalize()), prefix, suffix))
        })
        .transpose()?;
    eprintln!("recover-tail [{phase}]: finished {} entries", entries);
    let slots = index_bytes.div_ceil(8);
    discrepancies += slots.saturating_sub(entries);
    let indexed_head_hint = if slots == 0 {
        None
    } else {
        u64::from(begin.first)
            .checked_add(slots - 1)
            .filter(|n| *n < u64::from(u32::MAX))
            .map(|n| n as u32)
    };
    Ok(Summary {
        first_block: begin.first,
        last_block: (entries > 0).then_some(next - 1),
        entries,
        physical_prefix_end: at,
        physical_prefix_sha256,
        excluded_suffix_sha256,
        excluded_suffix_bytes: size - at,
        omitted_fork_entries: orphans,
        reference_checked_entries: compared,
        input_sha256: hex::encode(hash.finalize()),
        reference_evidence_sha256: hex::encode(evidence.finalize()),
        index_present: index.is_some(),
        index_bytes,
        index_partial_bytes: index_bytes % 8,
        index_discrepancies: discrepancies,
        index_sha256: index_hashes.as_ref().map(|h| h.0.clone()),
        index_prefix_sha256: index_hashes.as_ref().map(|h| h.1.clone()),
        index_suffix_sha256: index_hashes.map(|h| h.2),
        indexed_head_hint,
        failure,
    })
}

/// Compare decompressed payloads: trusted donors may use different zlib encodings.
fn ship_donor(
    n: u32,
    canonical_id: &[u8; 32],
    donors: &[Log],
) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut found: Option<(Vec<u8>, Vec<u8>)> = None;
    for donor in donors.iter().filter(|d| (d.first..=d.last).contains(&n)) {
        let body = donor.body(n)?;
        let (id, raw) = ship_payload(&body)?;
        ensure!(
            &id == canonical_id,
            "SHIP donor disagrees with canonical block {n}"
        );
        match &found {
            Some((_, seen)) => ensure!(*seen == raw, "SHIP donor payloads disagree at block {n}"),
            None => found = Some((body, raw)),
        }
    }
    Ok(found)
}

fn check_prefix_donors(entry: &Entry, kind: Kind, donors: &[Log]) -> Result<()> {
    if kind != Kind::Blocks
        && !donors.is_empty()
        && let Some((_, raw)) = ship_donor(entry.number(), &entry.id, donors)?
    {
        ensure!(
            ship_payload(entry.body())?.1 == raw,
            "target SHIP payload differs from trusted donors at block {}",
            entry.number()
        );
    }
    Ok(())
}

fn restoration(
    summary: &Summary,
    through: Option<u32>,
    kind: Kind,
    refs: Option<&Loaded>,
    donors: &[Log],
) -> Result<Vec<Vec<u8>>> {
    let last = summary
        .last_block
        .context("restoration requires at least one complete anchor entry")?;
    let through = through.context("pass --through-block with the intended final block; a torn file cannot prove how many entries are missing")?;
    ensure!(
        through >= last,
        "--through-block precedes the complete prefix"
    );
    ensure!(
        summary.indexed_head_hint.is_none_or(|head| through >= head),
        "--through-block precedes the indexed head hint; cannot call this full restoration"
    );
    ensure!(
        through - last <= crate::format::MAX_BLOCKS,
        "restoration exceeds 10,000 entries"
    );
    let mut previous = None;
    // The caller validates the first restored block against the original left anchor.
    let mut bytes = 0u64;
    let mut out = Vec::new();
    for n in last + 1..=through {
        let block = refs
            .context("restoration needs trusted block sources (--blocks-log or --blocks)")?
            .block(n)?
            .with_context(|| format!("block sources lack block {n}"))?;
        let h = full_block(&block)?;
        ensure!(h.number == n, "reference block number mismatch");
        if let Some(id) = previous {
            ensure!(
                h.previous == id,
                "restoration sources do not form one ID chain"
            );
        }
        previous = Some(h.id);
        let body = if kind == Kind::Blocks {
            block
        } else {
            ship_donor(n, &h.id, donors)?
                .with_context(|| format!("no SHIP donor covers block {n}"))?
                .0
        };
        bytes += body.len() as u64 + 8;
        ensure!(
            bytes <= MAX_BYTES,
            "restored tail exceeds 256 MiB safety limit"
        );
        out.push(body);
    }
    Ok(out)
}

struct Output {
    log: BufWriter<File>,
    index: BufWriter<File>,
    at: u64,
    entries: u64,
    log_hash: Sha256,
    index_hash: Sha256,
}
impl Output {
    fn new(dir: &Path, kind: Kind, header: &[u8]) -> Result<Self> {
        io::create_dir(dir)?;
        let open = |extension| -> Result<BufWriter<File>> {
            Ok(BufWriter::new(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dir.join(format!("{}.{extension}", kind.stem())))?,
            ))
        };
        let mut log = open("log")?;
        log.write_all(header)?;
        let mut log_hash = Sha256::new();
        log_hash.update(header);
        Ok(Self {
            log,
            index: open("index")?,
            at: header.len() as u64,
            entries: 0,
            log_hash,
            index_hash: Sha256::new(),
        })
    }
    fn append(&mut self, body: &[u8]) -> Result<()> {
        self.index.write_all(&self.at.to_le_bytes())?;
        self.log.write_all(body)?;
        self.log.write_all(&self.at.to_le_bytes())?;
        self.log_hash.update(body);
        self.log_hash.update(self.at.to_le_bytes());
        self.index_hash.update(self.at.to_le_bytes());
        self.at += body.len() as u64 + 8;
        self.entries += 1;
        Ok(())
    }
    fn finish(mut self, dir: &Path) -> Result<(String, String)> {
        self.log.flush()?;
        self.index.flush()?;
        self.log.get_ref().sync_all()?;
        self.index.get_ref().sync_all()?;
        let hashes = (
            hex::encode(self.log_hash.finalize()),
            hex::encode(self.index_hash.finalize()),
        );
        io::sync_dir(dir)?;
        Ok(hashes)
    }
}

pub fn run(options: Options<'_>) -> Result<Out> {
    let Options {
        log: path,
        index,
        sources,
        through,
        mode,
        out,
        node_stopped,
    } = options;
    ensure!(
        out.is_none() || node_stopped,
        "creating recovered files requires --node-stopped; stop the target node and every writer first"
    );
    let kind = Kind::from_path(path)?;
    let target = Pin::new(path, false)?;
    let disk = target.disk.as_ref().unwrap();
    let index = Pin::new(
        &index
            .map(Path::to_path_buf)
            .unwrap_or_else(|| disk.path.with_extension("index")),
        true,
    )?;
    let begin = start(disk, kind)?;
    let sources = sources.canonicalize()?;
    let mut pins = Vec::new();
    let mut pin_source = |path: &Path| -> Result<()> {
        let pin = Pin::new(path, false)?;
        let identity = same_file::Handle::from_file(pin.disk.as_ref().unwrap().file.try_clone()?)?;
        ensure!(
            identity != same_file::Handle::from_file(disk.file.try_clone()?)?,
            "the target cannot be its own recovery source"
        );
        if let Some(index) = &index.disk {
            ensure!(
                identity != same_file::Handle::from_file(index.file.try_clone()?)?,
                "the target index cannot be a recovery source"
            );
        }
        pins.push((path.to_path_buf(), pin));
        Ok(())
    };
    for p in &sources.blocks {
        pin_source(p)?;
    }
    for f in sources.block_logs.iter().chain(&sources.donors) {
        pin_source(&f.log)?;
        pin_source(
            &f.index
                .clone()
                .unwrap_or_else(|| f.log.with_extension("index")),
        )?;
    }
    let block_sources = Sources {
        blocks: sources.blocks.clone(),
        block_logs: sources.block_logs.clone(),
        donors: Vec::new(),
    };
    let refs = if block_sources.blocks.is_empty() && block_sources.block_logs.is_empty() {
        None
    } else {
        Some(block_sources.load_with(Kind::Blocks, |p| pinned_disk(&pins, p))?)
    };
    if let Some(chain_id) = begin.chain_id {
        for f in &block_sources.block_logs {
            let source = pinned_disk(&pins, &f.log)?;
            ensure!(
                start(&source, Kind::Blocks)?.chain_id == Some(chain_id),
                "block source belongs to another chain"
            );
        }
    }
    let donors = sources
        .donors
        .iter()
        .map(|f| {
            let donor = Log::from_disks(
                Kind::from_path(&f.log)?,
                pinned_disk(&pins, &f.log)?,
                pinned_disk(
                    &pins,
                    &f.index
                        .clone()
                        .unwrap_or_else(|| f.log.with_extension("index")),
                )?,
            )?;
            ensure!(
                kind != Kind::Blocks && donor.kind == kind,
                "SHIP donor kind differs from target"
            );
            Ok(donor)
        })
        .collect::<Result<Vec<_>>>()?;
    let check_inputs = || -> Result<()> {
        target.check()?;
        index.check()?;
        for (_, pin) in &pins {
            pin.check()?;
        }
        Ok(())
    };
    let mut left_anchor = None;
    let summary = walk(
        disk,
        index.disk.as_ref(),
        kind,
        &begin,
        refs.as_ref(),
        if out.is_some() {
            "inspect original (1/3)"
        } else {
            "inspect original (1/1)"
        },
        |e| {
            check_prefix_donors(e, kind, &donors)?;
            left_anchor = Some(e.id);
            Ok(())
        },
    )?;
    ensure!(
        through.is_none_or(|n| n >= summary.last_block.unwrap_or(summary.first_block)),
        "--through-block precedes the complete prefix or first block"
    );
    let canonical = kind == Kind::Blocks
        || summary.reference_checked_entries == summary.entries && refs.is_some();
    let restored = restoration(&summary, through, kind, refs.as_ref(), &donors).and_then(|tail| {
        ensure!(
            canonical,
            "SHIP recovery requires canonical block evidence for the complete prefix"
        );
        if kind == Kind::Blocks
            && let Some(first) = tail.first()
        {
            ensure!(
                Some(full_block(first)?.previous) == left_anchor,
                "first restored block does not link to the complete prefix"
            );
        }
        if kind != Kind::Blocks && !tail.is_empty() {
            let last = summary.last_block.unwrap();
            let block = refs
                .as_ref()
                .unwrap()
                .block(last)?
                .context("block sources lack the left anchor")?;
            ensure!(
                full_block(&block)?.id == left_anchor.unwrap(),
                "SHIP left anchor differs from canonical block evidence"
            );
            let next = refs
                .as_ref()
                .unwrap()
                .block(last + 1)?
                .context("block sources lack the next block")?;
            ensure!(
                full_block(&next)?.previous == left_anchor.unwrap(),
                "first restored block does not link to the complete prefix"
            );
        }
        Ok(tail)
    });
    let restore_reason = restored.as_ref().err().map(|e| format!("{e:#}"));
    let salvage_available = summary.entries > 0 && canonical;
    let mut report = json!({
        "ok": true, "format_version": 1, "operation": "recover_tail", "kind": kind,
        "tool_version": env!("CARGO_PKG_VERSION"), "sources": sources,
        "source_inputs": pins.iter().map(|(_, p)| json!({ "path": p.path, "stamp": p.stamp })).collect::<Vec<_>>(),
        "restored_tail_sha256": restored.as_ref().ok().map(|tail| {
            let mut hash = Sha256::new();
            for body in tail { hash.update((body.len() as u64).to_le_bytes()); hash.update(body); }
            hex::encode(hash.finalize())
        }),
        "target": disk.path, "target_stamp": target.stamp,
        "index": index.path, "index_stamp": index.stamp,
        "chain_id": begin.chain_id.map(hex::encode), "prefix": {
            "first_block": summary.first_block, "last_block": summary.last_block, "entries": summary.entries,
            "physical_end": summary.physical_prefix_end,
        },
        "failure": summary.failure,
        "inspection": summary,
        "through_block": through,
        "options": {
            "restore": { "available": restored.is_ok(), "reason": restore_reason, "through_block": through },
            "salvage": { "available": salvage_available, "last_block": summary.last_block,
                "excluded_suffix_bytes": summary.excluded_suffix_bytes,
                "reason": if !canonical { Some("SHIP output requires --blocks-log or --blocks covering the canonical prefix") } else if summary.entries == 0 { Some("no complete anchor entry") } else { None },
                "missing_block_range": through.and_then(|end| summary.last_block.filter(|last| *last < end).map(|last| [last + 1, end])),
                "missing_indexed_block_range": summary.indexed_head_hint.and_then(|end| summary.last_block.filter(|last| *last < end).map(|last| [last + 1, end])),
                "total_missing_history_known": through.is_some_and(|end| summary.indexed_head_hint.is_none_or(|hint| end >= hint)),
            },
        },
        "preferred_mode": "restore", "selected_mode": mode,
        "validation": "Whole signed_block framing, position trailers, header ID links; SHIP payload sizes, zlib checksums and canonical IDs when references are supplied. No signature or Merkle-root validation. SHIP payloads rely on the operator's trusted donor.",
        "activation": "Originals are never modified. Output is staged only; review complete.json and coordinate log/index activation with nodeos stopped.",
    });
    check_inputs()?;
    let mut text = match summary.last_block {
        Some(last) => format!(
            "Complete prefix: blocks {}..{} ({} entries).\n",
            summary.first_block, last, summary.entries
        ),
        None => "No complete anchor entry.\n".to_string(),
    };
    if let Some(f) = &summary.failure {
        text.push_str(&format!(
            "Stopped at byte {} before block {}: {}. Remaining bytes are unclassified.\n",
            f.offset, f.next_block, f.reason
        ));
    }
    text.push_str(&format!("Index: {} discrepancies, {} partial bytes.\nRestore (preferred): {}.\nSalvage: {} ({} suffix bytes excluded; originals preserved).\n", summary.index_discrepancies, summary.index_partial_bytes,
        restore_reason.as_deref().unwrap_or("available through the declared final block"), if salvage_available { "available; select --mode salvage explicitly" } else { "requires a complete canonical prefix" }, summary.excluded_suffix_bytes));
    if let Some(last) = summary.last_block
        && let Some(end) = through.filter(|end| *end > last)
    {
        text.push_str(&format!(
            "Salvage leaves declared blocks {}..{} unavailable.\n",
            last + 1,
            end
        ));
    }
    if through.is_none() {
        text.push_str("The original intended head is unknown; excluded bytes do not establish the total missing history.\n");
    }
    let Some(out) = out else {
        return Ok(Out { json: report, text });
    };
    let tail = match mode {
        Mode::Restore => restored.context(
            "restoration unavailable; no output created and no automatic fallback to salvage",
        )?,
        Mode::Salvage => {
            ensure!(salvage_available, "salvage unavailable; no output created");
            Vec::new()
        }
    };
    let desired_last = match mode {
        Mode::Restore => through,
        Mode::Salvage => summary.last_block,
    };
    let needed = summary
        .physical_prefix_end
        .saturating_add(tail.iter().map(|b| b.len() as u64 + 16).sum::<u64>())
        .saturating_add(summary.entries * 8)
        .saturating_add(1 << 20);
    ensure!(
        fs2::available_space(io::parent_dir(out))? >= needed,
        "insufficient space for a separate recovered log and index (need at least {} bytes)",
        needed
    );
    let plan = serde_json::to_vec_pretty(&report)?;
    io::create_dir(out)?;
    io::create_file(&out.join("recovery.json"), &plan)?;
    let recovered = out.join("recovered");
    let mut output = Output::new(&recovered, kind, &begin.bytes)?;
    let again = walk(
        disk,
        index.disk.as_ref(),
        kind,
        &begin,
        refs.as_ref(),
        "write recovered pair (2/3)",
        |e| {
            check_prefix_donors(e, kind, &donors)?;
            output.append(e.body())
        },
    )?;
    ensure!(
        again == summary,
        "input content or canonical evidence changed during recovery; incomplete output is kept without complete.json"
    );
    for body in &tail {
        output.append(body)?;
    }
    let (expected_log_hash, expected_index_hash) = output.finish(&recovered)?;
    check_inputs()?;
    // Read back through the same whole-entry parser, using the generated index.
    let result_log = Disk::open(&recovered.join(format!("{}.log", kind.stem())), false)?;
    let result_index = Disk::open(&recovered.join(format!("{}.index", kind.stem())), false)?;
    let readback = walk(
        &result_log,
        Some(&result_index),
        kind,
        &start(&result_log, kind)?,
        refs.as_ref(),
        "verify recovered pair (3/3)",
        |e| check_prefix_donors(e, kind, &donors),
    )?;
    ensure!(
        readback.failure.is_none()
            && readback.index_discrepancies == 0
            && readback.last_block == desired_last,
        "recovered output failed read-back verification; incomplete output is kept without complete.json"
    );
    ensure!(
        readback.input_sha256 == expected_log_hash
            && readback.index_sha256.as_ref() == Some(&expected_index_hash),
        "recovered output differs from the bytes written; incomplete output is kept without complete.json"
    );
    check_inputs()?;
    report["recovery_sha256"] = json!(io::hash(&plan));
    report["output"] = json!({ "directory": recovered, "last_block": readback.last_block, "entries": readback.entries, "log_sha256": readback.input_sha256, "index_sha256": readback.index_sha256 });
    io::create_file(
        &out.join("complete.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    io::sync_dir(out)?;
    text.push_str(&format!(
        "Staged verified output in {}. Review {}; activation is manual.\n",
        recovered.display(),
        out.join("complete.json").display()
    ));
    Ok(Out { json: report, text })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{block_header, u64le};
    use std::fs;
    use tempfile::TempDir;

    pub(super) struct Fixture {
        pub(super) dir: TempDir,
        pub(super) log: std::path::PathBuf,
        pub(super) offsets: Vec<u64>,
        pub(super) bytes: Vec<u8>,
    }

    impl Fixture {
        pub(super) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let log = dir.path().join("blocks.log");
            let mut bytes = 3u32.to_le_bytes().to_vec();
            bytes.extend_from_slice(&1000u32.to_le_bytes());
            bytes.extend_from_slice(&[34; 32]);
            bytes.extend_from_slice(&[255; 8]);
            let mut previous = [17; 32];
            previous[..4].copy_from_slice(&999u32.to_be_bytes());
            let mut offsets = Vec::new();
            for n in 1000..1004u32 {
                let pos = bytes.len() as u64;
                offsets.push(pos);
                let mut body = vec![0; 14];
                body.extend_from_slice(&previous);
                body.extend_from_slice(&[n as u8; 64]);
                body.extend_from_slice(&1u32.to_le_bytes());
                body.extend_from_slice(&[0, 0]); // optional producers, header extensions
                previous = block_header(&body).unwrap().id;
                body.extend_from_slice(&[0; 66]); // K1 producer signature
                body.extend_from_slice(&[0, 0]); // receipts, block extensions
                bytes.extend_from_slice(&body);
                bytes.extend_from_slice(&pos.to_le_bytes());
            }
            fs::write(&log, &bytes).unwrap();
            fs::write(
                log.with_extension("index"),
                offsets
                    .iter()
                    .flat_map(|p| p.to_le_bytes())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            Self {
                dir,
                log,
                offsets,
                bytes,
            }
        }

        fn run(&self, sources: &Sources, mode: Mode, out: Option<&Path>) -> Result<Out> {
            run(Options {
                log: &self.log,
                index: None,
                sources,
                through: Some(1003),
                mode,
                out,
                node_stopped: true,
            })
        }

        fn donor(&self, name: &str) -> std::path::PathBuf {
            let dir = self.dir.path().join(name);
            fs::create_dir(&dir).unwrap();
            let p = dir.join(self.log.file_name().unwrap());
            fs::copy(&self.log, &p).unwrap();
            fs::copy(self.log.with_extension("index"), p.with_extension("index")).unwrap();
            p
        }

        fn block_source(&self, p: std::path::PathBuf) -> Sources {
            Sources {
                block_logs: vec![crate::repair::LogFile {
                    log: p,
                    index: None,
                }],
                ..Sources::default()
            }
        }
    }

    #[test]
    fn source_loading_uses_pinned_handles_during_path_replacement() {
        let f = Fixture::new();
        let donor = f.donor("donor").canonicalize().unwrap();
        let index = donor.with_extension("index");
        let container = f.dir.path().join("blocks.bin");
        let mut blocks = crate::format::Blocks::new();
        for i in 0..4 {
            blocks.insert(
                1000 + i as u32,
                f.bytes[f.offsets[i] as usize..f.offsets[i] as usize + 184].to_vec(),
            );
        }
        fs::write(
            &container,
            crate::format::encode_container(&blocks).unwrap(),
        )
        .unwrap();
        let sources = Sources {
            blocks: vec![container],
            block_logs: vec![crate::repair::LogFile {
                log: donor,
                index: None,
            }],
            ..Sources::default()
        }
        .canonicalize()
        .unwrap();
        let paths = [&sources.blocks[0], &sources.block_logs[0].log, &index];
        let pins = paths
            .iter()
            .map(|p| ((*p).clone(), Pin::new(p, false).unwrap()))
            .collect::<Vec<_>>();
        let held = |path: &Path| {
            let mut name = path.as_os_str().to_os_string();
            name.push(".held");
            PathBuf::from(name)
        };
        for path in paths {
            fs::rename(path, held(path)).unwrap();
            fs::write(path, b"replacement, not the pinned input").unwrap();
        }
        assert!(sources.load(Kind::Blocks).is_err());
        let loaded = sources
            .load_with(Kind::Blocks, |p| pinned_disk(&pins, p))
            .unwrap();
        for path in paths {
            fs::remove_file(path).unwrap();
            fs::rename(held(path), path).unwrap();
        }
        for (_, pin) in &pins {
            pin.check().unwrap();
        }
        for (n, body) in blocks {
            assert_eq!(loaded.block(n).unwrap().unwrap(), body);
        }
    }

    #[test]
    fn index_read_ahead_crosses_pages_and_refuses_partial_slots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blocks.index");
        let mut bytes = (0u64..20_000)
            .flat_map(|n| (n * 192 + 48).to_le_bytes())
            .collect::<Vec<_>>();
        bytes.extend_from_slice(&[1, 2, 3]);
        fs::write(&path, &bytes).unwrap();
        let disk = Disk::open(&path, false).unwrap();
        let mut reader = ReadAhead {
            disk: &disk,
            offset: 0,
            bytes: Vec::new(),
        };
        for n in 0..20_000 {
            assert_eq!(
                reader.index_position(n * 8, bytes.len() as u64).unwrap(),
                n * 192 + 48
            );
        }
        assert!(
            reader
                .index_position(20_000 * 8, bytes.len() as u64)
                .is_err()
        );
        assert_eq!(reader.index_position(0, bytes.len() as u64).unwrap(), 48);
        assert!(reader.bytes.len() <= 64 << 10);
    }

    #[test]
    fn changed_preamble_is_detected_even_with_restored_metadata() {
        let f = Fixture::new();
        let pin = Pin::new(&f.log, false).unwrap();
        let disk = pin.disk.as_ref().unwrap();
        let begin = start(disk, Kind::Blocks).unwrap();
        let before = walk(disk, None, Kind::Blocks, &begin, None, "test", |_| Ok(())).unwrap();
        assert_eq!(before.input_sha256, disk_hash(disk).unwrap());
        let modified = disk.file.metadata().unwrap().modified().unwrap();
        let mut changed = f.bytes.clone();
        changed[8] ^= 1;
        fs::write(&f.log, &changed).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&f.log)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        pin.check().unwrap();
        assert!(walk(disk, None, Kind::Blocks, &begin, None, "test", |_| Ok(())).is_err());
    }

    #[test]
    fn truncated_body_reports_both_options_without_an_intact_index() {
        let f = Fixture::new();
        let cut = f.offsets[3] as usize + 30;
        fs::write(&f.log, &f.bytes[..cut]).unwrap();
        fs::remove_file(f.log.with_extension("index")).unwrap();
        let out = f.run(&Sources::default(), Mode::Restore, None).unwrap();
        assert_eq!(out.json["prefix"]["last_block"], 1002);
        assert_eq!(out.json["failure"]["offset"], f.offsets[3]);
        assert_eq!(out.json["options"]["restore"]["available"], false);
        assert_eq!(out.json["options"]["salvage"]["available"], true);
        assert_eq!(fs::read(&f.log).unwrap(), f.bytes[..cut]);
    }

    #[test]
    fn restore_prefers_a_donor_and_writes_a_consistent_new_pair() {
        let f = Fixture::new();
        let donor = f.dir.path().join("donor");
        fs::create_dir(&donor).unwrap();
        let donor_log = donor.join("blocks.log");
        fs::copy(&f.log, &donor_log).unwrap();
        fs::copy(
            f.log.with_extension("index"),
            donor_log.with_extension("index"),
        )
        .unwrap();
        let cut = f.offsets[3] as usize + 30;
        fs::write(&f.log, &f.bytes[..cut]).unwrap();
        let sources = Sources {
            block_logs: vec![crate::repair::LogFile {
                log: donor_log,
                index: None,
            }],
            ..Sources::default()
        };
        let dest = f.dir.path().join("recovery");
        let out = f.run(&sources, Mode::Restore, Some(&dest)).unwrap();
        assert_eq!(out.json["selected_mode"], "restore");
        assert_eq!(
            fs::read(dest.join("recovered/blocks.log")).unwrap(),
            f.bytes
        );
        let idx = fs::read(dest.join("recovered/blocks.index")).unwrap();
        assert_eq!(u64le(&idx[24..]), f.offsets[3]);
        assert!(dest.join("complete.json").is_file());
        assert_eq!(fs::read(&f.log).unwrap(), f.bytes[..cut]);
    }

    #[test]
    fn every_cut_of_the_last_signed_block_can_be_restored_or_explicitly_salvaged() {
        let f = Fixture::new();
        let sources = f.block_source(f.donor("donor"));
        let last = f.offsets[3] as usize;
        for cut in last..f.bytes.len() {
            fs::write(&f.log, &f.bytes[..cut]).unwrap();
            let restore = f.dir.path().join(format!("restore-{cut}"));
            let restored = f.run(&sources, Mode::Restore, Some(&restore)).unwrap();
            assert_eq!(restored.json["output"]["last_block"], 1003, "cut {cut}");
            assert_eq!(
                fs::read(restore.join("recovered/blocks.log")).unwrap(),
                f.bytes,
                "cut {cut}"
            );
            let salvage = f.dir.path().join(format!("salvage-{cut}"));
            let salvaged = f.run(&sources, Mode::Salvage, Some(&salvage)).unwrap();
            assert_eq!(salvaged.json["output"]["last_block"], 1002, "cut {cut}");
            assert_eq!(
                fs::read(salvage.join("recovered/blocks.log")).unwrap(),
                f.bytes[..last]
            );
            assert_eq!(fs::read(&f.log).unwrap(), f.bytes[..cut]);
        }
    }

    #[test]
    fn missing_torn_ahead_behind_and_wrong_indexes_are_rebuilt_from_entries() {
        let f = Fixture::new();
        let original = fs::read(f.log.with_extension("index")).unwrap();
        let mut wrong = original.clone();
        wrong[8..16].copy_from_slice(&0u64.to_le_bytes());
        let variants = [
            None,
            Some(Vec::new()),
            Some(original[..original.len() - 3].to_vec()),
            Some(original[..original.len() - 8].to_vec()),
            Some([original.clone(), vec![0; 8]].concat()),
            Some(wrong),
        ];
        for (i, idx) in variants.into_iter().enumerate() {
            let p = f.log.with_extension("index");
            if let Some(bytes) = idx {
                fs::write(&p, bytes).unwrap();
            } else {
                fs::remove_file(&p).unwrap();
            }
            // The extra index slot is a hint of additional lost history: restoration must
            // not claim it is complete through 1003. Explicit salvage still rebuilds 1000..1003.
            let mode = if i == 4 { Mode::Salvage } else { Mode::Restore };
            let out = f.dir.path().join(format!("index-{i}"));
            let report = f.run(&Sources::default(), mode, Some(&out)).unwrap();
            assert!(
                report.json["inspection"]["index_discrepancies"]
                    .as_u64()
                    .unwrap()
                    > 0
            );
            assert_eq!(
                fs::read(out.join("recovered/blocks.index")).unwrap(),
                original
            );
            assert_eq!(fs::read(out.join("recovered/blocks.log")).unwrap(), f.bytes);
        }
    }

    #[test]
    fn restoration_never_falls_back_to_salvage_or_guesses_the_intended_head() {
        let f = Fixture::new();
        fs::write(&f.log, &f.bytes[..f.bytes.len() - 20]).unwrap();
        let dest = f.dir.path().join("out");
        assert!(
            f.run(&Sources::default(), Mode::Restore, Some(&dest))
                .is_err()
        );
        assert!(!dest.exists());
        let report = run(Options {
            log: &f.log,
            index: None,
            sources: &Sources::default(),
            through: None,
            mode: Mode::Restore,
            out: None,
            node_stopped: false,
        })
        .unwrap();
        assert_eq!(report.json["options"]["restore"]["available"], false);
        assert_eq!(
            report.json["options"]["salvage"]["total_missing_history_known"],
            false
        );
        assert!(
            run(Options {
                log: &f.log,
                index: None,
                sources: &Sources::default(),
                through: Some(1003),
                mode: Mode::Salvage,
                out: Some(&dest),
                node_stopped: false
            })
            .is_err()
        );
        assert!(!dest.exists());
    }

    #[test]
    fn signed_blocks_larger_than_read_ahead_are_kept_whole() {
        let f = Fixture::new();
        let last = f.offsets[3] as usize;
        let mut bytes = f.bytes[..last + 182].to_vec(); // header and producer signature
        bytes.extend([1, 0, 0, 0, 0, 0, 0, 1]); // one executed packed receipt
        bytes.extend([0, 0, 0]); // no signatures, uncompressed, no context-free data
        let mut n = 200_000u32;
        while n >= 128 {
            bytes.push((n as u8 & 127) | 128);
            n >>= 7;
        }
        bytes.push(n as u8);
        bytes.extend(vec![7; 200_000]);
        bytes.push(0); // block extensions
        bytes.extend((last as u64).to_le_bytes());
        fs::write(&f.log, &bytes).unwrap();
        let dest = f.dir.path().join("large");
        let report = f
            .run(&Sources::default(), Mode::Restore, Some(&dest))
            .unwrap();
        assert_eq!(report.json["output"]["last_block"], 1003);
        assert_eq!(fs::read(dest.join("recovered/blocks.log")).unwrap(), bytes);
    }

    #[test]
    fn readable_damage_and_zero_suffixes_stop_at_the_first_invalid_entry() {
        let f = Fixture::new();
        for (name, data, expected) in [
            ("zero", [f.bytes.clone(), vec![0; 512]].concat(), 1003),
            (
                "trailer",
                {
                    let mut b = f.bytes.clone();
                    *b.last_mut().unwrap() ^= 1;
                    b
                },
                1002,
            ),
            (
                "interior",
                {
                    let mut b = f.bytes.clone();
                    b[f.offsets[1] as usize + 114] = 3;
                    b
                },
                1000,
            ),
        ] {
            fs::write(&f.log, &data).unwrap();
            let dest = f.dir.path().join(name);
            let report = f
                .run(&Sources::default(), Mode::Salvage, Some(&dest))
                .unwrap();
            assert_eq!(report.json["output"]["last_block"], expected);
            assert!(report.json["failure"].is_object());
            assert_eq!(fs::read(&f.log).unwrap(), data);
        }
    }

    #[test]
    fn source_alias_wrong_chain_conflicts_and_broken_anchor_cannot_restore() {
        let f = Fixture::new();
        assert!(
            f.run(&f.block_source(f.log.clone()), Mode::Restore, None)
                .is_err()
        );
        let a = f.donor("a");
        let b = f.donor("b");
        let last = f.offsets[3] as usize;
        fs::write(&f.log, &f.bytes[..last + 30]).unwrap();
        let mut wrong_chain = f.bytes.clone();
        wrong_chain[8] ^= 1;
        fs::write(&a, wrong_chain).unwrap();
        assert!(
            f.run(&f.block_source(a.clone()), Mode::Restore, None)
                .is_err()
        );
        let mut broken = f.bytes.clone();
        broken[last + 20] ^= 1; // same predecessor number, different predecessor ID
        fs::write(&a, &broken).unwrap();
        let report = f
            .run(&f.block_source(a.clone()), Mode::Restore, None)
            .unwrap();
        assert_eq!(report.json["options"]["restore"]["available"], false);
        let mut sources = f.block_source(a);
        sources.block_logs.push(crate::repair::LogFile {
            log: b,
            index: None,
        });
        let report = f.run(&sources, Mode::Restore, None).unwrap();
        assert!(
            report.json["options"]["restore"]["reason"]
                .as_str()
                .unwrap()
                .contains("disagree")
        );
    }

    #[test]
    fn no_anchor_unsupported_formats_existing_output_and_input_changes_are_refused() {
        let f = Fixture::new();
        let dest = f.dir.path().join("out");
        fs::create_dir(&dest).unwrap();
        fs::write(dest.join("preserve"), b"unrelated").unwrap();
        assert!(
            f.run(&Sources::default(), Mode::Salvage, Some(&dest))
                .is_err()
        );
        assert_eq!(fs::read(dest.join("preserve")).unwrap(), b"unrelated");
        fs::write(&f.log, &f.bytes[..48]).unwrap();
        let report = f.run(&Sources::default(), Mode::Restore, None).unwrap();
        assert_eq!(report.json["options"]["restore"]["available"], false);
        assert_eq!(report.json["options"]["salvage"]["available"], false);
        let pin = Pin::new(&f.log, false).unwrap();
        fs::write(&f.log, &f.bytes).unwrap();
        assert!(pin.check().is_err());
        let mut pruned = f.bytes.clone();
        pruned[..4].copy_from_slice(&(3u32 | 1 << 31).to_le_bytes());
        fs::write(&f.log, pruned).unwrap();
        assert!(f.run(&Sources::default(), Mode::Restore, None).is_err());
    }

    struct ShipFixture {
        blocks: Fixture,
        log: PathBuf,
        bytes: Vec<u8>,
        offsets: Vec<u64>,
        sources: Sources,
    }
    impl ShipFixture {
        fn new(kind: Kind, fork: bool) -> Self {
            let blocks = Fixture::new();
            let log = blocks.dir.path().join(format!("{}.log", kind.stem()));
            let mut bytes = Vec::new();
            let mut offsets = Vec::new();
            for i in 0..4 {
                let h = full_block(
                    &blocks.bytes[blocks.offsets[i] as usize..blocks.offsets[i] as usize + 184],
                )
                .unwrap();
                if fork && i == 1 {
                    let mut orphan = h.id;
                    orphan[31] ^= 1;
                    let at = bytes.len() as u64;
                    bytes.extend(crate::format::encode_ship(&orphan, &[0, 33], 1).unwrap());
                    bytes.extend(at.to_le_bytes());
                }
                let at = bytes.len() as u64;
                offsets.push(at);
                bytes.extend(crate::format::encode_ship(&h.id, &[0, i as u8], 1).unwrap());
                bytes.extend(at.to_le_bytes());
            }
            fs::write(&log, &bytes).unwrap();
            fs::write(
                log.with_extension("index"),
                offsets
                    .iter()
                    .flat_map(|p| p.to_le_bytes())
                    .collect::<Vec<_>>(),
            )
            .unwrap();
            let donor_dir = blocks.dir.path().join("ship-donor");
            fs::create_dir(&donor_dir).unwrap();
            let donor = donor_dir.join(format!("{}.log", kind.stem()));
            fs::copy(&log, &donor).unwrap();
            fs::copy(log.with_extension("index"), donor.with_extension("index")).unwrap();
            let mut sources = blocks.block_source(blocks.log.clone());
            sources.donors.push(crate::repair::LogFile {
                log: donor,
                index: None,
            });
            Self {
                blocks,
                log,
                bytes,
                offsets,
                sources,
            }
        }
        fn run(&self, sources: &Sources, mode: Mode, out: Option<&Path>) -> Result<Out> {
            run(Options {
                log: &self.log,
                index: None,
                sources,
                through: Some(1003),
                mode,
                out,
                node_stopped: true,
            })
        }
    }

    #[test]
    fn every_cut_of_ship_headers_payloads_and_trailers_restores_with_canonical_evidence() {
        for kind in [Kind::Trace, Kind::ChainState] {
            let f = ShipFixture::new(kind, false);
            let last = f.offsets[3] as usize;
            for cut in last..f.bytes.len() {
                fs::write(&f.log, &f.bytes[..cut]).unwrap();
                let dest = f.blocks.dir.path().join(format!("out-{cut}"));
                let report = f.run(&f.sources, Mode::Restore, Some(&dest)).unwrap();
                assert_eq!(
                    report.json["output"]["last_block"], 1003,
                    "{kind:?} cut {cut}"
                );
                assert_eq!(
                    fs::read(dest.join(format!("recovered/{}.log", kind.stem()))).unwrap(),
                    f.bytes
                );
                assert_eq!(fs::read(&f.log).unwrap(), f.bytes[..cut]);
            }
        }
    }

    #[test]
    fn ship_stale_index_unindexed_tail_and_forks_are_selected_by_canonical_ids() {
        let f = ShipFixture::new(Kind::Trace, true);
        let idx = fs::read(f.log.with_extension("index")).unwrap();
        fs::write(f.log.with_extension("index"), &idx[..idx.len() - 8]).unwrap();
        let dest = f.blocks.dir.path().join("out");
        let report = f.run(&f.sources, Mode::Restore, Some(&dest)).unwrap();
        assert_eq!(report.json["output"]["entries"], 4);
        assert_eq!(report.json["inspection"]["omitted_fork_entries"], 1);
        assert!(
            report.json["inspection"]["index_discrepancies"]
                .as_u64()
                .unwrap()
                > 0
        );
        let recovered = Log::open(&dest.join("recovered/trace_history.log"), None, false).unwrap();
        for n in 1000..=1003 {
            assert_eq!(
                recovered.body(n).unwrap(),
                Log::open(
                    &f.log,
                    Some(&f.sources.donors[0].log.with_extension("index")),
                    false
                )
                .unwrap()
                .body(n)
                .unwrap()
            );
        }
        let no_refs = f.run(&Sources::default(), Mode::Salvage, None).unwrap();
        assert_eq!(no_refs.json["options"]["salvage"]["available"], false);
    }

    #[test]
    fn ship_donor_conflicts_in_the_retained_prefix_are_rejected() {
        let f = ShipFixture::new(Kind::Trace, false);
        let dir = f.blocks.dir.path().join("conflicting-prefix");
        fs::create_dir(&dir).unwrap();
        let second = dir.join("trace_history.log");
        let at = f.offsets[1] as usize;
        let end = f.offsets[2] as usize;
        let id = f.bytes[at + 8..at + 40].try_into().unwrap();
        let mut changed = f.bytes[..at].to_vec();
        changed.extend(crate::format::encode_ship(&id, &[0, 44], 1).unwrap());
        changed.extend((at as u64).to_le_bytes());
        assert_eq!(changed.len(), end);
        changed.extend_from_slice(&f.bytes[end..]);
        fs::write(&second, changed).unwrap();
        fs::copy(
            f.log.with_extension("index"),
            second.with_extension("index"),
        )
        .unwrap();
        fs::write(&f.log, &f.bytes[..f.offsets[3] as usize + 5]).unwrap();
        let mut sources = f.sources.clone();
        sources.donors.push(crate::repair::LogFile {
            log: second,
            index: None,
        });
        let dest = f.blocks.dir.path().join("out");
        assert!(f.run(&sources, Mode::Restore, None).is_err());
        assert!(f.run(&sources, Mode::Restore, Some(&dest)).is_err());
        assert!(!dest.exists());
    }

    #[test]
    fn ship_donor_payload_disagreement_and_bad_checksums_never_produce_a_receipt() {
        let f = ShipFixture::new(Kind::ChainState, false);
        let second_dir = f.blocks.dir.path().join("second");
        fs::create_dir(&second_dir).unwrap();
        let second = second_dir.join("chain_state_history.log");
        let last = f.offsets[3] as usize;
        let id: [u8; 32] = f.bytes[last + 8..last + 40].try_into().unwrap();
        let mut changed = f.bytes[..last].to_vec();
        changed.extend(crate::format::encode_ship(&id, &[0, 44], 1).unwrap());
        changed.extend((last as u64).to_le_bytes());
        fs::write(&second, changed).unwrap();
        fs::copy(
            f.log.with_extension("index"),
            second.with_extension("index"),
        )
        .unwrap();
        fs::write(&f.log, &f.bytes[..last + 5]).unwrap();
        let mut sources = f.sources.clone();
        sources.donors.push(crate::repair::LogFile {
            log: second,
            index: None,
        });
        let dest = f.blocks.dir.path().join("out");
        let report = f.run(&sources, Mode::Restore, None).unwrap();
        assert!(
            report.json["options"]["restore"]["reason"]
                .as_str()
                .unwrap()
                .contains("payloads disagree")
        );
        assert!(f.run(&sources, Mode::Restore, Some(&dest)).is_err());
        assert!(!dest.exists());
        let mut corrupt = f.bytes.clone();
        corrupt[f.bytes.len() - 10] ^= 1;
        fs::write(&f.log, &corrupt).unwrap();
        let report = f.run(&f.sources, Mode::Salvage, Some(&dest)).unwrap();
        assert_eq!(report.json["output"]["last_block"], 1002);
        assert_eq!(fs::read(&f.log).unwrap(), corrupt);
    }
}
