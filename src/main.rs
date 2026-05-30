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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let out: Box<dyn Write + Send> = match &args.out {
        Some(path) => Box::new(BufWriter::new(
            File::create(path).context("create out file")?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    // Direct-from-disk mode: no nodeos, no SHiP — read the append-only log in parallel.
    if let Some(dir) = &args.from_disk {
        let threads = args.threads.unwrap_or(args.connections);
        let mut out = out;
        return disk::scan_disk(dir, args.start, args.end, threads, &mut out);
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
