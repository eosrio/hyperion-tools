//! Chunked reads of a node's logs, and ordered work stealing over block ranges.
//!
//! The per-block readers issue several small reads for every block (index entries, the entry
//! header, the payload, the block log's index and body). With many threads on one file, those
//! reads contend on the filesystem's per-file range lock, and most of a walk's time goes to the
//! kernel. A slab is one block range of one log read with one positional read of its index slice
//! and one of its byte region. Blocks are then cut out of memory, by the index, so a superseded
//! fork entry between two canonical ones is never parsed.
//!
//! The files stay open for the life of the process and are re-measured on every read, so a log
//! that a running node appends to can be followed. Only index entries are trusted for positions:
//! a slab never reads past the entry its last block's index slot names.
//!
//! [`ordered_chunks`] hands fixed-size chunks of a range to worker threads, one at a time from a
//! shared cursor, and gives the results to one fold in block order. A worker may run at most
//! `window` chunks ahead of the fold, so a slow consumer bounds the memory instead of growing it.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex};

use anyhow::{Context, Result, bail, ensure};

/// State-history entry magic: the high 32 bits are the "ship" name, the low 16 the version.
const SHIP_MAGIC_MASK: u64 = 0xffff_ffff_0000_0000;
const SHIP_MAGIC: u64 = 0xc35d_5000_0000_0000;
/// A state-history entry header: magic, block id (whose first four bytes are the block number,
/// big endian) and the payload size.
pub const SHIP_HEADER: usize = 48;
/// Bound on one block's entry, so a corrupt index or header cannot allocate the whole log.
pub const MAX_ENTRY: u64 = 256 * 1024 * 1024;

#[cfg(unix)]
fn pread_exact(file: &File, off: u64, buf: &mut [u8]) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(buf, off)
}

#[cfg(windows)]
fn pread_exact(file: &File, mut off: u64, mut buf: &mut [u8]) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        match file.seek_read(buf, off) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => {
                buf = &mut buf[n..];
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// An open log and its `.index` (one little-endian `u64` entry position per block).
#[derive(Debug)]
pub struct IndexedFile {
    log: File,
    index: File,
    log_path: PathBuf,
    index_path: PathBuf,
    first_block: u32,
}

impl IndexedFile {
    fn open(log_path: PathBuf, index_path: PathBuf, first_block: u32) -> Result<Self> {
        let log = File::open(&log_path).with_context(|| format!("open {}", log_path.display()))?;
        let index =
            File::open(&index_path).with_context(|| format!("open {}", index_path.display()))?;
        Ok(Self {
            log,
            index,
            log_path,
            index_path,
            first_block,
        })
    }

    pub fn first_block(&self) -> u32 {
        self.first_block
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }

    /// The last block the index holds now, or `None` while it is empty.
    pub fn last_block(&self) -> Result<Option<u32>> {
        let entries = self.index.metadata()?.len() / 8;
        Ok(match entries {
            0 => None,
            n => {
                Some(u32::try_from(u64::from(self.first_block) + n - 1).context("index too long")?)
            }
        })
    }

    pub fn log_len(&self) -> Result<u64> {
        Ok(self.log.metadata()?.len())
    }

    /// Index slots for blocks `from..=to` (one read).
    pub fn positions(&self, from: u32, to: u32) -> Result<Vec<u64>> {
        ensure!(
            from >= self.first_block && to >= from,
            "blocks {from}..={to} start before {} begins at {}",
            self.index_path.display(),
            self.first_block
        );
        let count = (to - from) as usize + 1;
        let mut buf = vec![0u8; count * 8];
        pread_exact(
            &self.index,
            u64::from(from - self.first_block) * 8,
            &mut buf,
        )
        .with_context(|| {
            format!(
                "blocks {from}..={to} are past the end of {}",
                self.index_path.display()
            )
        })?;
        let (slots, _) = buf.as_chunks::<8>();
        Ok(slots.iter().map(|s| u64::from_le_bytes(*s)).collect())
    }

    fn read(&self, off: u64, len: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; usize::try_from(len)?];
        pread_exact(&self.log, off, &mut buf).with_context(|| {
            format!(
                "read {len} bytes at offset {off} of {}",
                self.log_path.display()
            )
        })?;
        Ok(buf)
    }
}

/// One state-history log (`trace_history` or `chain_state_history`).
#[derive(Debug)]
pub struct ShipFile(IndexedFile);

impl ShipFile {
    /// Open `<dir>/<stem>.{log,index}`. The first block is the first entry's block number.
    pub fn open(dir: &Path, stem: &str) -> Result<Self> {
        let log_path = dir.join(format!("{stem}.log"));
        let index_path = dir.join(format!("{stem}.index"));
        let log = File::open(&log_path).with_context(|| format!("open {}", log_path.display()))?;
        let mut header = [0u8; SHIP_HEADER];
        pread_exact(&log, 0, &mut header)
            .with_context(|| format!("read the first entry of {}", log_path.display()))?;
        let magic = u64::from_le_bytes(header[0..8].try_into().unwrap());
        ensure!(
            magic & SHIP_MAGIC_MASK == SHIP_MAGIC,
            "{} is not a state-history log",
            log_path.display()
        );
        let first = u32::from_be_bytes(header[8..12].try_into().unwrap());
        Ok(Self(IndexedFile::open(log_path, index_path, first)?))
    }

    pub fn file(&self) -> &IndexedFile {
        &self.0
    }

    pub fn first_block(&self) -> u32 {
        self.0.first_block
    }

    pub fn last_block(&self) -> Result<Option<u32>> {
        self.0.last_block()
    }

    /// Blocks `from..=to` in one slab: one read of their index slots, one of the last entry's
    /// header, and one of the region from the first entry to the end of the last.
    pub fn slab(&self, from: u32, to: u32) -> Result<ShipSlab> {
        let offsets = self.0.positions(from, to)?;
        for pair in offsets.windows(2) {
            ensure!(
                pair[1] > pair[0],
                "{} positions are not increasing",
                self.0.index_path.display()
            );
        }
        let base = offsets[0];
        let last = *offsets.last().unwrap();
        let mut header = [0u8; SHIP_HEADER];
        pread_exact(&self.0.log, last, &mut header).with_context(|| {
            format!(
                "read the entry header of block {to} in {}",
                self.0.log_path.display()
            )
        })?;
        let size = u64::from_le_bytes(header[40..48].try_into().unwrap());
        ensure!(
            size <= MAX_ENTRY,
            "block {to} in {} has a {size}-byte payload",
            self.0.log_path.display()
        );
        let end = last + SHIP_HEADER as u64 + size;
        let bytes = self.0.read(base, end - base)?;
        Ok(ShipSlab {
            first: from,
            base,
            offsets,
            bytes,
            path: self.0.log_path.clone(),
        })
    }
}

/// A block range of one state-history log in memory.
pub struct ShipSlab {
    first: u32,
    base: u64,
    offsets: Vec<u64>,
    bytes: Vec<u8>,
    path: PathBuf,
}

/// One canonical state-history entry.
pub struct ShipEntry<'a> {
    /// The 48-byte header: magic, block id, payload size.
    pub header: &'a [u8; SHIP_HEADER],
    pub payload: &'a [u8],
}

impl ShipEntry<'_> {
    pub fn id(&self) -> &[u8; 32] {
        self.header[8..40].try_into().unwrap()
    }
}

impl ShipSlab {
    pub fn range(&self) -> (u32, u32) {
        (self.first, self.first + self.offsets.len() as u32 - 1)
    }

    /// Bytes of the slab that belong to no canonical entry (superseded fork entries).
    pub fn skipped(&self) -> u64 {
        let mut skipped = 0;
        for pair in self.offsets.windows(2) {
            let at = (pair[0] - self.base) as usize;
            let Some(size) = self.bytes.get(at + 40..at + 48) else {
                continue;
            };
            let size = u64::from_le_bytes(size.try_into().unwrap());
            // An entry is followed by its own 8-byte position.
            let end = pair[0] + SHIP_HEADER as u64 + size + 8;
            skipped += pair[1].saturating_sub(end);
        }
        skipped
    }

    /// The entry the index names for `block`. Its magic, block number and size are checked.
    pub fn entry(&self, block: u32) -> Result<ShipEntry<'_>> {
        let (first, last) = self.range();
        ensure!(
            (first..=last).contains(&block),
            "block {block} is outside the slab {first}..={last}"
        );
        let at = (self.offsets[(block - first) as usize] - self.base) as usize;
        let header: &[u8; SHIP_HEADER] = self
            .bytes
            .get(at..at + SHIP_HEADER)
            .and_then(|h| h.try_into().ok())
            .with_context(|| {
                format!(
                    "block {block}'s entry header lies outside {}",
                    self.path.display()
                )
            })?;
        let magic = u64::from_le_bytes(header[0..8].try_into().unwrap());
        let number = u32::from_be_bytes(header[8..12].try_into().unwrap());
        if magic & SHIP_MAGIC_MASK != SHIP_MAGIC || number != block {
            bail!(
                "{}: the index entry of block {block} points at an entry for block {number} (magic {magic:#x})",
                self.path.display()
            );
        }
        let size = u64::from_le_bytes(header[40..48].try_into().unwrap());
        let start = at + SHIP_HEADER;
        let payload = usize::try_from(size)
            .ok()
            .and_then(|size| self.bytes.get(start..start.checked_add(size)?))
            .with_context(|| {
                format!(
                    "block {block}'s {size}-byte payload runs past its slab in {}",
                    self.path.display()
                )
            })?;
        Ok(ShipEntry { header, payload })
    }
}

/// A nodeos block log (`blocks.{log,index}`): irreversible blocks only.
#[derive(Debug)]
pub struct BlockFile(IndexedFile);

impl BlockFile {
    pub fn open(dir: &Path) -> Result<Self> {
        let log_path = dir.join("blocks.log");
        let index_path = dir.join("blocks.index");
        let log = File::open(&log_path).with_context(|| format!("open {}", log_path.display()))?;
        let mut header = [0u8; 8];
        pread_exact(&log, 0, &mut header).context("read block-log header")?;
        let version = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let first = u32::from_le_bytes(header[4..8].try_into().unwrap());
        ensure!(
            version != 0 && first != 0,
            "{} is not a recognizable block log (version={version}, first_block={first})",
            log_path.display()
        );
        Ok(Self(IndexedFile::open(log_path, index_path, first)?))
    }

    pub fn file(&self) -> &IndexedFile {
        &self.0
    }

    pub fn first_block(&self) -> u32 {
        self.0.first_block
    }

    /// The last block whose bytes are complete. The node appends a block and its 8-byte position
    /// before its index slot, but a following block may already be partly written, so the last
    /// indexed block counts only when the log ends with its position.
    pub fn last_block(&self) -> Result<Option<u32>> {
        let Some(last) = self.0.last_block()? else {
            return Ok(None);
        };
        let position = self.0.positions(last, last)?[0];
        let len = self.0.log_len()?;
        if len >= position + 8 {
            let mut tail = [0u8; 8];
            pread_exact(&self.0.log, len - 8, &mut tail)?;
            if u64::from_le_bytes(tail) == position {
                return Ok(Some(last));
            }
        }
        Ok((last > self.0.first_block).then(|| last - 1))
    }

    /// Blocks `from..=to`: one read of their index slots (and the next block's), one of their bytes.
    pub fn slab(&self, from: u32, to: u32) -> Result<BlockSlab> {
        let last = self
            .last_block()?
            .with_context(|| format!("{} is empty", self.0.log_path.display()))?;
        ensure!(
            to <= last,
            "block {to} is past the end of {} ({last})",
            self.0.log_path.display()
        );
        let indexed = self.0.last_block()?.unwrap_or(last);
        let (mut offsets, end) = if to < indexed {
            let mut slots = self.0.positions(from, to + 1)?;
            let next = slots.pop().unwrap();
            (slots, next)
        } else {
            (self.0.positions(from, to)?, self.0.log_len()?)
        };
        offsets.push(end);
        for pair in offsets.windows(2) {
            ensure!(
                pair[1] >= pair[0] + 8 && pair[1] - pair[0] <= MAX_ENTRY,
                "{} positions are not increasing",
                self.0.index_path.display()
            );
        }
        let base = offsets[0];
        let bytes = self.0.read(base, end - base)?;
        Ok(BlockSlab {
            first: from,
            base,
            offsets,
            bytes,
        })
    }
}

/// A block range of the block log in memory.
pub struct BlockSlab {
    first: u32,
    base: u64,
    /// One more than the blocks: the last is where the range ends.
    offsets: Vec<u64>,
    bytes: Vec<u8>,
}

impl BlockSlab {
    pub fn range(&self) -> (u32, u32) {
        (self.first, self.first + self.offsets.len() as u32 - 2)
    }

    /// The serialized `signed_block` of `block`, without its 8-byte position trailer.
    pub fn block(&self, block: u32) -> Option<&[u8]> {
        let (first, last) = self.range();
        if !(first..=last).contains(&block) {
            return None;
        }
        let i = (block - first) as usize;
        let start = (self.offsets[i] - self.base) as usize;
        let end = (self.offsets[i + 1] - self.base) as usize - 8;
        self.bytes.get(start..end)
    }

    /// `(producer, slot)`: the first two fields of the block header.
    pub fn header(&self, block: u32) -> Option<(u64, u32)> {
        let b = self.block(block)?;
        Some((
            u64::from_le_bytes(b.get(4..12)?.try_into().ok()?),
            u32::from_le_bytes(b.get(0..4)?.try_into().ok()?),
        ))
    }
}

/// A range cut into chunks for [`ordered_chunks`].
#[derive(Clone, Copy, Debug)]
pub struct Chunks<'a> {
    pub from: u32,
    pub to: u32,
    /// Blocks per chunk.
    pub size: u32,
    pub threads: usize,
    /// How many chunks a worker may run ahead of the fold.
    pub window: usize,
    /// A lower window shared with other runs (at least one chunk), read on every claim: a process
    /// serving several ranges at once bounds what all of them hold together.
    pub limit: Option<&'a AtomicUsize>,
}

/// Run `produce` over `from..=to` in chunks of `size` blocks on `threads` workers and pass each
/// result to `fold` in block order. Each worker builds its own state with `init`. A worker never
/// claims a chunk more than `window` chunks ahead of the fold. The first error stops every worker
/// and is returned.
pub fn ordered_chunks<S, T, I, P, F>(chunks: Chunks, init: I, produce: P, mut fold: F) -> Result<()>
where
    T: Send,
    I: Fn() -> Result<S> + Sync,
    P: Fn(&mut S, u32, u32) -> Result<T> + Sync,
    F: FnMut(u32, u32, T) -> Result<()>,
{
    let Chunks {
        from,
        to,
        size: chunk,
        threads,
        window,
        limit,
    } = chunks;
    ensure!(from <= to, "empty block range {from}..={to}");
    let chunk = chunk.max(1);
    let total = (u64::from(to - from) + 1).div_ceil(u64::from(chunk));
    let threads = threads.clamp(1, usize::try_from(total).unwrap_or(usize::MAX));
    let window = window.max(threads) as u64;
    let window = || limit.map_or(window, |l| (l.load(Relaxed) as u64).clamp(1, window));
    let bounds = |k: u64| {
        let start = u64::from(from) + k * u64::from(chunk);
        let end = (start + u64::from(chunk) - 1).min(u64::from(to));
        (start as u32, end as u32)
    };

    struct Gate {
        claimed: u64,
        folded: u64,
        stop: bool,
    }
    let gate = Mutex::new(Gate {
        claimed: 0,
        folded: 0,
        stop: false,
    });
    let moved = Condvar::new();
    let stop_all = || {
        gate.lock().unwrap().stop = true;
        moved.notify_all();
    };

    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel::<(u64, Result<T>)>();
        for _ in 0..threads {
            let sender = sender.clone();
            let (gate, moved, init, produce) = (&gate, &moved, &init, &produce);
            scope.spawn(move || {
                let mut state = match init() {
                    Ok(state) => state,
                    Err(e) => {
                        let _ = sender.send((u64::MAX, Err(e)));
                        return;
                    }
                };
                loop {
                    let k = {
                        let mut g = gate.lock().unwrap();
                        while !g.stop && g.claimed < total && g.claimed >= g.folded + window() {
                            g = moved.wait(g).unwrap();
                        }
                        if g.stop || g.claimed >= total {
                            return;
                        }
                        g.claimed += 1;
                        g.claimed - 1
                    };
                    let (start, end) = bounds(k);
                    let result = produce(&mut state, start, end);
                    let failed = result.is_err();
                    if sender.send((k, result)).is_err() || failed {
                        return;
                    }
                }
            });
        }
        drop(sender);
        let outcome = (|| -> Result<()> {
            let mut pending = BTreeMap::new();
            let mut next = 0u64;
            for (k, result) in &receiver {
                let value = result?;
                pending.insert(k, value);
                while let Some(value) = pending.remove(&next) {
                    let (start, end) = bounds(next);
                    fold(start, end, value)?;
                    next += 1;
                    gate.lock().unwrap().folded = next;
                    moved.notify_all();
                }
            }
            ensure!(
                next == total,
                "workers stopped before chunk {next} of {total}"
            );
            Ok(())
        })();
        stop_all();
        drop(receiver);
        outcome
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::SeqCst;

    fn dir(test: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("hyperion-slab-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn id(n: u32) -> [u8; 32] {
        let mut id = [n as u8; 32];
        id[..4].copy_from_slice(&n.to_be_bytes());
        id
    }

    /// Entries for `blocks`, then a superseded copy of `fork` re-appended with a new payload.
    fn ship_log(d: &Path, blocks: std::ops::RangeInclusive<u32>, fork: Option<u32>) {
        let (mut log, mut index) = (Vec::new(), Vec::<u64>::new());
        let entry = |log: &mut Vec<u8>, n: u32, payload: &[u8]| -> u64 {
            let pos = log.len() as u64;
            log.extend_from_slice(&SHIP_MAGIC.to_le_bytes());
            log.extend_from_slice(&id(n));
            log.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            log.extend_from_slice(payload);
            log.extend_from_slice(&pos.to_le_bytes());
            pos
        };
        let first = *blocks.start();
        for n in blocks.clone() {
            index.push(entry(&mut log, n, &[n as u8; 3]));
        }
        if let Some(f) = fork {
            index.truncate((f - first) as usize);
            for n in f..=*blocks.end() {
                index.push(entry(&mut log, n, &[0xee, n as u8]));
            }
        }
        std::fs::write(d.join("trace_history.log"), log).unwrap();
        let bytes: Vec<u8> = index.iter().flat_map(|p| p.to_le_bytes()).collect();
        std::fs::write(d.join("trace_history.index"), bytes).unwrap();
    }

    #[test]
    fn ship_slabs_follow_the_index_past_forks() {
        let d = dir("ship");
        ship_log(&d, 2..=9, Some(6));
        let log = ShipFile::open(&d, "trace_history").unwrap();
        assert_eq!((log.first_block(), log.last_block().unwrap()), (2, Some(9)));
        let slab = log.slab(4, 8).unwrap();
        assert_eq!(slab.entry(5).unwrap().payload, &[5, 5, 5]);
        assert_eq!(
            slab.entry(6).unwrap().payload,
            &[0xee, 6],
            "the fork's entry"
        );
        assert_eq!(slab.entry(8).unwrap().id(), &id(8));
        assert!(slab.skipped() > 0, "superseded entries 6..=9 lie between");
        assert!(slab.entry(9).is_err() && slab.entry(3).is_err());
        assert!(log.slab(9, 10).is_err(), "past the index");
        assert!(log.slab(1, 3).is_err(), "before the log");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn a_wrong_index_entry_is_refused() {
        let d = dir("wrong");
        ship_log(&d, 2..=5, None);
        let mut index = std::fs::read(d.join("trace_history.index")).unwrap();
        let swapped: Vec<u8> = [&index[8..16], &index[8..16]].concat();
        index[8..24].copy_from_slice(&swapped);
        std::fs::write(d.join("trace_history.index"), index).unwrap();
        let log = ShipFile::open(&d, "trace_history").unwrap();
        assert!(log.slab(2, 5).is_err(), "positions must increase");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn block_slabs_cut_blocks_and_ignore_a_partial_append() {
        let d = dir("blocks");
        let (mut log, mut index) = (Vec::new(), Vec::new());
        log.extend_from_slice(&3u32.to_le_bytes());
        log.extend_from_slice(&1u32.to_le_bytes());
        log.extend_from_slice(&[0x42; 32]);
        for n in 1..=4u32 {
            let pos = log.len() as u64;
            index.extend_from_slice(&pos.to_le_bytes());
            log.extend_from_slice(&(100 + n).to_le_bytes()); // slot
            log.extend_from_slice(&u64::from(n).to_le_bytes()); // producer
            log.extend_from_slice(&[n as u8; 5]);
            log.extend_from_slice(&pos.to_le_bytes());
        }
        std::fs::write(d.join("blocks.index"), &index).unwrap();
        std::fs::write(d.join("blocks.log"), &log).unwrap();
        let blocks = BlockFile::open(&d).unwrap();
        assert_eq!(blocks.last_block().unwrap(), Some(4));
        let slab = blocks.slab(2, 4).unwrap();
        assert_eq!(slab.header(3), Some((3, 103)));
        assert_eq!(slab.block(4).unwrap().len(), 17);
        assert!(slab.block(1).is_none());

        // Block 5 is half written and already indexed: block 4 still counts, block 5 does not.
        index.extend_from_slice(&(log.len() as u64).to_le_bytes());
        log.extend_from_slice(&[1, 2, 3]);
        std::fs::write(d.join("blocks.index"), &index).unwrap();
        std::fs::write(d.join("blocks.log"), &log).unwrap();
        let blocks = BlockFile::open(&d).unwrap();
        assert_eq!(blocks.last_block().unwrap(), Some(4));
        assert_eq!(blocks.slab(3, 4).unwrap().block(4).unwrap().len(), 17);
        assert!(blocks.slab(4, 5).is_err());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn chunks_fold_in_order_within_the_window() {
        let ahead = AtomicUsize::new(0);
        let folded = AtomicUsize::new(0);
        let mut seen = Vec::new();
        ordered_chunks(
            Chunks {
                from: 5,
                to: 1000,
                size: 7,
                threads: 4,
                window: 4,
                limit: None,
            },
            || Ok(()),
            |_, start, end| {
                let k = (start as usize - 5) / 7;
                ahead.fetch_max(k.saturating_sub(folded.load(SeqCst)), SeqCst);
                Ok((start, end))
            },
            |start, end, value| {
                assert_eq!((start, end), value);
                seen.push(value);
                folded.fetch_add(1, SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen.first(), Some(&(5, 11)));
        assert_eq!(seen.last(), Some(&(999, 1000)));
        assert!(seen.windows(2).all(|w| w[1].0 == w[0].1 + 1));
        assert!(ahead.load(SeqCst) <= 4, "{}", ahead.load(SeqCst));
    }

    #[test]
    fn the_first_error_stops_the_run() {
        let produced = AtomicUsize::new(0);
        let r = ordered_chunks(
            Chunks {
                from: 1,
                to: 10_000,
                size: 10,
                threads: 4,
                window: 4,
                limit: None,
            },
            || Ok(()),
            |_, start, _| {
                produced.fetch_add(1, SeqCst);
                if start == 31 {
                    bail!("chunk at 31 failed");
                }
                Ok(())
            },
            |_, _, ()| Ok(()),
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("chunk at 31 failed"));
        assert!(produced.load(SeqCst) < 100);
        let r = ordered_chunks(
            Chunks {
                from: 1,
                to: 100,
                size: 10,
                threads: 2,
                window: 2,
                limit: None,
            },
            || Ok(()),
            |_, _, _| Ok(()),
            |s, _, ()| {
                ensure!(s != 51, "fold refused 51");
                Ok(())
            },
        );
        assert!(format!("{:#}", r.unwrap_err()).contains("fold refused 51"));
    }
}
