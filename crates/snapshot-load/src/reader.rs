//! Portable Antelope snapshot reader: file header, section framing, seek/skip.
//!
//! FILE    = u32 magic 0x30510550 | u32 file_format_version (=1) | SECTION* | u64 end-marker 0xFFFF..FF
//! SECTION = u64 size | u64 row_count | cstr name(NUL) | payload   (size excludes its own 8 bytes)
//! All integers little-endian; variable lengths are LEB128 varuint (fc::unsigned_int).

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};

use anyhow::{anyhow, bail, Context, Result};

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
    /// Advance the cursor by `n` bytes without materialising them. `n` is untrusted (derived from
    /// on-disk lengths), so guard the signed seek-offset cast and the position add against overflow.
    pub fn skip(&mut self, n: u64) -> Result<()> {
        if n == 0 {
            return Ok(());
        }
        if n > i64::MAX as u64 {
            bail!("skip {n} bytes overflows i64 seek offset at {}", self.pos);
        }
        let new_pos = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow!("skip {n} overflows position {}", self.pos))?;
        self.f.seek(SeekFrom::Current(n as i64))?;
        self.pos = new_pos;
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

    /// Read-and-discard the rest of the stream to EOF, advancing `pos`. Used after the row-section
    /// walk when `--tee` is active so the TeeReader mirrors EVERY byte (and gets its EOF flush),
    /// producing an on-disk `.bin` byte-identical to the source. Returns total bytes drained.
    pub fn drain_to_eof(&mut self) -> Result<u64> {
        let mut total = 0u64;
        loop {
            let n = self.r.read(&mut self.scratch)?;
            if n == 0 {
                break; // EOF
            }
            self.pos += n as u64;
            total += n as u64;
        }
        Ok(total)
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
        // EARLY guard: `size` must cover at least row_count(8). Check BEFORE reading `rows`/`name`,
        // because a `size < 8` would otherwise let us consume 8 bytes of `rows` from the NEXT frame
        // before the later `size >= header_bytes` check bails — desyncing the parse. (The full
        // `size >= 8 + name + NUL` check still runs after the name is read.)
        if size < 8 {
            bail!("section at offset {start}: size {size} < 8 (cannot hold row_count) — malformed framing");
        }
        let after_size = s.pos;
        let rows = s.u64()?;
        let name = s.cstr()?;
        let name_bytes = name.len() as u64 + 1;
        // `size` is untrusted: it must cover at least row_count(8) + name + NUL, and the section must
        // fit inside the file. Guard before the subtraction (else u64 underflow → bogus huge
        // payload_len) and before computing the next-section offset (else OOB / desync).
        let header_bytes = 8 + name_bytes; // row_count(8) + name + NUL
        if size < header_bytes {
            bail!(
                "section '{name}' at offset {start}: size {size} < header {header_bytes} (row_count+name+NUL) — malformed framing"
            );
        }
        let next_off = after_size.checked_add(size).ok_or_else(|| {
            anyhow!("section '{name}' at {start}: next-section offset overflow (size {size})")
        })?;
        if next_off > s.len {
            bail!(
                "section '{name}' at offset {start}: extends to {next_off} past file length {} — truncated/corrupt",
                s.len
            );
        }
        let payload_off = after_size + 8 + name_bytes;
        let payload_len = size - header_bytes; // size counts row_count(8) + name + NUL + payload
        out.push(Section {
            name,
            payload_off,
            rows,
            payload_len,
        });
        s.seek_to(next_off)?; // next section
    }
    Ok(out)
}

pub fn find<'a>(secs: &'a [Section], name: &str) -> Option<&'a Section> {
    secs.iter().find(|x| x.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Append one section frame to `buf`: `size(u64) | row_count(u64) | name(cstr) | payload`.
    /// `size` is supplied explicitly so a test can deliberately make it inconsistent.
    fn push_section(buf: &mut Vec<u8>, size: u64, rows: u64, name: &str, payload: &[u8]) {
        buf.extend_from_slice(&size.to_le_bytes());
        buf.extend_from_slice(&rows.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.push(0); // NUL
        buf.extend_from_slice(payload);
    }

    /// A well-formed section size = row_count(8) + name + NUL + payload.
    fn frame_size(name: &str, payload_len: usize) -> u64 {
        (8 + name.len() + 1 + payload_len) as u64
    }

    /// Write `bytes` to a unique temp file and open a `Snap` over it (mirrors the on-disk path).
    fn snap_over(bytes: &[u8], tag: &str) -> Snap {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let i = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "snapshot-load-reader-test-{}-{tag}-{i}.bin",
            std::process::id()
        ));
        let mut f = File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        drop(f);
        Snap::open(path.to_str().unwrap()).unwrap()
    }

    /// header = magic(u32 LE) | file_format_version(u32 LE)
    fn header() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&MAGIC.to_le_bytes());
        v.extend_from_slice(&FILE_FORMAT_VERSION.to_le_bytes());
        v
    }

    #[test]
    fn minimal_section_list_parses() {
        // header + two well-formed sections + end marker.
        let mut buf = header();
        push_section(
            &mut buf,
            frame_size("alpha", 3),
            1,
            "alpha",
            &[0xaa, 0xbb, 0xcc],
        );
        push_section(&mut buf, frame_size("beta", 0), 7, "beta", &[]);
        buf.extend_from_slice(&u64::MAX.to_le_bytes()); // end-of-file marker

        let mut s = snap_over(&buf, "ok");
        let secs = enumerate_sections(&mut s).unwrap();
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].name, "alpha");
        assert_eq!(secs[0].rows, 1);
        assert_eq!(secs[0].payload_len, 3);
        assert_eq!(secs[1].name, "beta");
        assert_eq!(secs[1].rows, 7);
        assert_eq!(secs[1].payload_len, 0);
        // alpha's payload starts right after its own header bytes.
        let mut s2 = snap_over(&buf, "ok2");
        s2.seek_to(secs[0].payload_off).unwrap();
        let mut p = vec![0u8; 3];
        s2.read_buf(&mut p).unwrap();
        assert_eq!(p, [0xaa, 0xbb, 0xcc]);
    }

    #[test]
    fn end_marker_stops_enumeration() {
        // One real section, then the end marker, then GARBAGE that must never be parsed.
        let mut buf = header();
        push_section(&mut buf, frame_size("only", 2), 1, "only", &[0x01, 0x02]);
        buf.extend_from_slice(&u64::MAX.to_le_bytes()); // end marker
        buf.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0x00, 0x11]); // trailing junk, never read

        let mut s = snap_over(&buf, "end");
        let secs = enumerate_sections(&mut s).unwrap();
        assert_eq!(secs.len(), 1, "enumeration stops at the end marker");
        assert_eq!(secs[0].name, "only");
    }

    #[test]
    fn malformed_size_underflow_bails_not_panics() {
        // size smaller than the mandatory header (row_count 8 + name + NUL) would underflow
        // `size - header` → a bogus huge payload_len. Must surface a clean Err, not panic.
        let mut buf = header();
        // name "x" → header_bytes = 8 + 1 + 1 = 10; declare size = 5 (< 10).
        push_section(&mut buf, 5, 0, "x", &[]);
        buf.extend_from_slice(&u64::MAX.to_le_bytes());

        let mut s = snap_over(&buf, "underflow");
        let err = enumerate_sections(&mut s).unwrap_err();
        assert!(
            err.to_string().contains("malformed framing"),
            "expected a framing error, got: {err}"
        );
    }

    #[test]
    fn section_past_eof_bails_not_panics() {
        // A size that points the next section past the file length must bail (OOB guard), not seek
        // wild / produce a bogus huge payload_len.
        let mut buf = header();
        // Declare a size far larger than the bytes actually present.
        push_section(&mut buf, 10_000, 1, "huge", &[0x01, 0x02]);
        // (no end marker / not enough bytes for the declared size)

        let mut s = snap_over(&buf, "pasteof");
        let err = enumerate_sections(&mut s).unwrap_err();
        assert!(
            err.to_string().contains("past file length"),
            "expected an EOF-overrun error, got: {err}"
        );
    }

    #[test]
    fn bad_magic_bails() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        buf.extend_from_slice(&FILE_FORMAT_VERSION.to_le_bytes());
        buf.extend_from_slice(&u64::MAX.to_le_bytes());
        let mut s = snap_over(&buf, "magic");
        let err = enumerate_sections(&mut s).unwrap_err();
        assert!(err.to_string().contains("bad magic"), "got: {err}");
    }
}
