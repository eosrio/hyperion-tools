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
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .context("open out file (append/resume)")?,
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
