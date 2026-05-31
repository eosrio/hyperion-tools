//! abi-scanner CLI — see the crate docs (`lib.rs`) for the architecture.
//!
//! Dispatches to the direct-from-disk reader (`--from-disk`) or the SHiP
//! scanner (`--ship`), writing one abi-index NDJSON doc per setabi.

use std::fs::File;
use std::io::{BufWriter, Write};

use anyhow::{Context, Result};
use clap::Parser;

use abi_scanner::{disk, ship};

#[derive(Parser, Debug, Clone)]
#[command(about = "Scan a SHiP block range for contract ABI versions (setabi), deltas-only.")]
struct Args {
    /// SHiP websocket endpoint (a fleet-router or a nodeos SHiP node). Omit when using --from-disk.
    #[arg(long)]
    ship: Option<String>,
    /// Read directly from the nodeos state-history dir (contains chain_state_history.{log,index}).
    /// Bypasses nodeos/SHiP entirely — read-only, parallel, decodes from the append-only log.
    #[arg(long)]
    from_disk: Option<String>,
    /// First block to scan (inclusive)
    #[arg(long)]
    start: u32,
    /// Last block to scan (inclusive)
    #[arg(long)]
    end: u32,
    /// Output NDJSON file (one abi-index doc per line). Defaults to stdout.
    #[arg(long)]
    out: Option<String>,
    /// SHiP max_messages_in_flight (flow-control window) per connection
    #[arg(long, default_value_t = 50)]
    in_flight: u32,
    /// SHiP: parallel connections. Disk: parallel reader threads. (range split into N contiguous chunks)
    #[arg(long, default_value_t = 1)]
    connections: u32,
    /// Disk reader threads (defaults to --connections if unset)
    #[arg(long)]
    threads: Option<u32>,
    /// Request irreversible blocks only (safe for historical ranges)
    #[arg(long, default_value_t = true)]
    irreversible_only: bool,
    /// Checkpoint file for resumable --from-disk scans. Records how far the scan is
    /// contiguously done; re-run the same command to continue (the output is appended).
    #[arg(long)]
    checkpoint: Option<String>,
    /// Disk: blocks per work-stealing chunk. Smaller keeps the threads' read cursors clustered
    /// (better shared prefetch/cache locality on a cold, I/O-bound scan); larger scatters them.
    /// ~8 threads with small chunks was the measured sweet spot on a ZFS NVMe array.
    #[arg(long, default_value_t = 20_000)]
    chunk_size: u64,
    /// Disk: entries whose payload is at least this many bytes are stream-inflated only up to
    /// the account table (skipping the rest), instead of read + inflated whole. This avoids a
    /// multi-GB read/allocation on a snapshot init-delta. Default 16 MiB.
    #[arg(long, default_value_t = 16 << 20)]
    stream_threshold: u64,
}

/// Open `path` for resumable appending, first trimming any partial trailing line an
/// interrupted run may have left — so a resumed write never concatenates a new record onto an
/// incomplete one. Anything trimmed sits at/after the checkpoint watermark and is re-emitted on
/// resume, so it is never lost; the output stays clean NDJSON.
fn open_resumable_out(path: &str) -> Result<File> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false) // open the existing file in place; we trim only the partial last line
        .open(path)?;
    let len = f.seek(SeekFrom::End(0))?;
    if len == 0 {
        return Ok(f);
    }
    f.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    if last[0] != b'\n' {
        // unterminated trailing line — drop it back to the last complete record
        let window = len.min(1 << 20); // a record is far smaller than 1 MiB; scan that tail
        let start = len - window;
        f.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; window as usize];
        f.read_exact(&mut buf)?;
        match buf.iter().rposition(|&b| b == b'\n') {
            Some(pos) => f.set_len(start + pos as u64 + 1)?,
            // no newline in the scanned tail: isolate the stray line so the next record is clean
            None => {
                f.seek(SeekFrom::End(0))?;
                f.write_all(b"\n")?;
            }
        }
    }
    f.seek(SeekFrom::End(0))?;
    Ok(f)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    // Resume when the checkpoint already exists: append to the prior output, don't truncate it.
    let resuming = args
        .checkpoint
        .as_deref()
        .is_some_and(|c| std::path::Path::new(c).exists());
    let out: Box<dyn Write + Send> = match &args.out {
        Some(path) if resuming => Box::new(BufWriter::new(
            open_resumable_out(path).context("open out file (append/resume)")?,
        )),
        Some(path) => Box::new(BufWriter::new(
            File::create(path).context("create out file")?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    // Direct-from-disk mode: no nodeos, no SHiP — read the append-only log in parallel.
    if let Some(dir) = &args.from_disk {
        let threads = args.threads.unwrap_or(args.connections);
        let mut out = out;
        return disk::scan_disk(
            dir,
            args.start,
            args.end,
            threads,
            args.chunk_size,
            args.stream_threshold,
            args.checkpoint.as_deref(),
            &mut out,
        );
    }

    if args.checkpoint.is_some() {
        eprintln!("[abi-scanner] note: --checkpoint is only supported with --from-disk; ignoring");
    }
    let ship = args
        .ship
        .clone()
        .context("--ship is required (or use --from-disk)")?;
    ship::run_ship(
        ship,
        args.start,
        args.end,
        args.in_flight,
        args.connections,
        args.irreversible_only,
        out,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A resumed append must trim a partial trailing line (interrupted mid-write) so the next
    /// record is a clean line, never concatenated onto the incomplete one.
    #[test]
    fn resumable_out_trims_partial_trailing_line() {
        let dir = std::env::temp_dir().join(format!("abi-scanner-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.ndjson");
        let p = path.to_str().unwrap();
        // two complete records + an interrupted partial (no trailing newline)
        std::fs::write(p, b"{\"a\":1}\n{\"b\":2}\n{\"partial\":").unwrap();
        {
            let mut f = open_resumable_out(p).unwrap();
            f.write_all(b"{\"c\":3}\n").unwrap();
        }
        let mut s = String::new();
        std::fs::File::open(p)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            s, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n",
            "partial line trimmed and next record clean; got {s:?}"
        );
    }

    /// A clean file (ends with newline) is appended to as-is.
    #[test]
    fn resumable_out_keeps_clean_file() {
        let dir = std::env::temp_dir().join(format!("abi-scanner-resume2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("out.ndjson");
        let p = path.to_str().unwrap();
        std::fs::write(p, b"{\"a\":1}\n").unwrap();
        {
            let mut f = open_resumable_out(p).unwrap();
            f.write_all(b"{\"b\":2}\n").unwrap();
        }
        let mut s = String::new();
        std::fs::File::open(p)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            s, "{\"a\":1}\n{\"b\":2}\n",
            "clean file appended as-is; got {s:?}"
        );
    }
}
