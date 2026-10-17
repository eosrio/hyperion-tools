//! zlib encodings nodeos has written SHIP payloads with, to rebuild entries byte for byte.
//!
//! Every release from EOSIO 1.8 to Leap 5 compresses each payload with the host's zlib at the
//! default level 6 (window 15, memLevel 8, default strategy) through boost::iostreams, which
//! only ever calls `deflate` with `Z_NO_FLUSH` and then `Z_FINISH`. Stock zlib 1.2.8 to 1.3.x
//! produces identical bytes for that, however the input is fed. Spring writes level 0: stored
//! blocks of 32 KiB. Older zlib split stored data at 32,506 bytes, and a payload that is an
//! exact multiple of the block size may end with an extra empty final block.
//!
//! Which encoding a log uses is never assumed: it is calibrated per region on the readable
//! anchor entries, which must be reproduced byte for byte (see `repair::rebuild`).

use std::os::raw::{c_int, c_uint, c_void};

use anyhow::{Result, bail, ensure};
use libz_sys as z;

/// One way of turning an uncompressed SHIP payload into the zlib stream stored on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// zlib level 0 as stored deflate blocks of `block` bytes. With `empty_final`, a payload
    /// that fills its last block exactly is followed by an empty final block.
    Stored { block: usize, empty_final: bool },
    /// Stock zlib `deflate` at this level, window 15, memLevel 8, default strategy.
    Deflate(i32),
}

/// What Spring 1.x writes; also the default for files this tool creates.
pub const SPRING: Codec = Codec::Stored {
    block: 32 * 1024,
    empty_final: false,
};

impl Codec {
    /// Every encoding to calibrate against, most common first.
    pub fn candidates() -> Vec<Codec> {
        let mut out = Vec::new();
        for block in [32 * 1024, 32_506] {
            for empty_final in [false, true] {
                out.push(Codec::Stored { block, empty_final });
            }
        }
        out.push(Codec::Deflate(6));
        out.extend((1..=9).filter(|l| *l != 6).map(Codec::Deflate));
        out
    }

    pub fn name(&self) -> String {
        match self {
            Codec::Stored {
                block,
                empty_final: false,
            } => format!("stored zlib, {block}-byte blocks"),
            Codec::Stored {
                block,
                empty_final: true,
            } => format!("stored zlib, {block}-byte blocks, empty final block on exact fill"),
            Codec::Deflate(level) => format!("zlib level {level}"),
        }
    }

    pub fn encode(&self, raw: &[u8]) -> Result<Vec<u8>> {
        match *self {
            Codec::Stored { block, empty_final } => Ok(stored(raw, block, empty_final)),
            Codec::Deflate(level) => deflate(raw, level),
        }
    }
}

fn stored(raw: &[u8], block: usize, empty_final: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + (raw.len() / block + 2) * 5 + 6);
    out.extend_from_slice(&[0x78, 0x01]);
    let exact = !raw.is_empty() && raw.len().is_multiple_of(block);
    let mut chunks = raw.chunks(block).peekable();
    while let Some(chunk) = chunks.next() {
        let last = chunks.peek().is_none() && !(exact && empty_final);
        out.push(u8::from(last));
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    if raw.is_empty() || (exact && empty_final) {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

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

/// The linked zlib's version string. This crate links stock zlib built from source, never the
/// host's libz, so the output does not depend on the machine running the tool.
pub fn zlib_version() -> String {
    // SAFETY: zlibVersion returns a static NUL-terminated string.
    unsafe { std::ffi::CStr::from_ptr(z::zlibVersion()) }
        .to_string_lossy()
        .into_owned()
}

extern "C" fn zalloc(_opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
    let Some(bytes) = (items as usize).checked_mul(size as usize) else {
        return std::ptr::null_mut();
    };
    // A u64 length header, so zfree knows the layout.
    let Ok(layout) = std::alloc::Layout::from_size_align(bytes + 8, 8) else {
        return std::ptr::null_mut();
    };
    // SAFETY: layout has a non-zero size.
    unsafe {
        let p = std::alloc::alloc(layout);
        if p.is_null() {
            return p.cast();
        }
        p.cast::<u64>().write(bytes as u64);
        p.add(8).cast()
    }
}

extern "C" fn zfree(_opaque: *mut c_void, ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: ptr came from zalloc, which stored the size 8 bytes before it.
    unsafe {
        let base = ptr.cast::<u8>().sub(8);
        let bytes = base.cast::<u64>().read() as usize;
        std::alloc::dealloc(
            base,
            std::alloc::Layout::from_size_align_unchecked(bytes + 8, 8),
        );
    }
}

/// One-shot stock zlib compression with the parameters boost::iostreams passes.
fn deflate(raw: &[u8], level: i32) -> Result<Vec<u8>> {
    let version = zlib_version();
    ensure!(
        !version.contains("ng"),
        "this build links {version}, which does not reproduce stock zlib output"
    );
    ensure!(
        raw.len() <= c_uint::MAX as usize,
        "payload too large to re-encode"
    );
    // SAFETY: the stream is initialized by deflateInit2_ before use, its buffers outlive every
    // call, and deflateEnd releases it on every path.
    unsafe {
        let mut s = z::z_stream {
            next_in: raw.as_ptr().cast_mut(),
            avail_in: raw.len() as c_uint,
            total_in: 0,
            next_out: std::ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: std::ptr::null_mut(),
            state: std::ptr::null_mut(),
            zalloc,
            zfree,
            opaque: std::ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        };
        let rc = z::deflateInit2_(
            &mut s,
            level as c_int,
            z::Z_DEFLATED,
            15,
            8,
            z::Z_DEFAULT_STRATEGY,
            z::zlibVersion(),
            std::mem::size_of::<z::z_stream>() as c_int,
        );
        ensure!(rc == z::Z_OK, "deflateInit2 failed: {rc}");
        let mut out = vec![0u8; z::deflateBound(&mut s, raw.len() as z::uLong) as usize + 16];
        s.next_out = out.as_mut_ptr();
        s.avail_out = out.len() as c_uint;
        let rc = z::deflate(&mut s, z::Z_FINISH);
        let written = s.total_out as usize;
        z::deflateEnd(&mut s);
        if rc != z::Z_STREAM_END {
            bail!("deflate did not finish: {rc}");
        }
        out.truncate(written);
        Ok(out)
    }
}
