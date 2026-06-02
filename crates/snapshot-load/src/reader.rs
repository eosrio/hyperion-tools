//! Portable Antelope snapshot reader: file header, section framing, seek/skip.
//!
//! FILE    = u32 magic 0x30510550 | u32 file_format_version (=1) | SECTION* | u64 end-marker 0xFFFF..FF
//! SECTION = u64 size | u64 row_count | cstr name(NUL) | payload   (size excludes its own 8 bytes)
//! All integers little-endian; variable lengths are LEB128 varuint (fc::unsigned_int).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use anyhow::{bail, Context, Result};

pub const MAGIC: u32 = 0x3051_0550;
pub const FILE_FORMAT_VERSION: u32 = 1;

/// Fixed serialized row sizes (bytes) for the 5 secondary index types, in
/// `contract_database_index_set` order *after* `key_value`:
/// index64, index128, index256, index_double, index_long_double. Each row is
/// `primary_key(u64) | payer(name u64) | secondary_key`.
pub const SECONDARY_ROW_SIZES: [u64; 5] = [
    8 + 8 + 8,  // index64:           u64
    8 + 8 + 16, // index128:          u128
    8 + 8 + 32, // index256:          array<u128,2>
    8 + 8 + 8,  // index_double:      f64
    8 + 8 + 16, // index_long_double: f128 stored as 16-byte u128 LE
];

/// Skips up to this many bytes are read through the BufReader (warm buffer); larger skips seek.
pub const READ_SKIP_MAX: u64 = 1 << 20;

/// Forward-only-capable read surface over a snapshot. The seekable file impl
/// (`Snap`) supports real seeks; the streaming impl (`StreamSnap`) supports
/// forward skips only. Walkers use *only* these methods, so the same code drives
/// both. All multi-byte reads are little-endian; varuint is LEB128.
///
/// The composite readers (`u8/u32/u64/varuint/cstr`) have default impls in terms
/// of `read_buf`, so the byte-level decode logic is defined in exactly one place —
/// guaranteeing the seek-from-file path stays byte-identical. (NOTE: `Snap` keeps
/// its inherent composite methods; inherent methods win over trait defaults, so
/// every concrete-`Snap` call site still resolves to the inherent, identical code.)
pub trait SnapRead {
    /// Current logical byte offset from the start of the snapshot.
    fn pos(&self) -> u64;

    /// Read exactly `buf.len()` bytes; advance pos.
    fn read_buf(&mut self, buf: &mut [u8]) -> Result<()>;

    /// Read exactly `n` bytes into `dst` (reusing its allocation); advance pos.
    fn read_into(&mut self, n: usize, dst: &mut Vec<u8>) -> Result<()>;

    /// Advance the cursor by `n` bytes without materialising them.
    fn skip(&mut self, n: u64) -> Result<()>;

    /// Position at absolute offset `p`. On a stream this is FORWARD-ONLY:
    /// it skips `p - pos` bytes and HARD-ERRORS if `p < pos`.
    fn seek_to(&mut self, p: u64) -> Result<()>;

    // ── composite readers: identical bytes for every impl ──
    fn u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_buf(&mut b)?;
        Ok(b[0])
    }
    fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_buf(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_buf(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    /// LEB128 varuint (fc::unsigned_int).
    fn varuint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                bail!("varuint too long at offset {}", self.pos());
            }
        }
    }
    /// NUL-terminated section name.
    fn cstr(&mut self) -> Result<String> {
        let mut v = Vec::new();
        loop {
            let b = self.u8()?;
            if b == 0 {
                break;
            }
            v.push(b);
        }
        Ok(String::from_utf8(v)?)
    }
}

/// Sequential+seekable reader over a snapshot file, tracking logical position.
pub struct Snap {
    f: BufReader<File>,
    pub pos: u64,
    pub len: u64,
}

impl Snap {
    pub fn open(path: &str) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("open {path}"))?;
        let len = file.metadata()?.len();
        Ok(Self {
            f: BufReader::with_capacity(1 << 20, file),
            pos: 0,
            len,
        })
    }
    pub fn seek_to(&mut self, p: u64) -> Result<()> {
        self.f.seek(SeekFrom::Start(p))?;
        self.pos = p;
        Ok(())
    }
    /// Advance the cursor by `n` bytes without materialising them.
    pub fn skip(&mut self, n: u64) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        self.f.seek(SeekFrom::Current(n as i64))?;
        self.pos += n;
        Ok(())
    }
    pub fn read_buf(&mut self, buf: &mut [u8]) -> Result<()> {
        self.f.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
    /// Read exactly `n` bytes into `dst` (reusing its allocation).
    pub fn read_into(&mut self, n: usize, dst: &mut Vec<u8>) -> Result<()> {
        dst.resize(n, 0);
        self.f.read_exact(dst)?;
        self.pos += n as u64;
        Ok(())
    }
    pub fn u8(&mut self) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read_buf(&mut b)?;
        Ok(b[0])
    }
    pub fn u32(&mut self) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read_buf(&mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    pub fn u64(&mut self) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read_buf(&mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    /// LEB128 varuint (fc::unsigned_int).
    #[allow(dead_code)] // wired up by the in-progress snapshot reader; kept to satisfy -D warnings
    pub fn varuint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        let mut shift = 0u32;
        loop {
            let byte = self.u8()?;
            value |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
            shift += 7;
            if shift > 63 {
                bail!("varuint too long at offset {}", self.pos);
            }
        }
    }
    /// NUL-terminated section name.
    pub fn cstr(&mut self) -> Result<String> {
        let mut v = Vec::new();
        loop {
            let b = self.u8()?;
            if b == 0 {
                break;
            }
            v.push(b);
        }
        Ok(String::from_utf8(v)?)
    }
}

/// Trait surface for `Snap`: delegate to the inherent primitives. The composite
/// readers use the trait defaults — byte-identical to the inherent ones (which
/// still exist and win at every concrete-`Snap` call site).
impl SnapRead for Snap {
    fn pos(&self) -> u64 {
        self.pos
    }
    fn read_buf(&mut self, buf: &mut [u8]) -> Result<()> {
        Snap::read_buf(self, buf)
    }
    fn read_into(&mut self, n: usize, dst: &mut Vec<u8>) -> Result<()> {
        Snap::read_into(self, n, dst)
    }
    fn skip(&mut self, n: u64) -> Result<()> {
        Snap::skip(self, n)
    }
    fn seek_to(&mut self, p: u64) -> Result<()> {
        Snap::seek_to(self, p)
    }
}

/// Forward-only reader over an arbitrary byte stream (e.g. an HTTP body or a
/// streaming zstd/gzip/tar decoder). `skip` reads-and-discards into a reusable
/// scratch buffer; `seek_to` is forward-only and hard-errors on any backward
/// target. This is the streaming-overlap path: download + decompress + decode
/// all run concurrently against one forward pass.
pub struct StreamSnap<R: Read> {
    r: R,
    pos: u64,
    scratch: Vec<u8>, // reused discard buffer
}

impl<R: Read> StreamSnap<R> {
    pub fn new(r: R) -> Self {
        Self {
            r,
            pos: 0,
            scratch: vec![0u8; READ_SKIP_MAX as usize],
        }
    }
}

impl<R: Read> SnapRead for StreamSnap<R> {
    fn pos(&self) -> u64 {
        self.pos
    }
    fn read_buf(&mut self, buf: &mut [u8]) -> Result<()> {
        self.r.read_exact(buf)?;
        self.pos += buf.len() as u64;
        Ok(())
    }
    fn read_into(&mut self, n: usize, dst: &mut Vec<u8>) -> Result<()> {
        dst.resize(n, 0);
        self.r.read_exact(dst)?;
        self.pos += n as u64;
        Ok(())
    }
    fn skip(&mut self, mut n: u64) -> Result<()> {
        while n > 0 {
            let chunk = n.min(self.scratch.len() as u64) as usize;
            self.r.read_exact(&mut self.scratch[..chunk])?;
            self.pos += chunk as u64;
            n -= chunk as u64;
        }
        Ok(())
    }
    fn seek_to(&mut self, p: u64) -> Result<()> {
        use std::cmp::Ordering::*;
        match p.cmp(&self.pos) {
            Equal => Ok(()),
            Greater => self.skip(p - self.pos), // forward-only skip
            Less => bail!(
                "StreamSnap: backward seek to {p} from {} is impossible on a forward-only stream",
                self.pos
            ),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Section {
    pub name: String,
    pub payload_off: u64,
    pub rows: u64,
    pub payload_len: u64,
}

/// Walk the top-level section headers from offset 8 to the end marker, skipping payloads by size.
pub fn enumerate_sections(s: &mut Snap) -> Result<Vec<Section>> {
    s.seek_to(0)?;
    let magic = s.u32()?;
    if magic != MAGIC {
        bail!("bad magic 0x{magic:08x} (expected 0x{MAGIC:08x}) — not a portable snapshot");
    }
    let fv = s.u32()?;
    if fv != FILE_FORMAT_VERSION {
        bail!("unsupported snapshot file-format version {fv} (expected {FILE_FORMAT_VERSION})");
    }
    let mut out = Vec::new();
    loop {
        let start = s.pos;
        if start + 8 > s.len {
            break;
        }
        let size = s.u64()?;
        if size == u64::MAX {
            break; // end-of-file marker
        }
        let after_size = s.pos;
        let rows = s.u64()?;
        let name = s.cstr()?;
        let name_bytes = name.len() as u64 + 1; // + NUL
        let payload_off = after_size + 8 + name_bytes;
        let payload_len = size - 8 - name_bytes; // size counts row_count(8) + name + NUL + payload
        out.push(Section {
            name,
            payload_off,
            rows,
            payload_len,
        });
        s.seek_to(after_size + size)?; // next section
    }
    Ok(out)
}

pub fn find<'a>(secs: &'a [Section], name: &str) -> Option<&'a Section> {
    secs.iter().find(|x| x.name == name)
}
