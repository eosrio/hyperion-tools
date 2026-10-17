//! State-history log writers for synthetic fixtures.
//!
//! [`LogBuilder`] lays out a log and its index the way Spring's writer does: entries are only
//! ever appended, and writing a block below the current end (a fork) truncates the index there,
//! leaving the abandoned entries in the file as orphans. Payloads are framed and compressed in
//! any of the encodings nodeos has written ([`Encoding`]). Faults (pruned or version-1 magic,
//! broken tails, bad checksums, wrong index slots) are applied to the bytes before writing:
//! [`LogBuilder::set_magic`], [`LogBuilder::corrupt_checksum`], [`LogBuilder::log_mut`] and
//! [`LogBuilder::slots_mut`]. [`write_partitioned`] writes the retained files of a split log.

use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use flate2::Compression;
use flate2::write::ZlibEncoder;

use crate::binio::num_from_id;
use crate::shiplog::{HEADER_LEN, SHIP_MAGIC};

/// How an entry payload is framed and compressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoding {
    /// Spring 1.x: preamble `1` and the u64 uncompressed size, then zlib level 0 (stored deflate
    /// blocks of 32 KiB), byte for byte as Spring writes it.
    SizedStored,
    /// Leap 4 and 5: the same preamble, then compressed zlib.
    SizedDeflate,
    /// EOSIO 1.8 and 2.0: preamble u32 holding the zlib length, then compressed zlib.
    LegacyLength,
    /// The legacy framing with a u32 `0` preamble, then compressed zlib.
    LegacyZero,
}

impl Encoding {
    pub const ALL: [Self; 4] = [
        Self::SizedStored,
        Self::SizedDeflate,
        Self::LegacyLength,
        Self::LegacyZero,
    ];

    /// The `payload_size` bytes of an entry holding `raw`.
    pub fn payload(self, raw: &[u8]) -> Vec<u8> {
        match self {
            Self::SizedStored => sized_payload(raw.len() as u64, &zlib_stored(raw)),
            Self::SizedDeflate => sized_payload(raw.len() as u64, &zlib_deflate(raw)),
            Self::LegacyLength => {
                let z = zlib_deflate(raw);
                let len = u32::try_from(z.len()).expect("legacy entries are under 4 GiB");
                legacy_payload(len, &z)
            }
            Self::LegacyZero => legacy_payload(0, &zlib_deflate(raw)),
        }
    }
}

/// An entry magic: the name `ship` with `version` and `features` (bit 0: pruned).
pub const fn magic(version: u16, features: u16) -> u64 {
    SHIP_MAGIC | (version as u64) | ((features as u64) << 16)
}

/// The first-entry magic of a pruned log (`state-history-log-retain-blocks`).
pub const PRUNED_MAGIC: u64 = magic(0, 1);
/// The magic of a version-1 log (EOSIO 2.1 and 2.2).
pub const V1_MAGIC: u64 = magic(1, 0);

/// A block id for block `n` on fork `fork`: the block number big-endian, then filler that
/// differs between forks. Not a hash of anything.
pub fn synthetic_id(n: u32, fork: u8) -> [u8; 32] {
    let mut id = [0; 32];
    id[..4].copy_from_slice(&n.to_be_bytes());
    id[4] = fork;
    let seed = (n as u8).wrapping_mul(29) ^ fork.wrapping_mul(101);
    for (i, b) in id.iter_mut().enumerate().skip(5) {
        *b = seed.wrapping_add(i as u8);
    }
    id
}

/// `len` deterministic payload bytes for block `n`, partly random so that compression is real
/// but not total.
pub fn synthetic_payload(n: u32, len: usize) -> Vec<u8> {
    let mut x = u64::from(n).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|i| {
            if i % 3 == 0 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 56) as u8
            } else {
                (i / 3) as u8
            }
        })
        .collect()
}

/// zlib level 0 the way Spring writes it: stored deflate blocks of 32 KiB, the last one final,
/// and the adler32 trailer.
pub fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    const BLOCK: usize = 32 << 10;
    let mut out = Vec::with_capacity(raw.len() + (raw.len() / BLOCK + 2) * 5 + 6);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = raw.chunks(BLOCK).peekable();
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    if raw.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Compressed zlib at the default level. Valid zlib, though not necessarily the bytes stock
/// zlib would produce.
pub fn zlib_deflate(raw: &[u8]) -> Vec<u8> {
    let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
    z.write_all(raw).expect("in-memory zlib");
    z.finish().expect("in-memory zlib")
}

/// The zlib checksum of `data`.
pub fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in data.chunks(5552) {
        for &byte in chunk {
            a += u32::from(byte);
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// A Type A payload: u32 `1`, the u64 `uncompressed` size (which need not be true), the stream.
pub fn sized_payload(uncompressed: u64, zlib: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + zlib.len());
    p.extend_from_slice(&1u32.to_le_bytes());
    p.extend_from_slice(&uncompressed.to_le_bytes());
    p.extend_from_slice(zlib);
    p
}

/// A Type B payload: the u32 `field` (the zlib length, or 0), then the stream.
pub fn legacy_payload(field: u32, zlib: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(4 + zlib.len());
    p.extend_from_slice(&field.to_le_bytes());
    p.extend_from_slice(zlib);
    p
}

/// One whole entry at offset `pos`: header, `payload` and the position trailer.
pub fn entry(id: &[u8; 32], payload: &[u8], pos: u64) -> Vec<u8> {
    let mut e = Vec::with_capacity(payload.len() + 56);
    e.extend_from_slice(&SHIP_MAGIC.to_le_bytes());
    e.extend_from_slice(id);
    e.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    e.extend_from_slice(payload);
    e.extend_from_slice(&pos.to_le_bytes());
    e
}

/// The stem of a retained file holding blocks `first..=last`: `trace_history-2-20`.
pub fn retained_stem(stem: &str, first: u32, last: u32) -> String {
    format!("{stem}-{first}-{last}")
}

/// A log and its index under construction, in memory.
#[derive(Clone, Debug)]
pub struct LogBuilder {
    log: Vec<u8>,
    /// Slot k is the offset of the entry of block `begin + k`.
    slots: Vec<u64>,
    /// The block of the first entry in the file, fixed once it is written (as in Spring).
    begin: Option<u32>,
    encoding: Encoding,
}

impl LogBuilder {
    /// An empty log whose entries are written in `encoding` unless a push says otherwise.
    pub fn new(encoding: Encoding) -> Self {
        Self {
            log: Vec::new(),
            slots: Vec::new(),
            begin: None,
            encoding,
        }
    }

    /// Append the entry of block `num(id)` holding `raw`.
    pub fn push(&mut self, id: [u8; 32], raw: &[u8]) -> &mut Self {
        self.push_with(id, raw, self.encoding)
    }

    /// Append the entry of block `num(id)` holding `raw`, written in `encoding`.
    pub fn push_with(&mut self, id: [u8; 32], raw: &[u8], encoding: Encoding) -> &mut Self {
        self.push_encoded(id, &encoding.payload(raw))
    }

    /// Append an entry with these exact payload bytes (preamble and zlib stream), as Spring's
    /// writer would: the block must be at most one past the current end, and a block below the
    /// end is a fork that drops the index slots from it on. The old entries stay in the file.
    pub fn push_encoded(&mut self, id: [u8; 32], payload: &[u8]) -> &mut Self {
        let n = num_from_id(&id);
        let begin = *self.begin.get_or_insert(n);
        let end = u64::from(begin) + self.slots.len() as u64;
        assert!(
            n >= begin && u64::from(n) <= end,
            "block {n} is not in {begin}..={end}: a log has no gaps"
        );
        self.slots.truncate((n - begin) as usize);
        let pos = self.log.len() as u64;
        self.log.extend_from_slice(&entry(&id, payload, pos));
        self.slots.push(pos);
        self
    }

    /// Append blocks `blocks` on fork 0, with [`synthetic_id`]s and [`synthetic_payload`]s of
    /// `len` bytes.
    pub fn push_blocks(&mut self, blocks: Range<u32>, len: usize) -> &mut Self {
        for n in blocks {
            self.push(synthetic_id(n, 0), &synthetic_payload(n, len));
        }
        self
    }

    /// The blocks the index covers, `[begin, end)`; `(0, 0)` while nothing is written.
    pub fn range(&self) -> (u32, u32) {
        match self.begin {
            Some(b) => (b, b + self.slots.len() as u32),
            None => (0, 0),
        }
    }

    /// The offset of block `n`'s current entry.
    pub fn position(&self, n: u32) -> u64 {
        let (begin, end) = self.range();
        assert!(
            (begin..end).contains(&n),
            "block {n} is not in {begin}..{end}"
        );
        self.slots[(n - begin) as usize]
    }

    /// Overwrite the magic of the entry at offset `pos`.
    pub fn set_magic(&mut self, pos: u64, magic: u64) -> &mut Self {
        let pos = pos as usize;
        self.log[pos..pos + 8].copy_from_slice(&magic.to_le_bytes());
        self
    }

    /// Break the adler32 of block `n`'s zlib stream (its last four payload bytes), leaving the
    /// framing intact: only inflating the whole stream notices.
    pub fn corrupt_checksum(&mut self, n: u32) -> &mut Self {
        let pos = self.position(n) as usize;
        let header = HEADER_LEN as usize;
        let size = u64::from_le_bytes(self.log[pos + 40..pos + header].try_into().unwrap());
        self.log[pos + header + size as usize - 1] ^= 0xff;
        self
    }

    pub fn log(&self) -> &[u8] {
        &self.log
    }

    /// The log bytes, for truncating, extending or patching.
    pub fn log_mut(&mut self) -> &mut Vec<u8> {
        &mut self.log
    }

    /// The index slots, for patching what [`Self::index`] writes.
    pub fn slots_mut(&mut self) -> &mut Vec<u64> {
        &mut self.slots
    }

    /// The index file's bytes.
    pub fn index(&self) -> Vec<u8> {
        self.slots.iter().flat_map(|p| p.to_le_bytes()).collect()
    }

    /// Write `<dir>/<stem>.log` and `<dir>/<stem>.index`, creating `dir`. Returns both paths.
    pub fn write(&self, dir: &Path, stem: &str) -> io::Result<(PathBuf, PathBuf)> {
        fs::create_dir_all(dir)?;
        let log = dir.join(format!("{stem}.log"));
        let index = dir.join(format!("{stem}.index"));
        fs::write(&log, &self.log)?;
        fs::write(&index, self.index())?;
        Ok((log, index))
    }
}

/// Write `entries` in Spring's partitioned layout (`state-history-stride`): after block n is
/// written the head log rotates whenever n is a multiple of `stride`, into
/// `<retained_dir>/<stem>-<first>-<n>.{log,index}`. The blocks after the last multiple stay in
/// `<head_dir>/<stem>.{log,index}`, which is 0 bytes right after a rotation. Returns every
/// (log, index) pair written, the retained files first and the head log last.
pub fn write_partitioned<I, P>(
    head_dir: &Path,
    retained_dir: &Path,
    stem: &str,
    stride: u32,
    encoding: Encoding,
    entries: I,
) -> io::Result<Vec<(PathBuf, PathBuf)>>
where
    I: IntoIterator<Item = ([u8; 32], P)>,
    P: AsRef<[u8]>,
{
    assert!(stride > 0, "a stride of 0 never rotates");
    let mut files = Vec::new();
    let mut head = LogBuilder::new(encoding);
    for (id, raw) in entries {
        head.push(id, raw.as_ref());
        let n = num_from_id(&id);
        if n.is_multiple_of(stride) {
            let (first, end) = head.range();
            files.push(head.write(retained_dir, &retained_stem(stem, first, end - 1))?);
            head = LogBuilder::new(encoding);
        }
    }
    files.push(head.write(head_dir, stem)?);
    Ok(files)
}
