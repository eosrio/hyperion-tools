//! Restricted Antelope formats. Fail closed for unknown versions, pruning, or framing.
use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use flate2::{Decompress, FlushDecompress, Status};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::io::{Disk, MAX_BYTES};

pub const SHIP_MAGIC: u64 = 0xc35d_5000_0000_0000;
pub const MAX_BLOCKS: u32 = 10_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Blocks,
    Trace,
    ChainState,
}

impl Kind {
    pub fn from_path(path: &Path) -> Result<Self> {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .context("log needs a UTF-8 stem")?;
        if stem == "blocks" || stem.starts_with("blocks-") {
            Ok(Self::Blocks)
        } else if stem == "trace_history" || stem.starts_with("trace_history-") {
            Ok(Self::Trace)
        } else if stem == "chain_state_history" || stem.starts_with("chain_state_history-") {
            Ok(Self::ChainState)
        } else {
            bail!(
                "expected blocks.log, trace_history.log, chain_state_history.log, or a retained range log"
            )
        }
    }
    pub fn stem(self) -> &'static str {
        match self {
            Self::Blocks => "blocks",
            Self::Trace => "trace_history",
            Self::ChainState => "chain_state_history",
        }
    }
}

pub fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
pub fn u64le(b: &[u8]) -> u64 {
    u64::from_le_bytes(b[..8].try_into().unwrap())
}
pub fn num(id: &[u8; 32]) -> u32 {
    u32::from_be_bytes(id[..4].try_into().unwrap())
}

pub struct Cursor<'a> {
    pub bytes: &'a [u8],
    pub pos: usize,
}
impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).context("length overflow")?;
        ensure!(
            end <= self.bytes.len(),
            "truncated binary data at {}",
            self.pos
        );
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }
    pub fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    pub fn var(&mut self) -> Result<u32> {
        let mut v = 0;
        for shift in (0..35).step_by(7) {
            let b = self.byte()?;
            ensure!(shift != 28 || b <= 15, "varuint32 overflow");
            v |= u32::from(b & 127) << shift;
            if b & 128 == 0 {
                ensure!(shift == 0 || b != 0, "noncanonical varuint32");
                return Ok(v);
            }
        }
        bail!("varuint32 overflow")
    }
    pub fn id(&mut self) -> Result<[u8; 32]> {
        Ok(self.take(32)?.try_into()?)
    }
}

#[derive(Debug, Clone)]
pub struct Header {
    pub number: u32,
    pub id: [u8; 32],
    pub previous: [u8; 32],
}
pub fn block_header(body: &[u8]) -> Result<Header> {
    let mut c = Cursor::new(body);
    c.take(14)?;
    let previous = c.id()?;
    c.take(68)?;
    match c.byte()? {
        0 => (),
        1 => {
            c.take(4)?;
            let count = c.var()?;
            ensure!(count <= 4096, "too many producers");
            for _ in 0..count {
                c.take(8)?;
                ensure!(c.var()? <= 1, "unsupported producer public key");
                c.take(33)?;
            }
        }
        _ => bail!("invalid producer optional flag"),
    }
    let count = c.var()?;
    ensure!(count <= 4096, "too many header extensions");
    for _ in 0..count {
        c.take(2)?;
        let n = c.var()?;
        c.take(n as usize)?;
    }
    let number = num(&previous)
        .checked_add(1)
        .context("block number overflow")?;
    let mut id: [u8; 32] = Sha256::digest(&body[..c.pos]).into();
    id[..4].copy_from_slice(&number.to_be_bytes());
    Ok(Header {
        number,
        id,
        previous,
    })
}

pub struct Log {
    pub log: Disk,
    pub index: Disk,
    pub kind: Kind,
    pub first: u32,
    pub last: u32,
    pub size: u64,
}
impl Log {
    pub fn open(path: &Path, index: Option<&Path>, writable: bool) -> Result<Self> {
        let kind = Kind::from_path(path)?;
        let log = Disk::open(path, writable)?;
        let index = Disk::open(
            &index
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.with_extension("index")),
            false,
        )?;
        let size = log.stamp()?.size;
        let count_bytes = index.stamp()?.size;
        ensure!(
            count_bytes > 0 && count_bytes % 8 == 0,
            "index is empty or truncated"
        );
        let first = if kind == Kind::Blocks {
            let h = log.read(0, 8)?;
            ensure!(u32le(&h) == 3, "only non-pruned blocks.log v3 is supported");
            u32le(&h[4..])
        } else {
            let h = log.read(0, 48)?;
            ensure!(
                u64le(&h) == SHIP_MAGIC,
                "only non-pruned SHIP v0 without feature flags is supported"
            );
            u32::from_be_bytes(h[8..12].try_into()?)
        };
        ensure!(first > 0, "invalid first block");
        let last = u64::from(first)
            .checked_add(count_bytes / 8 - 1)
            .context("index range overflow")?;
        ensure!(
            last < u64::from(u32::MAX),
            "index range overflows block numbers"
        );
        let out = Self {
            log,
            index,
            kind,
            first,
            last: last as u32,
            size,
        };
        let first_pos = out.pos(first)?;
        ensure!(
            if kind == Kind::Blocks {
                first_pos >= 48
            } else {
                first_pos == 0
            },
            "invalid first index position"
        );
        if kind == Kind::Blocks {
            ensure!(
                out.log.read(first_pos - 8, 8)? == [255; 8],
                "blocks.log header separator missing"
            );
        }
        out.bounds(out.last)?;
        Ok(out)
    }
    pub fn pos(&self, n: u32) -> Result<u64> {
        if n == self.last + 1 {
            return Ok(self.size);
        }
        ensure!(
            (self.first..=self.last).contains(&n),
            "block {n} outside {}..{}",
            self.first,
            self.last
        );
        let p = u64le(&self.index.read(u64::from(n - self.first) * 8, 8)?);
        ensure!(
            p < self.size,
            "index position past end of log for block {n}"
        );
        Ok(p)
    }
    pub fn bounds(&self, n: u32) -> Result<(u64, u64)> {
        let (a, b) = (self.pos(n)?, self.pos(n + 1)?);
        ensure!(
            b > a && b - a >= if self.kind == Kind::Blocks { 124 } else { 60 },
            "invalid index entry for block {n}"
        );
        ensure!(b - a <= MAX_BYTES, "block {n} exceeds 256 MiB safety limit");
        Ok((a, b))
    }
    pub fn body(&self, n: u32) -> Result<Vec<u8>> {
        let (a, b) = self.bounds(n)?;
        let mut bytes = self.log.read(a, b - a)?;
        ensure!(
            u64le(&bytes[bytes.len() - 8..]) == a,
            "position trailer mismatch at block {n}"
        );
        bytes.truncate(bytes.len() - 8);
        self.id(n, &bytes)?;
        Ok(bytes)
    }
    /// SHIP entry ID from its framing alone: header and position trailer, payload unread.
    /// A replayed donor's first entry holds the full chain state, which can far exceed
    /// the per-entry read limit, yet only its ID is needed to anchor the donor.
    pub fn ship_entry_id(&self, n: u32) -> Result<[u8; 32]> {
        ensure!(
            self.kind != Kind::Blocks,
            "block logs have no SHIP entry header"
        );
        let (a, b) = (self.pos(n)?, self.pos(n + 1)?);
        ensure!(b > a && b - a >= 60, "invalid index entry for block {n}");
        let h = self.log.read(a, 48)?;
        ensure!(
            u64le(&h) == SHIP_MAGIC,
            "unsupported SHIP header at block {n}"
        );
        ensure!(
            u64le(&h[40..]) == b - a - 56,
            "SHIP payload_size disagrees with index at block {n}"
        );
        ensure!(
            u64le(&self.log.read(b - 8, 8)?) == a,
            "position trailer mismatch at block {n}"
        );
        let id: [u8; 32] = h[8..40].try_into()?;
        ensure!(num(&id) == n, "SHIP entry {n} has wrong block number");
        Ok(id)
    }
    pub fn id(&self, n: u32, body: &[u8]) -> Result<[u8; 32]> {
        if self.kind == Kind::Blocks {
            let h = block_header(body)?;
            ensure!(h.number == n, "block {n} header has wrong number");
            Ok(h.id)
        } else {
            let (id, _) = ship_payload(body)?;
            ensure!(num(&id) == n, "SHIP entry {n} has wrong block number");
            Ok(id)
        }
    }
}

/// Verify framing, Adler checksum, exact compressed consumption, and bounded expansion.
pub fn ship_payload(body: &[u8]) -> Result<([u8; 32], Vec<u8>)> {
    ensure!(
        body.len() >= 52 && u64le(body) == SHIP_MAGIC,
        "unsupported SHIP header"
    );
    let size = u64le(&body[40..]);
    ensure!(
        size == (body.len() - 48) as u64,
        "SHIP payload_size disagrees with index"
    );
    let marker = u32le(&body[48..]);
    let start = if marker == 1 {
        ensure!(body.len() >= 60, "truncated SHIP size header");
        60
    } else {
        52
    };
    let expected = if marker == 1 {
        Some(u64le(&body[52..]))
    } else {
        None
    };
    ensure!(
        expected.is_none_or(|s| s <= MAX_BYTES),
        "SHIP expansion exceeds 256 MiB limit"
    );
    let mut z = Decompress::new(true);
    let mut raw = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        let (input_before, output_before) = (z.total_in(), z.total_out());
        let status = z
            .decompress(
                &body[start + input_before as usize..],
                &mut chunk,
                FlushDecompress::None,
            )
            .context("invalid SHIP zlib payload")?;
        let produced = (z.total_out() - output_before) as usize;
        ensure!(
            raw.len() as u64 + produced as u64 <= MAX_BYTES,
            "SHIP decompression limit exceeded"
        );
        raw.extend_from_slice(&chunk[..produced]);
        if status == Status::StreamEnd {
            break;
        }
        ensure!(
            z.total_in() > input_before || produced > 0,
            "incomplete SHIP zlib stream"
        );
    }
    ensure!(
        z.total_in() == (body.len() - start) as u64,
        "trailing or incomplete SHIP zlib stream"
    );
    ensure!(
        expected.is_none_or(|s| s == raw.len() as u64),
        "SHIP uncompressed size mismatch"
    );
    ensure!(
        marker <= 1 || u64::from(marker) == (body.len() - start) as u64,
        "legacy SHIP compressed size mismatch"
    );
    Ok((body[8..40].try_into()?, raw))
}

/// Stored-block size nodeos writes for SHIP payloads (zlib level 0, 32 KiB window). Measured
/// on Spring 1.2.2 and 1.2.3 logs; flate2's encoder splits at 31,745 bytes instead, so any
/// payload larger than that would not reproduce the on-disk bytes.
const SHIP_STORED_BLOCK: usize = 32 * 1024;

/// zlib stream of stored deflate blocks, split the way nodeos writes them.
fn stored_zlib(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + (raw.len() / SHIP_STORED_BLOCK + 1) * 5 + 6);
    out.extend_from_slice(&[0x78, 0x01]);
    let mut chunks = raw.chunks(SHIP_STORED_BLOCK).peekable();
    if chunks.peek().is_none() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    while let Some(chunk) = chunks.next() {
        out.push(u8::from(chunks.peek().is_none()));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
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

pub fn encode_ship(id: &[u8; 32], raw: &[u8], marker: u32) -> Result<Vec<u8>> {
    let compressed = stored_zlib(raw);
    let mut body = SHIP_MAGIC.to_le_bytes().to_vec();
    body.extend_from_slice(id);
    body.extend_from_slice(
        &((compressed.len() + if marker == 1 { 12 } else { 4 }) as u64).to_le_bytes(),
    );
    body.extend_from_slice(
        &(if marker <= 1 {
            marker
        } else {
            compressed.len() as u32
        })
        .to_le_bytes(),
    );
    if marker == 1 {
        body.extend_from_slice(&(raw.len() as u64).to_le_bytes());
    }
    body.extend_from_slice(&compressed);
    ship_payload(&body)?;
    Ok(body)
}

pub type Blocks = BTreeMap<u32, Vec<u8>>;
pub fn container(path: &Path) -> Result<Blocks> {
    let disk = Disk::open(path, false)?;
    let bytes = disk.read(0, disk.stamp()?.size)?;
    let mut c = Cursor::new(&bytes);
    ensure!(c.take(8)? == b"P2PBLK01", "not a P2PBLK01 container");
    let first = u32le(c.take(4)?);
    let last = u32le(c.take(4)?);
    ensure!(
        first > 0 && last >= first && last - first < MAX_BLOCKS,
        "invalid container range"
    );
    let mut out = BTreeMap::new();
    let mut previous = None;
    for expected in first..=last {
        let n = u32le(c.take(4)?);
        ensure!(
            n == expected,
            "missing, duplicate, or unordered container block {expected}"
        );
        let len = u32le(c.take(4)?);
        let body = c.take(len as usize)?;
        let h = block_header(body)?;
        ensure!(h.number == n, "container header number mismatch");
        if let Some(prev) = previous {
            ensure!(h.previous == prev, "container id chain broken at {n}");
        }
        previous = Some(h.id);
        out.insert(n, body.to_vec());
    }
    ensure!(c.pos == bytes.len(), "container has trailing data");
    Ok(out)
}

pub fn encode_container(blocks: &Blocks) -> Result<Vec<u8>> {
    let mut bytes = b"P2PBLK01".to_vec();
    bytes.extend_from_slice(
        &blocks
            .first_key_value()
            .context("no blocks")?
            .0
            .to_le_bytes(),
    );
    bytes.extend_from_slice(
        &blocks
            .last_key_value()
            .context("no blocks")?
            .0
            .to_le_bytes(),
    );
    for (n, body) in blocks {
        bytes.extend_from_slice(&n.to_le_bytes());
        bytes.extend_from_slice(&(body.len() as u32).to_le_bytes());
        bytes.extend_from_slice(body);
    }
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "container exceeds safety limit"
    );
    Ok(bytes)
}
