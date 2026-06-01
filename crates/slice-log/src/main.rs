//! slice-log — extract a small, self-contained block-range slice of a nodeos log (a state-history
//! ship log like trace_history/chain_state_history, OR the block log) with a REBASED index, so a
//! real slice can be copied off a production node for LOCAL, ground-truth testing of the
//! direct-from-disk tools. Read-only on the source; only ever reads committed (immutable) blocks
//! well below the head, so it cannot disturb a running nodeos.
//!
//! ship log:   <stem>.log starts directly with 48-byte-framed entries (first_block = first entry's
//!             block_num, BE at [8..12]); index = u64 LE offset per block. Slice has NO header.
//! block log:  blocks.log = [u32 version][u32 first_block][genesis...] then signed_block(+trailer)
//!             per block; index = u64 LE offset per block. The slice writes a fresh 8-byte header
//!             [version][slice_start] (BlockLog::open only reads those 8 bytes) and rebases offsets
//!             past it.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{bail, Context, Result};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Extract a rebased block-range slice of a nodeos ship/block log (read-only).")]
struct Args {
    /// directory containing <stem>.{log,index}
    #[arg(long)]
    dir: String,
    /// log stem: trace_history | chain_state_history | blocks
    #[arg(long)]
    stem: String,
    #[arg(long)]
    start: u32,
    #[arg(long)]
    count: u32,
    /// output directory for the slice
    #[arg(long)]
    out: String,
}

fn u64_at(f: &mut File, pos: u64) -> Result<u64> {
    f.seek(SeekFrom::Start(pos))?;
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_le_bytes(b))
}

fn main() -> Result<()> {
    let a = Args::parse();
    let log_path = format!("{}/{}.log", a.dir, a.stem);
    let idx_path = format!("{}/{}.index", a.dir, a.stem);
    let mut log = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
    let mut idx = File::open(&idx_path).with_context(|| format!("open {idx_path}"))?;
    let log_len = log.metadata()?.len();
    let is_blocklog = a.stem == "blocks";

    // first_block of the source + the header bytes the slice must carry.
    let (first_block, header): (u32, Vec<u8>) = if is_blocklog {
        let mut h = [0u8; 8];
        log.read_exact(&mut h)?;
        let version = u32::from_le_bytes(h[0..4].try_into().unwrap());
        let fb = u32::from_le_bytes(h[4..8].try_into().unwrap());
        let mut hd = version.to_le_bytes().to_vec();
        hd.extend_from_slice(&a.start.to_le_bytes()); // slice's first_block = start
        (fb, hd)
    } else {
        let mut h = [0u8; 48];
        log.read_exact(&mut h)?;
        let fb = u32::from_be_bytes(h[8..12].try_into().unwrap());
        (fb, Vec::new())
    };

    let n_idx = (idx.metadata()?.len() / 8) as u32;
    if n_idx == 0 {
        bail!(
            "{idx_path}: empty or truncated index ({} bytes) — no blocks to slice",
            idx.metadata()?.len()
        );
    }
    let last_block = first_block + n_idx - 1;
    if a.start < first_block || a.start > last_block {
        bail!(
            "start {} out of log range [{first_block}..{last_block}]",
            a.start
        );
    }
    let end = a.start.saturating_add(a.count).min(last_block + 1); // exclusive

    let start_off = u64_at(&mut idx, (a.start - first_block) as u64 * 8)?;
    let end_off = if end <= last_block {
        u64_at(&mut idx, (end - first_block) as u64 * 8)?
    } else {
        log_len
    };

    std::fs::create_dir_all(&a.out)?;
    let mut out_log = File::create(format!("{}/{}.log", a.out, a.stem))?;
    out_log.write_all(&header)?;
    log.seek(SeekFrom::Start(start_off))?;
    if end_off < start_off {
        bail!(
            "corrupt index: offset for block {end} ({end_off}) precedes start offset ({start_off})"
        );
    }
    let mut remaining = end_off - start_off;
    let mut buf = vec![0u8; 8 << 20];
    while remaining > 0 {
        let n = remaining.min(buf.len() as u64) as usize;
        log.read_exact(&mut buf[..n])?;
        out_log.write_all(&buf[..n])?;
        remaining -= n as u64;
    }

    // Rebased index: each block's offset becomes (orig - start_off + header_len).
    let mut out_idx = File::create(format!("{}/{}.index", a.out, a.stem))?;
    let hdr_len = header.len() as u64;
    let mut slice_offs = Vec::with_capacity((end - a.start) as usize);
    for b in a.start..end {
        let o = u64_at(&mut idx, (b - first_block) as u64 * 8)?;
        let so = o - start_off + hdr_len;
        slice_offs.push(so);
        out_idx.write_all(&so.to_le_bytes())?;
    }

    // Ship-log entries carry a trailing 8-byte position suffix == the entry's OWN absolute offset.
    // The raw copy preserved the original (full-log) offsets, which derails sequential readers
    // (action-proto/delta-proto use `suffix == pos` to stay aligned). Rewrite each suffix to its
    // slice-local offset. (The block log has no such per-entry suffix in BlockLog's read path.)
    if !is_blocklog {
        use std::io::Write as _;
        let mut rw = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("{}/{}.log", a.out, a.stem))?;
        for &so in &slice_offs {
            rw.seek(SeekFrom::Start(so + 40))?; // payload_size field in the 48-byte header
            let mut pb = [0u8; 8];
            rw.read_exact(&mut pb)?;
            let payload_size = u64::from_le_bytes(pb);
            rw.seek(SeekFrom::Start(so + 48 + payload_size))?;
            rw.write_all(&so.to_le_bytes())?;
        }
    }

    eprintln!(
        "[slice-log] {} blocks [{}..{}] of {} -> {} payload bytes (+{} header)",
        end - a.start,
        a.start,
        end - 1,
        a.stem,
        end_off - start_off,
        hdr_len
    );
    Ok(())
}
