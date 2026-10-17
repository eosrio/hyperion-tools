//! Strict, index-addressed reader for state-history logs (`trace_history`,
//! `chain_state_history`, `finality_data_history`). Every read goes through the `.index`, so
//! orphaned fork entries are never seen, and any inconsistency fails closed.
//!
//! A log is a run of entries `[magic u64][block id, 32 bytes][payload_size u64][payload]
//! [u64 offset of the entry]`, little-endian, and the `.index` holds one u64 entry offset per
//! block from the first block in the file. Spring's writer only appends: a fork truncates the
//! index and appends the replacement entries, leaving the abandoned ones in the file, so a log
//! can physically hold `2 3 4 5 6 7 5'`. A forward scan would serve the orphans; the index never
//! points at them.
//!
//! The payload is a preamble and a zlib stream ([`PayloadEnc`]):
//!   * Type A (Leap 4 and later, every Spring release): u32 `1`, then the u64 uncompressed size.
//!     Spring stores the stream at zlib level 0; Leap compresses it.
//!   * Type B (EOSIO 1.8 to Leap 3): a u32 holding the zlib length, or 0. The uncompressed size
//!     is known only after inflating the whole stream once ([`ShipLog::verify_count`]).
//!
//! [`ShipLog::open`] mirrors Spring's `check_log_on_init` read-only and without its recovery: the
//! first entry must carry the version-0, non-pruned magic, the last entry must end exactly at the
//! end of the file, and the index must hold one slot per block with its last slot on the last
//! entry. Per-block reads check the magic, the block number in the id and the position trailer.
//! Inflating checks the zlib framing and adler32, that the stream spans the whole payload, and
//! the declared size. Every error names the file, and per-block errors the block and offset.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use flate2::{Decompress, FlushDecompress, Status};

use crate::binio::{FileCache, PosFile, num_from_id};

/// The magic of every entry of a version-0, non-pruned log: the name `ship`, with the log version
/// in the low 16 bits and feature flags in the 16 above them.
pub const SHIP_MAGIC: u64 = 0xc35d_5000_0000_0000;
/// Bytes of an entry header: magic, block id, payload size.
pub const HEADER_LEN: u64 = 48;
/// Bytes of an entry besides its payload: the header and the u64 position trailer.
pub const ENTRY_OVERHEAD: u64 = HEADER_LEN + 8;
/// Compressed bytes read per refill of an [`InflateStream`].
pub const INPUT_WINDOW: usize = 1 << 20;

const NAME_MASK: u64 = 0xffff_ffff_0000_0000;
const PRUNED_FEATURE: u64 = 1;
const INDEX_HINT: &str = "run `antelope-ship-archive index`";
/// Output scratch of [`ShipLog::verify_count`].
const COUNT_SCRATCH: usize = 256 << 10;
/// Output growth bounds of [`ShipLog::read_inline`] for Type B entries (size unknown up front).
const INLINE_STEP_MIN: usize = 64 << 10;
const INLINE_STEP_MAX: usize = 16 << 20;

/// Why an entry magic is not [`SHIP_MAGIC`], naming the writer when it is known.
pub fn unsupported_magic(magic: u64) -> Option<String> {
    if magic == SHIP_MAGIC {
        return None;
    }
    if magic & NAME_MASK != SHIP_MAGIC {
        return Some(
            "no `ship` magic: not a state-history log, or one written before nodeos 1.8 (EOSIO \
             1.5 to 1.7), which is not supported"
                .into(),
        );
    }
    let (version, features) = (magic & 0xffff, (magic >> 16) & 0xffff);
    Some(if version == 1 {
        "state-history log version 1, written by EOSIO 2.1 or 2.2, is not supported: its trace \
         entries split out prunable transaction data that SHiP does not serve"
            .into()
    } else if features & PRUNED_FEATURE != 0 {
        "pruned state-history log (state-history-log-retain-blocks) is not supported".into()
    } else {
        format!("unsupported state-history log version {version}, features {features:#x}")
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LogKind {
    Trace,
    ChainState,
    Finality,
}

impl LogKind {
    pub const ALL: [Self; 3] = [Self::Trace, Self::ChainState, Self::Finality];

    /// The name Spring gives the log's files: `<stem>.log`, retained `<stem>-<first>-<last>.log`.
    pub fn stem(self) -> &'static str {
        match self {
            Self::Trace => "trace_history",
            Self::ChainState => "chain_state_history",
            Self::Finality => "finality_data_history",
        }
    }

    /// A short name for messages and metric labels: `trace`, `chain_state`, `finality`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::ChainState => "chain_state",
            Self::Finality => "finality",
        }
    }
}

/// How an entry's payload frames its zlib stream, decided by the u32 at entry offset 48.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PayloadEnc {
    /// Type A: u32 `1`, then the u64 uncompressed size; the stream starts at entry offset 60.
    Sized { uncompressed: u64 },
    /// Type B: the u32 is the zlib length or 0; the stream starts at entry offset 52, and its
    /// uncompressed size is unknown until it is inflated.
    Legacy { u32_field: u32 },
}

/// A validated entry header. `pos` is the entry's offset in its log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EntryHeader {
    pub pos: u64,
    pub id: [u8; 32],
    pub payload_size: u64,
    pub enc: PayloadEnc,
}

impl EntryHeader {
    /// The block number embedded in the id.
    pub fn num(&self) -> u32 {
        num_from_id(&self.id)
    }

    /// The inflated payload size when the header records it (Type A).
    pub fn uncompressed(&self) -> Option<u64> {
        match self.enc {
            PayloadEnc::Sized { uncompressed } => Some(uncompressed),
            PayloadEnc::Legacy { .. } => None,
        }
    }

    /// File offset of the zlib stream.
    pub fn data_offset(&self) -> u64 {
        self.pos + HEADER_LEN + self.preamble_len()
    }

    /// Bytes of the zlib stream.
    pub fn data_len(&self) -> u64 {
        self.payload_size.saturating_sub(self.preamble_len())
    }

    /// One past the entry's last byte, its position trailer.
    pub fn end(&self) -> u64 {
        self.pos + ENTRY_OVERHEAD + self.payload_size
    }

    fn preamble_len(&self) -> u64 {
        match self.enc {
            PayloadEnc::Sized { .. } => 12,
            PayloadEnc::Legacy { .. } => 4,
        }
    }
}

/// One opened, validated log file and its index. Holds paths and bounds only: file handles come
/// from the shared [`FileCache`] on each read.
#[derive(Clone)]
pub struct ShipLog {
    kind: LogKind,
    log: PathBuf,
    index: PathBuf,
    cache: Arc<FileCache>,
    /// The first block in the file, which is also the block of index slot 0 (pruned logs, where
    /// the two differ, are refused).
    begin: u32,
    end: u32,
    last_pos: u64,
    size: u64,
}

impl fmt::Debug for ShipLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShipLog")
            .field("kind", &self.kind)
            .field("log", &self.log)
            .field("index", &self.index)
            .field("range", &(self.begin, self.end))
            .field("last_pos", &self.last_pos)
            .field("size", &self.size)
            .finish()
    }
}

impl ShipLog {
    /// Open and validate `log` with its `index` (next to it, or in a sidecar directory).
    /// `Ok(None)` for a 0-byte log, which is what the head log is right after a rotation.
    pub fn open(
        log: &Path,
        index: &Path,
        kind: LogKind,
        cache: &Arc<FileCache>,
    ) -> Result<Option<Self>> {
        let file = cache.get(log)?;
        let size = file.len();
        if size == 0 {
            return Ok(None);
        }
        let name = log.display();
        ensure!(
            size >= ENTRY_OVERHEAD,
            "{name}: corrupted tail; repair the log first: {size} bytes cannot hold an entry"
        );

        let magic = file.u64_at(0)?;
        if let Some(problem) = unsupported_magic(magic) {
            if magic & NAME_MASK == SHIP_MAGIC {
                bail!(
                    "pruned or unsupported state-history log {name}: {problem}; use a vacuumed, \
                     non-pruned copy"
                );
            }
            bail!("{name} is not a supported state-history log: {problem}");
        }
        let begin = num_from_id(&id_at(&file, 0)?);
        read_header(&file, begin, 0).with_context(|| format!("{name}: first entry"))?;

        let (last, last_pos) =
            tail(&file).with_context(|| format!("{name}: corrupted tail; repair the log first"))?;
        ensure!(
            begin <= last,
            "{name}: corrupted tail; repair the log first: the last entry is block {last}, \
             before the first, block {begin}"
        );
        let end = last
            .checked_add(1)
            .with_context(|| format!("{name}: block {last} is the last block number"))?;

        let idx = cache
            .get(index)
            .with_context(|| format!("index for {name}: {INDEX_HINT}"))?;
        let want = u64::from(end - begin) * 8;
        ensure!(
            idx.len() == want,
            "index mismatch for {name}: {} holds {} bytes, blocks {begin}..{end} need {want}: \
             {INDEX_HINT}",
            index.display(),
            idx.len()
        );
        let slot = idx.u64_at(want - 8)?;
        ensure!(
            slot == last_pos,
            "index mismatch for {name}: the slot of block {last} holds offset {slot}, the last \
             entry is at {last_pos}: {INDEX_HINT}"
        );

        Ok(Some(Self {
            kind,
            log: log.to_path_buf(),
            index: index.to_path_buf(),
            cache: Arc::clone(cache),
            begin,
            end,
            last_pos,
            size,
        }))
    }

    pub fn kind(&self) -> LogKind {
        self.kind
    }

    /// The blocks the log holds, `[begin, end)`.
    pub fn range(&self) -> (u32, u32) {
        (self.begin, self.end)
    }

    pub fn log_path(&self) -> &Path {
        &self.log
    }

    pub fn index_path(&self) -> &Path {
        &self.index
    }

    /// The log's length in bytes, as validated at open.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// The index slots (entry offsets) of blocks `from..from + count`, in one read of
    /// `count * 8` bytes. Replaces the contents of `out`.
    pub fn positions(&self, from: u32, count: u32, out: &mut Vec<u64>) -> Result<()> {
        let in_range =
            from >= self.begin && from.checked_add(count).is_some_and(|to| to <= self.end);
        ensure!(
            in_range,
            "{}: {count} blocks from {from} are not all in {}..{}",
            self.log.display(),
            self.begin,
            self.end
        );
        let bytes = self
            .index_file()?
            .read_vec(u64::from(from - self.begin) * 8, count as usize * 8)?;
        out.clear();
        out.extend(
            bytes
                .as_chunks::<8>()
                .0
                .iter()
                .map(|s| u64::from_le_bytes(*s)),
        );
        Ok(())
    }

    /// The header of block `n`'s entry at offset `pos` (an index slot from [`Self::positions`]).
    pub fn header_at(&self, n: u32, pos: u64) -> Result<EntryHeader> {
        self.check_block(n)?;
        read_header(&self.log_file()?, n, pos).with_context(|| self.block(n))
    }

    /// The header of block `n`'s entry, found through the index.
    pub fn header(&self, n: u32) -> Result<EntryHeader> {
        self.check_block(n)?;
        let pos = self.index_file()?.u64_at(u64::from(n - self.begin) * 8)?;
        read_header(&self.log_file()?, n, pos).with_context(|| self.block(n))
    }

    /// Block `n`'s id, from its validated entry header.
    pub fn id(&self, n: u32) -> Result<[u8; 32]> {
        Ok(self.header(n)?.id)
    }

    /// Inflate `h`'s payload into `out` (replacing its contents), refusing one that inflates to
    /// more than `limit` bytes. A Type A size over the limit is refused before anything is read.
    pub fn read_inline(&self, h: &EntryHeader, limit: u64, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        let mut step = match h.uncompressed() {
            Some(size) => {
                self.check_limit(h, size, limit)?;
                // A corrupt size the limit lets through must fail, not abort the process.
                usize::try_from(size)
                    .ok()
                    .filter(|&size| out.try_reserve_exact(size).is_ok())
                    .with_context(|| {
                        format!(
                            "{}: no room for the {size} bytes its header declares",
                            self.entry(h)
                        )
                    })?
            }
            None => usize::try_from(h.data_len().saturating_mul(4))
                .unwrap_or(usize::MAX)
                .clamp(INLINE_STEP_MIN, INLINE_STEP_MAX),
        };
        let mut s = self.stream(h)?;
        loop {
            // At most one byte past the limit, which is enough to tell that it is exceeded.
            let room = limit.saturating_sub(out.len() as u64).saturating_add(1);
            let more = s.next_chunk(out, step.min(usize::try_from(room).unwrap_or(usize::MAX)))?;
            ensure!(
                out.len() as u64 <= limit,
                "{}: the payload inflates to more than the {limit}-byte limit",
                self.entry(h)
            );
            if !more {
                return Ok(());
            }
            step = step.saturating_mul(2).min(INLINE_STEP_MAX);
        }
    }

    /// Inflate `h`'s payload in full, keeping no output, and return its length: the same checks
    /// as [`Self::read_inline`]. This is how a Type B entry's size is learned, and how an entry
    /// is validated before it is streamed.
    pub fn verify_count(&self, h: &EntryHeader, limit: u64) -> Result<u64> {
        let mut scratch_len = COUNT_SCRATCH as u64;
        if let Some(size) = h.uncompressed() {
            self.check_limit(h, size, limit)?;
            // Saturating: a corrupt size of u64::MAX must not wrap the scratch to nothing.
            scratch_len = scratch_len.min(size.saturating_add(1));
        }
        // Never empty: inflating into an empty buffer makes no progress (and is refused).
        let mut scratch = vec![0; scratch_len.min(limit.saturating_add(1)).max(1) as usize];
        let mut s = self.stream(h)?;
        while !s.done {
            s.inflate(&mut scratch)?;
            ensure!(
                s.produced() <= limit,
                "{}: the payload inflates to more than the {limit}-byte limit",
                self.entry(h)
            );
        }
        Ok(s.produced())
    }

    /// A chunked inflater over `h`'s payload, reading the compressed bytes in windows of
    /// [`INPUT_WINDOW`]. Its checks complete only at the end of the stream, so a caller that must
    /// not emit bad data validates the entry with [`Self::verify_count`] first.
    pub fn stream(&self, h: &EntryHeader) -> Result<InflateStream> {
        Ok(InflateStream::new(self.log_file()?, h))
    }

    fn check_block(&self, n: u32) -> Result<()> {
        ensure!(
            (self.begin..self.end).contains(&n),
            "{}: block {n} is outside {}..{}",
            self.log.display(),
            self.begin,
            self.end
        );
        Ok(())
    }

    fn check_limit(&self, h: &EntryHeader, size: u64, limit: u64) -> Result<()> {
        ensure!(
            size <= limit,
            "{}: the payload inflates to {size} bytes, over the {limit}-byte limit",
            self.entry(h)
        );
        Ok(())
    }

    /// The log file, which must still have the length validated at open: the cache may have
    /// reopened it, and a frozen log that changed is not the log that was validated.
    fn log_file(&self) -> Result<PosFile> {
        let f = self.cache.get(&self.log)?;
        ensure!(
            f.len() == self.size,
            "{} changed after it was opened: {} bytes, was {}",
            self.log.display(),
            f.len(),
            self.size
        );
        Ok(f)
    }

    fn index_file(&self) -> Result<PosFile> {
        let f = self.cache.get(&self.index)?;
        let want = u64::from(self.end - self.begin) * 8;
        ensure!(
            f.len() == want,
            "{} changed after it was opened: {} bytes, was {want}",
            self.index.display(),
            f.len()
        );
        Ok(f)
    }

    fn block(&self, n: u32) -> String {
        format!("{}: block {n}", self.log.display())
    }

    fn entry(&self, h: &EntryHeader) -> String {
        format!(
            "{}: block {} at offset {}",
            self.log.display(),
            h.num(),
            h.pos
        )
    }
}

fn le_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().expect("four bytes"))
}

fn le_u64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().expect("eight bytes"))
}

fn id_at(file: &PosFile, pos: u64) -> Result<[u8; 32]> {
    let mut id = [0; 32];
    file.read_exact_at(pos + 8, &mut id)?;
    Ok(id)
}

/// The header of the entry at `pos`, which must be block `n`'s: exact magic, block number,
/// a payload within the file and long enough for its preamble, and a trailer naming `pos`.
fn read_header(file: &PosFile, n: u32, pos: u64) -> Result<EntryHeader> {
    let size = file.len();
    ensure!(
        pos.checked_add(ENTRY_OVERHEAD).is_some_and(|e| e <= size),
        "offset {pos} leaves no room for an entry in the {size}-byte log"
    );
    // The header and the longest preamble in one read (an entry is at least 56 bytes).
    let head = file.read_vec(pos, (size - pos).min(HEADER_LEN + 12) as usize)?;
    let magic = le_u64(&head);
    ensure!(
        magic == SHIP_MAGIC,
        "no entry header at offset {pos} (magic {magic:#018x})"
    );
    let id: [u8; 32] = head[8..40].try_into()?;
    let num = num_from_id(&id);
    ensure!(num == n, "the entry at offset {pos} is block {num}");
    let payload_size = le_u64(&head[40..]);
    let Some(end) = (pos + ENTRY_OVERHEAD)
        .checked_add(payload_size)
        .filter(|&end| end <= size)
    else {
        bail!(
            "payload size {payload_size} at offset {pos} runs past the end of the {size}-byte log"
        );
    };
    ensure!(
        payload_size >= 4,
        "payload size {payload_size} at offset {pos} is shorter than its preamble"
    );
    let trailer = file.u64_at(end - 8)?;
    ensure!(
        trailer == pos,
        "the position trailer of the entry at offset {pos} holds {trailer}"
    );
    let field = le_u32(&head[48..]);
    let enc = if field == 1 {
        ensure!(
            payload_size >= 12,
            "payload size {payload_size} at offset {pos} is shorter than its sized preamble"
        );
        PayloadEnc::Sized {
            uncompressed: le_u64(&head[52..]),
        }
    } else {
        let zlen = payload_size - 4;
        ensure!(
            field == 0 || u64::from(field) == zlen,
            "the legacy size field of the entry at offset {pos} holds {field}, its zlib stream \
             has {zlen} bytes"
        );
        PayloadEnc::Legacy { u32_field: field }
    };
    Ok(EntryHeader {
        pos,
        id,
        payload_size,
        enc,
    })
}

/// The last entry's block and offset. It must be a valid entry that ends at the end of the file.
fn tail(file: &PosFile) -> Result<(u32, u64)> {
    let size = file.len();
    let last_pos = file.u64_at(size - 8)?;
    ensure!(
        last_pos <= size - ENTRY_OVERHEAD,
        "the last trailer holds offset {last_pos}, too close to the end of the {size}-byte log \
         for an entry"
    );
    let last = num_from_id(&id_at(file, last_pos)?);
    let h = read_header(file, last, last_pos)?;
    ensure!(
        h.end() == size,
        "the entry at offset {last_pos} (block {last}) ends at byte {}, not at the end of the \
         file ({size})",
        h.end()
    );
    Ok((last, last_pos))
}

/// Inflates one entry's zlib stream chunk by chunk, reading the compressed bytes in windows of
/// [`INPUT_WINDOW`]. At the end of the stream it checks that the stream used the whole payload
/// and, for Type A, that the output has the declared size; the decoder checks the adler32. An
/// error poisons the stream.
#[derive(Debug)]
pub struct InflateStream {
    file: PosFile,
    block: u32,
    pos: u64,
    /// File offset of the next compressed byte to read, and the end of the stream.
    next: u64,
    end: u64,
    len: u64,
    window: Vec<u8>,
    taken: usize,
    z: Decompress,
    expected: Option<u64>,
    done: bool,
    failed: bool,
}

impl InflateStream {
    fn new(file: PosFile, h: &EntryHeader) -> Self {
        Self {
            file,
            block: h.num(),
            pos: h.pos,
            next: h.data_offset(),
            end: h.data_offset() + h.data_len(),
            len: h.data_len(),
            window: Vec::new(),
            taken: 0,
            z: Decompress::new(true),
            expected: h.uncompressed(),
            done: false,
            failed: false,
        }
    }

    /// Inflated bytes produced so far.
    pub fn produced(&self) -> u64 {
        self.z.total_out()
    }

    /// Whether the stream has ended and passed every check.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Append up to `max` inflated bytes to `out`. Returns `Ok(true)` while more may follow and
    /// `Ok(false)` once the stream has ended and passed every check. Every call that returns
    /// `true` appends exactly `max` bytes. For a Type A entry the call that returns `false`
    /// appends the last bytes; for a Type B entry it may append fewer, possibly none.
    pub fn next_chunk(&mut self, out: &mut Vec<u8>, max: usize) -> Result<bool> {
        let room = match self.expected {
            Some(e) => max.min(usize::try_from(e.saturating_sub(self.produced())).unwrap_or(max)),
            None => max,
        };
        if room > 0 {
            let start = out.len();
            out.resize(start + room, 0);
            let r = self.inflate(&mut out[start..]);
            out.truncate(start + r.as_ref().map_or(0, |&n| n));
            r?;
        }
        if !self.done && self.expected == Some(self.produced()) {
            // Every declared byte is out, so the stream must end without another one.
            self.inflate(&mut [0])?;
        }
        Ok(!self.done)
    }

    /// Inflate into `buf` until it is full or the stream ends; returns the bytes written. Until
    /// the stream ends `buf` must not be empty: such a call could make no progress, and a
    /// caller looping until the end would spin forever.
    fn inflate(&mut self, buf: &mut [u8]) -> Result<usize> {
        ensure!(
            !self.failed,
            "{}: an earlier read of this entry failed",
            self.what()
        );
        ensure!(
            self.done || !buf.is_empty(),
            "{}: no room to inflate the payload into",
            self.what()
        );
        let r = self.pump(buf);
        if r.is_err() {
            self.failed = true;
        }
        r.with_context(|| self.what())
    }

    fn pump(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut written = 0;
        while !self.done {
            if self.taken == self.window.len() && self.next < self.end {
                let n = usize::try_from(self.end - self.next)
                    .unwrap_or(INPUT_WINDOW)
                    .min(INPUT_WINDOW);
                self.window.resize(n, 0);
                self.file.read_exact_at(self.next, &mut self.window)?;
                self.next += n as u64;
                self.taken = 0;
            }
            let (in0, out0) = (self.z.total_in(), self.z.total_out());
            let status = self
                .z
                .decompress(
                    &self.window[self.taken..],
                    &mut buf[written..],
                    FlushDecompress::None,
                )
                .map_err(|e| anyhow!("invalid zlib payload: {e}"))?;
            let consumed = (self.z.total_in() - in0) as usize;
            let produced = (self.z.total_out() - out0) as usize;
            self.taken += consumed;
            written += produced;
            if let Some(e) = self.expected {
                ensure!(
                    self.z.total_out() <= e,
                    "the payload inflates past the {e} bytes its header declares"
                );
            }
            if status == Status::StreamEnd {
                self.finish()?;
            } else if consumed == 0 && produced == 0 {
                if written == buf.len() {
                    // Out of room: the caller drains `buf` and calls again.
                    break;
                }
                bail!(
                    "the zlib stream stops after {} of its {} bytes without ending",
                    self.z.total_in(),
                    self.len
                );
            }
        }
        Ok(written)
    }

    fn finish(&mut self) -> Result<()> {
        let used = self.z.total_in();
        ensure!(
            used == self.len,
            "{} bytes of the payload follow the end of its zlib stream",
            self.len - used
        );
        if let Some(e) = self.expected {
            ensure!(
                self.z.total_out() == e,
                "the payload inflates to {} bytes, its header declares {e}",
                self.z.total_out()
            );
        }
        self.done = true;
        Ok(())
    }

    fn what(&self) -> String {
        format!(
            "{}: block {} at offset {}",
            self.file.path().display(),
            self.block,
            self.pos
        )
    }
}

#[cfg(all(test, feature = "testkit"))]
mod tests {
    use std::fmt::Debug;
    use std::ops::Range;

    use super::*;
    use crate::binio::TestDir;
    use crate::testkit::ship::{
        self as kit, Encoding, LogBuilder, PRUNED_MAGIC, V1_MAGIC, synthetic_id, synthetic_payload,
    };

    /// Payload lengths covering the empty stream, one stored block, a block boundary and
    /// several blocks.
    const LENS: [usize; 6] = [0, 1, 100, 32 << 10, (32 << 10) + 1, 100_000];

    fn cache() -> Arc<FileCache> {
        FileCache::new(16)
    }

    fn chain(encoding: Encoding, blocks: Range<u32>, len: usize) -> LogBuilder {
        let mut b = LogBuilder::new(encoding);
        b.push_blocks(blocks, len);
        b
    }

    fn try_open(dir: &TestDir, b: &LogBuilder) -> Result<Option<ShipLog>> {
        let (log, index) = b.write(dir.path(), "trace_history").unwrap();
        ShipLog::open(&log, &index, LogKind::Trace, &cache())
    }

    fn open(dir: &TestDir, b: &LogBuilder) -> ShipLog {
        try_open(dir, b).unwrap().expect("a non-empty log")
    }

    fn err<T: Debug>(r: Result<T>) -> String {
        format!("{:#}", r.unwrap_err())
    }

    fn inline(log: &ShipLog, n: u32) -> Vec<u8> {
        let mut out = Vec::new();
        log.read_inline(&log.header(n).unwrap(), u64::MAX, &mut out)
            .unwrap();
        out
    }

    /// The stream's chunks for `max`, checking the chunk contract on the way.
    fn chunks(log: &ShipLog, h: &EntryHeader, max: usize) -> Vec<Vec<u8>> {
        let mut s = log.stream(h).unwrap();
        let mut out = Vec::new();
        loop {
            let mut chunk = Vec::new();
            let more = s.next_chunk(&mut chunk, max).unwrap();
            assert!(chunk.len() <= max);
            if more {
                assert_eq!(chunk.len(), max, "a chunk before the end is full");
            }
            out.push(chunk);
            if !more {
                assert!(s.is_done());
                assert!(!s.next_chunk(&mut Vec::new(), max).unwrap(), "stays ended");
                return out;
            }
        }
    }

    /// Drive a stream until it fails, returning the error.
    fn stream_error(log: &ShipLog, h: &EntryHeader, max: usize) -> (String, usize) {
        let mut s = log.stream(h).unwrap();
        let mut out = Vec::new();
        loop {
            match s.next_chunk(&mut out, max) {
                Ok(true) => {}
                Ok(false) => panic!("block {} ended cleanly", h.num()),
                Err(e) => return (format!("{e:#}"), out.len()),
            }
        }
    }

    #[test]
    fn log_kinds_name_spring_files() {
        assert_eq!(
            LogKind::ALL.map(LogKind::stem),
            [
                "trace_history",
                "chain_state_history",
                "finality_data_history"
            ]
        );
        assert_eq!(
            LogKind::ALL.map(LogKind::label),
            ["trace", "chain_state", "finality"]
        );
    }

    #[test]
    fn a_zero_byte_log_is_none() {
        let dir = TestDir::new("shiplog-empty");
        let log = dir.write("trace_history.log", &[]);
        // A freshly rotated head log: no index is needed to tell that it is empty.
        let missing = dir.path().join("trace_history.index");
        let opened = ShipLog::open(&log, &missing, LogKind::Trace, &cache()).unwrap();
        assert!(opened.is_none());
        let b = LogBuilder::new(Encoding::SizedStored);
        assert_eq!(b.range(), (0, 0));
        assert!(try_open(&dir, &b).unwrap().is_none());
    }

    #[test]
    fn type_a_stored_entries_read_back() {
        let dir = TestDir::new("shiplog-stored");
        let mut b = LogBuilder::new(Encoding::SizedStored);
        for (i, len) in LENS.into_iter().enumerate() {
            let n = 2 + i as u32;
            b.push(synthetic_id(n, 0), &synthetic_payload(n, len));
        }
        let log = open(&dir, &b);
        let file = std::fs::read(log.log_path()).unwrap();
        assert_eq!((log.kind(), log.range()), (LogKind::Trace, (2, 8)));
        assert_eq!(log.size(), file.len() as u64);

        let mut slots = Vec::new();
        log.positions(2, 6, &mut slots).unwrap();
        assert_eq!(slots, (2..8).map(|n| b.position(n)).collect::<Vec<_>>());

        for (i, len) in LENS.into_iter().enumerate() {
            let n = 2 + i as u32;
            let raw = synthetic_payload(n, len);
            let h = log.header(n).unwrap();
            assert_eq!(h, log.header_at(n, slots[i]).unwrap());
            assert_eq!(
                (h.num(), h.pos, h.id),
                (n, b.position(n), synthetic_id(n, 0))
            );
            let uncompressed = len as u64;
            assert_eq!(h.enc, PayloadEnc::Sized { uncompressed });
            assert_eq!(h.uncompressed(), Some(uncompressed));
            assert_eq!(log.id(n).unwrap(), h.id);
            // Spring's level-0 stream, right after the sized preamble.
            assert_eq!(h.data_offset(), h.pos + 60);
            let data = &file[h.data_offset() as usize..][..h.data_len() as usize];
            assert_eq!(data, kit::zlib_stored(&raw));
            assert_eq!(h.end(), h.data_offset() + h.data_len() + 8);
            assert_eq!(inline(&log, n), raw);
            assert_eq!(log.verify_count(&h, uncompressed).unwrap(), uncompressed);
        }
    }

    #[test]
    fn type_b_deflate_entries_with_a_length_and_with_zero() {
        for encoding in [Encoding::LegacyLength, Encoding::LegacyZero] {
            let dir = TestDir::new("shiplog-legacy");
            let mut b = LogBuilder::new(encoding);
            for (i, len) in LENS.into_iter().enumerate() {
                let n = 10 + i as u32;
                b.push(synthetic_id(n, 0), &synthetic_payload(n, len));
            }
            let log = open(&dir, &b);
            for (i, len) in LENS.into_iter().enumerate() {
                let n = 10 + i as u32;
                let raw = synthetic_payload(n, len);
                let h = log.header(n).unwrap();
                let PayloadEnc::Legacy { u32_field } = h.enc else {
                    panic!("{encoding:?} block {n}: {:?}", h.enc);
                };
                let want = match encoding {
                    Encoding::LegacyLength => h.data_len() as u32,
                    _ => 0,
                };
                assert_eq!(u32_field, want, "{encoding:?} block {n}");
                assert_eq!(h.data_offset(), h.pos + 52);
                assert_eq!(h.uncompressed(), None);
                if len >= 32 << 10 {
                    assert!(h.data_len() < len as u64, "really compressed");
                }
                assert_eq!(inline(&log, n), raw);
                assert_eq!(log.verify_count(&h, u64::MAX).unwrap(), len as u64);
            }
        }
    }

    #[test]
    fn one_log_can_mix_every_encoding() {
        // Upgrading nodeos never rewrites old entries, so one log holds several encodings.
        let dir = TestDir::new("shiplog-mixed");
        let mut b = LogBuilder::new(Encoding::SizedStored);
        for (i, encoding) in Encoding::ALL.into_iter().cycle().take(12).enumerate() {
            let n = 100 + i as u32;
            b.push_with(
                synthetic_id(n, 0),
                &synthetic_payload(n, 5000 + i),
                encoding,
            );
        }
        let log = open(&dir, &b);
        for (i, n) in (100..112).enumerate() {
            assert_eq!(inline(&log, n), synthetic_payload(n, 5000 + i));
        }
    }

    #[test]
    fn orphaned_fork_entries_are_never_read() {
        // The file holds 2 3 4 5 6 7 5': the old 5, 6 and 7 are orphans after a fork at 5.
        let dir = TestDir::new("shiplog-orphans");
        let mut b = chain(Encoding::SizedStored, 2..8, 300);
        let forked = synthetic_payload(5_000_005, 300);
        b.push(synthetic_id(5, 1), &forked);
        let log = open(&dir, &b);
        assert_eq!(log.range(), (2, 6));

        let h = log.header(5).unwrap();
        assert_eq!(h.id, synthetic_id(5, 1));
        assert_eq!(inline(&log, 5), forked);
        for n in 2..5 {
            assert_eq!(log.id(n).unwrap(), synthetic_id(n, 0));
        }
        for n in [6, 7] {
            let e = err(log.header(n));
            assert!(e.contains(&format!("block {n} is outside 2..6")), "{e}");
        }

        // A forward scan sees seven entries. Keeping the first entry of each block it serves
        // the abandoned 5; either way it reports blocks 6 and 7, which are gone.
        let bytes = b.log();
        let mut seen = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let id: [u8; 32] = bytes[pos + 8..pos + 40].try_into().unwrap();
            seen.push(id);
            pos += 56 + le_u64(&bytes[pos + 40..]) as usize;
        }
        let nums: Vec<u32> = seen.iter().map(num_from_id).collect();
        assert_eq!(nums, [2, 3, 4, 5, 6, 7, 5]);
        let first_5 = seen.iter().find(|id| num_from_id(id) == 5).unwrap();
        assert_eq!(*first_5, synthetic_id(5, 0));
        assert_ne!(*first_5, h.id);

        // Replacing more of the forked tail extends the range through the new entries only.
        b.push(synthetic_id(6, 1), &synthetic_payload(6, 10));
        let dir = TestDir::new("shiplog-orphans-2");
        let log = open(&dir, &b);
        assert_eq!(log.range(), (2, 7));
        assert_eq!(log.id(6).unwrap(), synthetic_id(6, 1));
        assert_eq!(log.id(5).unwrap(), synthetic_id(5, 1));
    }

    #[test]
    fn a_fork_at_the_first_block_keeps_its_number() {
        // Index slot 0 then points past offset 0, and the first block is still the one in the
        // first entry of the file.
        let dir = TestDir::new("shiplog-fork-first");
        let mut b = chain(Encoding::SizedStored, 2..5, 64);
        b.push(synthetic_id(2, 1), &synthetic_payload(2, 64));
        let log = open(&dir, &b);
        assert_eq!(log.range(), (2, 3));
        let h = log.header(2).unwrap();
        assert_eq!(h.id, synthetic_id(2, 1));
        assert_ne!(h.pos, 0);
    }

    #[test]
    fn an_index_size_mismatch_is_refused() {
        for slots in [3, 5, 0] {
            let dir = TestDir::new("shiplog-index-size");
            let mut b = chain(Encoding::SizedStored, 2..6, 64);
            b.slots_mut().resize(slots, 0);
            let e = err(try_open(&dir, &b));
            assert!(
                e.contains("index mismatch for") && e.contains("trace_history.log"),
                "{e}"
            );
            assert!(e.contains("need 32") && e.contains(INDEX_HINT), "{e}");
        }
    }

    #[test]
    fn an_index_whose_last_slot_misses_the_tail_is_refused() {
        let dir = TestDir::new("shiplog-index-tail");
        let mut b = chain(Encoding::SizedStored, 2..6, 64);
        let p4 = b.position(4);
        *b.slots_mut().last_mut().unwrap() = p4;
        let e = err(try_open(&dir, &b));
        assert!(
            e.contains("index mismatch") && e.contains("slot of block 5"),
            "{e}"
        );
    }

    #[test]
    fn a_missing_index_is_refused_and_a_sidecar_one_accepted() {
        let dir = TestDir::new("shiplog-index-missing");
        let b = chain(Encoding::SizedStored, 2..4, 64);
        let (log, _) = b.write(dir.path(), "trace_history").unwrap();
        let sidecar = dir.path().join("sidecar").join("trace_history.index");
        let e = err(ShipLog::open(&log, &sidecar, LogKind::Trace, &cache()));
        assert!(e.contains("index for") && e.contains(INDEX_HINT), "{e}");

        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b.index()).unwrap();
        let log = ShipLog::open(&log, &sidecar, LogKind::Trace, &cache())
            .unwrap()
            .unwrap();
        assert_eq!(log.index_path(), sidecar);
        assert_eq!(log.id(3).unwrap(), synthetic_id(3, 0));
    }

    #[test]
    fn a_corrupted_tail_is_refused() {
        let good = chain(Encoding::SizedStored, 2..6, 200);
        let partial = kit::entry(&synthetic_id(6, 0), &[1, 0, 0, 0], good.log().len() as u64);
        type Damage = Box<dyn Fn(&mut Vec<u8>)>;
        let cases: Vec<(&str, Damage)> = vec![
            ("cut by one byte", Box::new(|l| l.truncate(l.len() - 1))),
            ("cut by its trailer", Box::new(|l| l.truncate(l.len() - 8))),
            (
                "cut inside the last entry",
                Box::new(|l| l.truncate(l.len() - 100)),
            ),
            (
                "garbage appended",
                Box::new(|l| l.extend_from_slice(&[0xde, 0xad])),
            ),
            (
                "an entry without its trailer appended",
                Box::new(move |l| l.extend_from_slice(&partial[..partial.len() - 8])),
            ),
            (
                "a trailer past the end",
                Box::new(|l| {
                    let n = l.len();
                    l[n - 8..].copy_from_slice(&u64::MAX.to_le_bytes());
                }),
            ),
            ("shorter than an entry", Box::new(|l| l.truncate(30))),
        ];
        for (what, damage) in cases {
            let dir = TestDir::new("shiplog-tail");
            let mut b = good.clone();
            damage(b.log_mut());
            let e = err(try_open(&dir, &b));
            assert!(
                e.contains("corrupted tail; repair the log first"),
                "{what}: {e}"
            );
        }
    }

    #[test]
    fn a_last_entry_that_ends_early_is_refused() {
        // The last trailer names the last entry, which is whole, but bytes follow it.
        let dir = TestDir::new("shiplog-tail-early");
        let mut b = chain(Encoding::SizedStored, 2..4, 64);
        let pos = b.position(3);
        let log = b.log_mut();
        log.extend_from_slice(&[0; 8]);
        log.extend_from_slice(&pos.to_le_bytes());
        let e = err(try_open(&dir, &b));
        assert!(
            e.contains("corrupted tail") && e.contains("not at the end"),
            "{e}"
        );
    }

    #[test]
    fn pruned_and_version_1_logs_are_refused() {
        let cases = [
            (PRUNED_MAGIC, "pruned state-history log"),
            (V1_MAGIC, "version 1, written by EOSIO 2.1 or 2.2"),
            (kit::magic(0, 2), "version 0, features 0x2"),
        ];
        for (magic, why) in cases {
            let dir = TestDir::new("shiplog-magic");
            let mut b = chain(Encoding::SizedStored, 2..5, 64);
            b.set_magic(0, magic);
            let e = err(try_open(&dir, &b));
            assert!(
                e.contains("pruned or unsupported state-history log")
                    && e.contains("use a vacuumed, non-pruned copy")
                    && e.contains(why),
                "{e}"
            );
        }
        let dir = TestDir::new("shiplog-magic-none");
        let mut b = chain(Encoding::SizedStored, 2..5, 64);
        b.set_magic(0, 0x0123_4567_89ab_cdef);
        let e = err(try_open(&dir, &b));
        assert!(
            e.contains("is not a supported state-history log") && e.contains("no `ship` magic"),
            "{e}"
        );
        assert_eq!(unsupported_magic(SHIP_MAGIC), None);
    }

    #[test]
    fn a_wrong_block_number_or_trailer_is_detected() {
        let dir = TestDir::new("shiplog-entry");
        let mut b = chain(Encoding::SizedStored, 2..9, 200);
        let (p3, p4, p5) = (b.position(3), b.position(4), b.position(5));
        // Block 4's slot names block 5's entry, block 6's points inside block 5's entry, and
        // block 7's is past the end of the log.
        b.slots_mut()[2] = p5;
        b.slots_mut()[4] = p5 + 10;
        b.slots_mut()[5] = 1 << 40;
        // Block 3's trailer is wrong.
        let t3 = (p4 - 8) as usize;
        b.log_mut()[t3..t3 + 8].copy_from_slice(&7u64.to_le_bytes());
        let log = open(&dir, &b);

        let e = err(log.header(4));
        assert!(
            e.contains("block 4") && e.contains(&format!("offset {p5} is block 5")),
            "{e}"
        );
        assert!(err(log.id(4)).contains("is block 5"));
        let e = err(log.header(3));
        assert!(e.contains(&format!("offset {p3} holds 7")), "{e}");
        assert!(err(log.header(6)).contains("no entry header"));
        assert!(err(log.header(7)).contains("leaves no room for an entry"));
        assert!(err(log.header_at(5, p4)).contains("is block 4"));
        assert_eq!(log.header_at(5, p5).unwrap(), log.header(5).unwrap());
        assert!(err(log.header_at(9, p5)).contains("outside 2..9"));
        for n in [2, 5, 8] {
            assert_eq!(log.id(n).unwrap(), synthetic_id(n, 0));
        }
    }

    #[test]
    fn a_payload_size_past_the_end_or_short_of_a_preamble_is_detected() {
        let dir = TestDir::new("shiplog-payload-size");
        let mut b = chain(Encoding::SizedStored, 2..5, 64);
        b.push_encoded(synthetic_id(5, 0), &[1, 0, 0, 0, 9]);
        b.push_encoded(synthetic_id(6, 0), &[0, 0]);
        b.push_blocks(7..8, 64);
        let p3 = b.position(3) as usize;
        b.log_mut()[p3 + 40..p3 + 48].copy_from_slice(&(1u64 << 40).to_le_bytes());
        let log = open(&dir, &b);
        assert!(err(log.header(3)).contains("runs past the end"));
        assert!(err(log.header(5)).contains("shorter than its sized preamble"));
        assert!(err(log.header(6)).contains("shorter than its preamble"));
        assert_eq!(inline(&log, 7), synthetic_payload(7, 64));
    }

    #[test]
    fn a_legacy_size_field_must_match_the_stream() {
        let dir = TestDir::new("shiplog-legacy-field");
        let raw = synthetic_payload(3, 500);
        let z = kit::zlib_deflate(&raw);
        let zlen = z.len() as u32;
        let mut b = chain(Encoding::LegacyLength, 2..3, 64);
        b.push_encoded(synthetic_id(3, 0), &kit::legacy_payload(zlen + 1, &z));
        b.push_encoded(synthetic_id(4, 0), &kit::legacy_payload(zlen, &z));
        let log = open(&dir, &b);
        let e = err(log.header(3));
        assert!(
            e.contains("legacy size field") && e.contains("block 3"),
            "{e}"
        );
        assert_eq!(inline(&log, 4), raw);
    }

    #[test]
    fn a_bad_adler32_is_detected() {
        for encoding in Encoding::ALL {
            let dir = TestDir::new("shiplog-adler");
            let mut b = chain(encoding, 2..6, 70_000);
            b.corrupt_checksum(3);
            let log = open(&dir, &b);
            let h = log.header(3).unwrap();
            let e = err(log.read_inline(&h, u64::MAX, &mut Vec::new()));
            assert!(
                e.contains("trace_history.log: block 3 at offset") && e.contains("invalid zlib"),
                "{encoding:?}: {e}"
            );
            assert!(err(log.verify_count(&h, u64::MAX)).contains("invalid zlib"));
            // A stream hands out the data before the checksum fails at its end.
            let mut s = log.stream(&h).unwrap();
            let mut out = Vec::new();
            let e = loop {
                match s.next_chunk(&mut out, 4096) {
                    Ok(true) => {}
                    Ok(false) => panic!("{encoding:?}: the corrupt stream ended cleanly"),
                    Err(e) => break format!("{e:#}"),
                }
            };
            assert!(e.contains("invalid zlib"), "{e}");
            assert!(
                (32 << 10..=70_000).contains(&out.len()),
                "{encoding:?}: {}",
                out.len()
            );
            let e = err(s.next_chunk(&mut out, 4096));
            assert!(e.contains("an earlier read of this entry failed"), "{e}");
            // The neighbours are intact.
            assert_eq!(inline(&log, 2), synthetic_payload(2, 70_000));
            assert_eq!(inline(&log, 4), synthetic_payload(4, 70_000));
        }
    }

    #[test]
    fn truncated_padded_or_missized_streams_are_refused() {
        let dir = TestDir::new("shiplog-streams");
        let raw = synthetic_payload(9, 40_000);
        let len = raw.len() as u64;
        let stored = kit::zlib_stored(&raw);
        let deflated = kit::zlib_deflate(&raw);
        let mut padded = stored.clone();
        padded.extend_from_slice(&[0, 0, 0]);
        let cases = [
            (
                kit::sized_payload(len, &stored[..stored.len() - 5]),
                "stops after",
            ),
            (
                kit::legacy_payload(0, &deflated[..deflated.len() / 2]),
                "stops after",
            ),
            (
                kit::sized_payload(len, &padded),
                "3 bytes of the payload follow the end of its zlib stream",
            ),
            (
                kit::sized_payload(len - 1, &stored),
                "inflates past the 39999 bytes its header declares",
            ),
            (
                kit::sized_payload(len + 1, &stored),
                "inflates to 40000 bytes, its header declares 40001",
            ),
            (
                kit::legacy_payload(0, &[0x78, 0x9c, 0xff, 0xff]),
                "invalid zlib payload",
            ),
        ];
        let mut b = LogBuilder::new(Encoding::SizedStored);
        for (i, (payload, _)) in cases.iter().enumerate() {
            b.push_encoded(synthetic_id(2 + i as u32, 0), payload);
        }
        let log = open(&dir, &b);
        for (i, (_, want)) in cases.into_iter().enumerate() {
            let n = 2 + i as u32;
            let h = log.header(n).unwrap();
            for e in [
                err(log.read_inline(&h, u64::MAX, &mut Vec::new())),
                err(log.verify_count(&h, u64::MAX)),
                stream_error(&log, &h, 1 << 20).0,
            ] {
                assert!(
                    e.contains(want) && e.contains(&format!("block {n} at offset")),
                    "{e}"
                );
            }
        }
    }

    #[test]
    fn a_declared_size_of_u64_max_fails_closed() {
        // A size field reading u64::MAX over an otherwise valid stream, stored and deflated.
        let dir = TestDir::new("shiplog-max-size");
        let raw = synthetic_payload(3, 1000);
        let mut b = LogBuilder::new(Encoding::SizedStored);
        b.push_encoded(
            synthetic_id(2, 0),
            &kit::sized_payload(u64::MAX, &kit::zlib_stored(&raw)),
        );
        b.push_encoded(
            synthetic_id(3, 0),
            &kit::sized_payload(u64::MAX, &kit::zlib_deflate(&raw)),
        );
        let log = Arc::new(open(&dir, &b));
        let want = "inflates to 1000 bytes, its header declares 18446744073709551615";
        for n in [2, 3] {
            let h = log.header(n).unwrap();
            assert_eq!(h.uncompressed(), Some(u64::MAX));
            // Counting with the declared size as the limit (a payload to stream) and with no
            // limit (a deep check) must end, not spin: on a thread, so a regression fails
            // here instead of hanging the run.
            for limit in [u64::MAX, u64::MAX - 1] {
                let (tx, rx) = std::sync::mpsc::channel();
                let counting = Arc::clone(&log);
                std::thread::spawn(move || {
                    let _ = tx.send(
                        counting
                            .verify_count(&h, limit)
                            .map_err(|e| format!("{e:#}")),
                    );
                });
                let r = rx
                    .recv_timeout(std::time::Duration::from_secs(20))
                    .unwrap_or_else(|_| panic!("block {n}: verify_count spins"));
                let e = r.expect_err("a size the stream does not have");
                if limit == u64::MAX {
                    assert!(e.contains(want) && e.contains(&format!("block {n}")), "{e}");
                } else {
                    assert!(
                        e.contains("over the 18446744073709551614-byte limit"),
                        "{e}"
                    );
                }
            }
            let e = err(log.read_inline(&h, u64::MAX, &mut Vec::new()));
            assert!(e.contains(&format!("block {n} at offset")), "{e}");
            assert!(err(log.read_inline(&h, 1 << 20, &mut Vec::new())).contains("over the"));
            assert!(stream_error(&log, &h, 1 << 16).0.contains(want));
        }
    }

    #[test]
    fn a_stream_never_inflates_into_an_empty_buffer() {
        let dir = TestDir::new("shiplog-empty-buffer");
        let log = open(&dir, &chain(Encoding::SizedStored, 2..4, 1000));
        let mut s = log.stream(&log.header(2).unwrap()).unwrap();
        let e = format!("{:#}", s.inflate(&mut []).unwrap_err());
        assert!(e.contains("no room to inflate"), "{e}");
        // An empty payload still ends, with nothing to write: a Type A one even when asked for
        // no bytes at all, as every declared byte is already out.
        let dir = TestDir::new("shiplog-empty-payload");
        for encoding in Encoding::ALL {
            let log = open(&dir, &chain(encoding, 2..3, 0));
            let h = log.header(2).unwrap();
            let max = usize::from(h.uncompressed().is_none());
            assert_eq!(chunks(&log, &h, max), [Vec::<u8>::new()]);
            assert_eq!(log.verify_count(&h, 0).unwrap(), 0);
        }
    }

    #[test]
    fn the_inline_limit_is_enforced() {
        for encoding in Encoding::ALL {
            let dir = TestDir::new("shiplog-limit");
            let mut b = chain(encoding, 2..4, 50_000);
            b.corrupt_checksum(3);
            let log = open(&dir, &b);
            let h = log.header(2).unwrap();
            let mut out = Vec::new();
            log.read_inline(&h, 50_000, &mut out).unwrap();
            assert_eq!(out, synthetic_payload(2, 50_000));
            assert_eq!(log.verify_count(&h, 50_000).unwrap(), 50_000);

            let e = err(log.read_inline(&h, 49_999, &mut out));
            assert!(
                e.contains("49999-byte limit") && e.contains("block 2"),
                "{e}"
            );
            assert!(out.len() <= 50_000);
            assert!(err(log.verify_count(&h, 49_999)).contains("49999-byte limit"));
            assert!(err(log.read_inline(&h, 0, &mut out)).contains("0-byte limit"));

            // A declared size over the limit is refused before the stream is read, so block
            // 3's broken checksum is never reached.
            let h3 = log.header(3).unwrap();
            let e = err(log.read_inline(&h3, 1000, &mut out));
            if h3.uncompressed().is_some() {
                assert!(
                    e.contains("inflates to 50000 bytes, over the 1000-byte"),
                    "{e}"
                );
                assert!(err(log.verify_count(&h3, 1000)).contains("over the 1000-byte"));
            } else {
                assert!(e.contains("more than the 1000-byte limit"), "{e}");
            }
        }
    }

    #[test]
    fn stream_chunks_concatenate_to_read_inline() {
        // 2.5 MiB entries span several input windows; the small ones are chunked byte by byte.
        let sizes: [(u32, usize); 3] = [(2, 2_500_000), (3, 777), (4, 0)];
        for encoding in Encoding::ALL {
            let dir = TestDir::new("shiplog-chunks");
            let mut b = LogBuilder::new(encoding);
            for (n, len) in sizes {
                b.push(synthetic_id(n, 0), &synthetic_payload(n, len));
            }
            let log = open(&dir, &b);
            for (n, len) in sizes {
                let h = log.header(n).unwrap();
                let whole = inline(&log, n);
                assert_eq!(whole.len(), len);
                assert_eq!(log.verify_count(&h, u64::MAX).unwrap(), len as u64);
                let maxes: &[usize] = if len < 1000 {
                    &[1, 7, 1 << 20]
                } else {
                    &[7_777, 65_536, 1 << 20, 3 << 20]
                };
                for &max in maxes {
                    let parts = chunks(&log, &h, max);
                    assert_eq!(parts.concat(), whole, "{encoding:?} block {n} max {max}");
                    if h.uncompressed().is_some() {
                        // A Type A stream ends on its last bytes, never on an extra empty call.
                        assert_eq!(parts.len(), len.div_ceil(max).max(1), "{encoding:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_log_that_changes_after_open_is_refused() {
        let dir = TestDir::new("shiplog-changed");
        let b = chain(Encoding::SizedStored, 2..5, 64);
        let (path, index) = b.write(dir.path(), "chain_state_history").unwrap();
        // With one open file, every read reopens the log after the index evicted it.
        let log = ShipLog::open(&path, &index, LogKind::ChainState, &FileCache::new(1))
            .unwrap()
            .unwrap();
        assert_eq!(log.id(3).unwrap(), synthetic_id(3, 0));
        let mut grown = b.log().to_vec();
        grown.extend_from_slice(&[0; 16]);
        std::fs::write(&path, grown).unwrap();
        let e = err(log.header(3));
        assert!(e.contains("changed after it was opened"), "{e}");
    }

    #[test]
    fn positions_reads_index_slots_in_range_only() {
        let dir = TestDir::new("shiplog-positions");
        let b = chain(Encoding::SizedDeflate, 1000..1100, 16);
        let log = open(&dir, &b);
        let mut out = vec![9; 3];
        log.positions(1040, 20, &mut out).unwrap();
        assert_eq!(out, (1040..1060).map(|n| b.position(n)).collect::<Vec<_>>());
        log.positions(1100, 0, &mut out).unwrap();
        assert!(out.is_empty());
        for (from, count) in [(999, 1), (1099, 2), (1050, u32::MAX)] {
            let e = err(log.positions(from, count, &mut out));
            assert!(e.contains("are not all in 1000..1100"), "{e}");
        }
        for n in [1000, 1099] {
            let p = b.position(n);
            assert_eq!(log.header_at(n, p).unwrap().pos, p);
        }
    }

    #[test]
    fn partitioned_logs_rotate_on_stride_multiples() {
        let dir = TestDir::new("shiplog-partitioned");
        let retained = dir.path().join("retained");
        let entries = |r: Range<u32>| r.map(|n| (synthetic_id(n, 0), synthetic_payload(n, 40)));
        let files = kit::write_partitioned(
            dir.path(),
            &retained,
            "trace_history",
            5,
            Encoding::SizedStored,
            entries(2..18),
        )
        .unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|(log, _)| log.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "trace_history-2-5.log",
                "trace_history-6-10.log",
                "trace_history-11-15.log",
                "trace_history.log"
            ]
        );
        assert!(files[0].0.starts_with(&retained) && files[3].1.starts_with(dir.path()));
        let mut ranges = Vec::new();
        for (path, index) in &files {
            let log = ShipLog::open(path, index, LogKind::Trace, &cache())
                .unwrap()
                .unwrap();
            let (begin, end) = log.range();
            for n in begin..end {
                assert_eq!(inline(&log, n), synthetic_payload(n, 40));
            }
            ranges.push((begin, end));
        }
        assert_eq!(ranges, [(2, 6), (6, 11), (11, 16), (16, 18)]);

        // Ending on a multiple leaves a 0-byte head log.
        let dir = TestDir::new("shiplog-partitioned-rotated");
        let files = kit::write_partitioned(
            dir.path(),
            dir.path(),
            "chain_state_history",
            5,
            Encoding::SizedDeflate,
            entries(3..11),
        )
        .unwrap();
        assert_eq!(files.len(), 3);
        assert!(files[1].0.ends_with("chain_state_history-6-10.log"));
        let (path, index) = files.last().unwrap();
        let head = ShipLog::open(path, index, LogKind::ChainState, &cache()).unwrap();
        assert!(head.is_none());
    }

    /// Real entries, each the 48-byte header and the payload without the trailer: Leap 4/5
    /// compressed (EOS and WAX) and Spring stored (EOS).
    const REAL: [(&str, &[u8]); 7] = [
        (
            "eos-trace-deflate-50000085",
            include_bytes!("../testdata/ship/eos-trace-deflate-50000085.bin"),
        ),
        (
            "eos-trace-deflate-large-100000002",
            include_bytes!("../testdata/ship/eos-trace-deflate-large-100000002.bin"),
        ),
        (
            "eos-state-deflate-300000054",
            include_bytes!("../testdata/ship/eos-state-deflate-300000054.bin"),
        ),
        (
            "eos-trace-stored-480000070",
            include_bytes!("../testdata/ship/eos-trace-stored-480000070.bin"),
        ),
        (
            "eos-state-stored-525000000",
            include_bytes!("../testdata/ship/eos-state-stored-525000000.bin"),
        ),
        (
            "wax-trace-deflate-432000128",
            include_bytes!("../testdata/ship/wax-trace-deflate-432000128.bin"),
        ),
        (
            "wax-state-deflate-432001551",
            include_bytes!("../testdata/ship/wax-state-deflate-432001551.bin"),
        ),
    ];

    #[test]
    fn real_entries_read_back_through_the_strict_reader() {
        for (name, bytes) in REAL {
            let n: u32 = name.rsplit('-').next().unwrap().parse().unwrap();
            let id: [u8; 32] = bytes[8..40].try_into().unwrap();
            assert_eq!(num_from_id(&id), n, "{name}");
            let mut b = LogBuilder::new(Encoding::SizedStored);
            b.push_encoded(id, &bytes[48..]);
            // The writer lays the entry out exactly as nodeos did.
            assert_eq!(&b.log()[..bytes.len()], bytes, "{name}");

            let dir = TestDir::new("shiplog-real");
            let kind = if name.contains("-state-") {
                LogKind::ChainState
            } else {
                LogKind::Trace
            };
            let (path, index) = b.write(dir.path(), kind.stem()).unwrap();
            let log = ShipLog::open(&path, &index, kind, &cache())
                .unwrap()
                .unwrap();
            assert_eq!(log.range(), (n, n + 1), "{name}");
            let h = log.header(n).unwrap();
            assert_eq!(h.id, id);
            let size = h
                .uncompressed()
                .expect("real entries carry the sized preamble");
            let raw = inline(&log, n);
            assert_eq!(raw.len() as u64, size, "{name}");
            assert_eq!(log.verify_count(&h, size).unwrap(), size);
            assert_eq!(chunks(&log, &h, 4096).concat(), raw, "{name}");

            let data = &bytes[60..];
            if name.contains("-stored-") {
                // Spring's level-0 stream is exactly what the fixture writer produces.
                assert_eq!(data, kit::zlib_stored(&raw), "{name}");
            } else {
                assert_eq!(data[..2], [0x78, 0x9c], "{name}");
            }
        }
    }

    #[test]
    fn the_stored_fixture_encoding_is_valid_zlib() {
        // Stored blocks are cut at 32 KiB, only the last is final, and adler32 matches zlib's.
        for len in [0, 1, 32 << 10, (32 << 10) + 1, 64 << 10, 100_000] {
            let raw = synthetic_payload(len as u32, len);
            let z = kit::zlib_stored(&raw);
            let mut d = Decompress::new(true);
            let mut out = vec![0; len + 1];
            let status = d.decompress(&z, &mut out, FlushDecompress::Finish).unwrap();
            assert_eq!(status, Status::StreamEnd);
            assert_eq!(d.total_in(), z.len() as u64);
            assert_eq!(out[..d.total_out() as usize], raw[..]);
            assert_eq!(z.len(), 2 + len.div_ceil(32 << 10).max(1) * 5 + len + 4);
        }
        assert_eq!(kit::adler32(b"Wikipedia"), 0x11e6_0398);
        assert_eq!(kit::adler32(&[]), 1);
    }
}
