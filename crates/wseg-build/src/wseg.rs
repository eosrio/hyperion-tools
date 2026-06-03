//! Writer for the `.wseg` frozen-segment format. Mirrors the reader in WormDB
//! `src/storage/segment.zig` (all integers little-endian). See docs/WSEG_FORMAT.md.
//!
//! Layout: header (40 B) | table directory (48 B × N) | then, per table in
//! order, its index region (16 B × key_count, sorted by key asc) followed by
//! its blob arena.

use std::fs::File;
use std::io::{self, BufWriter, Write};

pub const MAGIC: &[u8; 8] = b"WSEG0001";
pub const VERSION: u32 = 1;
const HEADER_FIXED: u64 = 40;
const DIR_ENTRY: u64 = 48;
const INDEX_ENTRY: u64 = 20; // key u64 | off u64 | len u32

#[derive(Clone, Copy)]
pub struct IndexEntry {
    pub key: u64,
    pub off: u64,
    pub len: u32,
}

/// One table: a blob arena plus a (to-be-sorted) index over it.
pub struct Table {
    pub table_id: u32,
    pub index: Vec<IndexEntry>,
    pub arena: Vec<u8>,
}

struct Loc {
    key_count: u64,
    index_off: u64,
    index_len: u64,
    blob_off: u64,
    blob_len: u64,
}

/// Write a multi-table segment. Each table's index is sorted by key here.
pub fn write_segment(path: &str, mut tables: Vec<Table>) -> io::Result<()> {
    for t in &mut tables {
        t.index.sort_unstable_by_key(|e| e.key);
    }

    let n = tables.len() as u64;
    let mut running = HEADER_FIXED + DIR_ENTRY * n;
    let mut locs: Vec<Loc> = Vec::with_capacity(tables.len());
    for t in &tables {
        let key_count = t.index.len() as u64;
        let index_off = running;
        let index_len = key_count * INDEX_ENTRY;
        running += index_len;
        let blob_off = running;
        let blob_len = t.arena.len() as u64;
        running += blob_len;
        locs.push(Loc {
            key_count,
            index_off,
            index_len,
            blob_off,
            blob_len,
        });
    }

    let f = File::create(path)?;
    let mut w = BufWriter::with_capacity(1 << 20, f);

    // Header (40 B)
    w.write_all(MAGIC)?;
    w.write_all(&VERSION.to_le_bytes())?;
    w.write_all(&0u32.to_le_bytes())?; // flags
    w.write_all(&(tables.len() as u32).to_le_bytes())?; // table_count
    w.write_all(&0u32.to_le_bytes())?; // pad
    w.write_all(&0u64.to_le_bytes())?; // meta_off
    w.write_all(&0u64.to_le_bytes())?; // meta_len

    // Table directory (48 B each)
    for (t, loc) in tables.iter().zip(&locs) {
        w.write_all(&t.table_id.to_le_bytes())?;
        w.write_all(&(INDEX_ENTRY as u32).to_le_bytes())?; // key_stride
        w.write_all(&loc.key_count.to_le_bytes())?;
        w.write_all(&loc.index_off.to_le_bytes())?;
        w.write_all(&loc.index_len.to_le_bytes())?;
        w.write_all(&loc.blob_off.to_le_bytes())?;
        w.write_all(&loc.blob_len.to_le_bytes())?;
    }

    // Regions: index then blob, per table, in directory order.
    for t in &tables {
        for e in &t.index {
            w.write_all(&e.key.to_le_bytes())?; // 8
            w.write_all(&e.off.to_le_bytes())?; // 8 (u64)
            w.write_all(&e.len.to_le_bytes())?; // 4
        }
        w.write_all(&t.arena)?;
    }
    w.flush()?;
    Ok(())
}
