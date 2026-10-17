mod codec;
mod format;
mod io;
mod repair;
mod replay;
mod report;
mod ship;
mod target;
#[cfg(test)]
mod tests;
mod verify;

// Offsets and lengths are u64 throughout and are sliced as usize after explicit bounds checks.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("antelope-log-repair supports 64-bit targets only");

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};

use report::Out;
use target::Selection;

#[derive(Parser)]
#[command(
    version,
    about = "Find and repair unreadable records in Antelope blocks.log and SHIP logs",
    long_about = "Find unreadable records in an Antelope blocks.log, trace_history.log, or \
        chain_state_history.log, rebuild them from independent state-history endpoints or an \
        isolated snapshot replay, and write them back offline after you approve a reviewed \
        plan. Scanning, fetching, replaying, and building never write the target. Apply \
        requires the approved plan hash and a stopped nodeos, refuses readable differing data, \
        and writes only the unreadable extents. No truncate, force, or index mutation.\n\n\
        Typical use:\n  \
        antelope-log-repair scan --log LOG\n  \
        antelope-log-repair prepare --log LOG --damaged-block N --ship ws://HOST:PORT --out DIR\n  \
        antelope-log-repair apply --bundle DIR/bundle --approve PLAN_SHA256 --node-stopped\n\n\
        Silent corruption (bytes that read back wrong, e.g. on ext4/xfs) is not repairable, \
        but `verify` finds it:\n  \
        antelope-log-repair verify --log LOG [--block-range A-B] [--against OTHER/blocks.log]"
)]
struct Cli {
    /// Print one JSON document on stdout instead of a human summary.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct TargetArgs {
    /// blocks.log, trace_history.log, chain_state_history.log, or a retained range log.
    #[arg(long)]
    log: PathBuf,
    /// Index file (default: LOG with the .index extension).
    #[arg(long)]
    index: Option<PathBuf>,
    /// Record size in bytes (default: the file's filesystem block size, which on ZFS is the
    /// file's record size). Decimal or 0x hex.
    #[arg(long, value_parser = parse_integer)]
    record_size: Option<u64>,
    /// Damaged record index or inclusive range A-B; repeatable. Nothing is scanned.
    /// A ZFS level-0 `zio_blkid` is a record index.
    #[arg(
        long = "record-index",
        visible_alias = "zfs-blkid",
        value_name = "N|A-B",
        value_parser = parse_records
    )]
    record_index: Vec<(u64, u64)>,
    /// Scan only the records holding this block's entry, following damage outward.
    #[arg(long, conflicts_with_all = ["record_index", "block_range"])]
    damaged_block: Option<u32>,
    /// Scan only the records holding blocks A through B, following damage outward.
    #[arg(long, value_name = "A-B", value_parser = parse_block_range, conflicts_with = "record_index")]
    block_range: Option<(u32, u32)>,
    /// Stop a scan after this many failed reads.
    #[arg(long, default_value_t = 1024)]
    max_bad_reads: u32,
}

impl TargetArgs {
    fn selection(&self) -> Selection {
        if !self.record_index.is_empty() {
            Selection::Records(self.record_index.clone())
        } else if let Some(n) = self.damaged_block {
            Selection::Block(n)
        } else if let Some((a, b)) = self.block_range {
            Selection::Blocks(a, b)
        } else {
            Selection::Whole
        }
    }

    fn locate(&self) -> Result<(format::Log, target::Located)> {
        let log = format::Log::open(&self.log, self.index.as_deref(), false)?;
        let selection = self.selection();
        if matches!(selection, Selection::Whole) {
            eprintln!(
                "Reading all {} of {} (no --damaged-block, --block-range, or --record-index \
                 given). On a live node this is a long sequential read competing with nodeos: \
                 prefer a narrow range, a quiet node or copy, or off-peak hours. Nothing is \
                 written; Ctrl-C stops it safely.",
                report::bytes(log.size),
                log.log.path.display()
            );
        }
        let located = target::locate(&log, self.record_size, &selection, self.max_bad_reads)?;
        Ok((log, located))
    }

    /// Damage that must exist and be repairable.
    fn target(&self) -> Result<(format::Log, target::Target)> {
        let (log, located) = self.locate()?;
        ensure!(
            !located.extents.is_empty(),
            "no unreadable data found in the selected range of {}; nothing to repair",
            log.log.path.display()
        );
        let t = target::Target::new(&log, located.record_size, &located.extents)?;
        Ok((log, t))
    }
}

#[derive(Args)]
struct SourceArgs {
    /// Directory written by fetch-ship or replay-ship (its source.json lists the files).
    /// Repeatable.
    #[arg(long)]
    sources: Vec<PathBuf>,
    /// P2PBLK01 block container. Repeatable.
    #[arg(long)]
    blocks: Vec<PathBuf>,
    /// Intact blocks.log from another node or a backup, with blocks.index alongside.
    /// Repeatable.
    #[arg(long)]
    blocks_log: Vec<PathBuf>,
    /// SHIP donor log of the target's kind, with its .index alongside. Repeatable.
    #[arg(long)]
    donor_log: Vec<PathBuf>,
}

impl SourceArgs {
    fn given(&self) -> bool {
        !(self.sources.is_empty()
            && self.blocks.is_empty()
            && self.blocks_log.is_empty()
            && self.donor_log.is_empty())
    }

    fn resolve(&self) -> Result<repair::Sources> {
        let files = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|log| repair::LogFile {
                    log: log.clone(),
                    index: None,
                })
                .collect()
        };
        let mut sources = repair::Sources {
            blocks: self.blocks.clone(),
            block_logs: files(&self.blocks_log),
            donors: files(&self.donor_log),
        };
        for dir in &self.sources {
            sources.extend(repair::Sources::from_dir(dir)?);
        }
        Ok(sources)
    }
}

#[derive(Args)]
struct ReplayArgs {
    /// Matching nodeos/Spring executable; runs only inside the new sandbox.
    #[arg(long, requires_all = ["snapshot", "snapshot_block", "blocks_dir"])]
    nodeos: Option<PathBuf>,
    /// Uncompressed portable snapshot taken before the damage; never modified.
    #[arg(long, requires = "nodeos")]
    snapshot: Option<PathBuf>,
    /// Operator-verified snapshot head; at most (first damaged block - 2).
    #[arg(long, requires = "nodeos")]
    snapshot_block: Option<u32>,
    /// Directory with intact block history: a blocks.log and/or retained
    /// blocks-FIRST-LAST.log files, each with its index. Repeatable, e.g. the node's
    /// blocks-dir and blocks-retained-dir. Only the replay range is copied.
    #[arg(long, requires = "nodeos")]
    blocks_dir: Vec<PathBuf>,
    /// Match the original node's trace-history-debug-mode setting.
    #[arg(long)]
    trace_history_debug_mode: bool,
    /// Sandbox chain-state database size; provision enough for the snapshot.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(256..))]
    chain_state_db_size_mb: u32,
}

impl ReplayArgs {
    fn given(&self) -> bool {
        self.nodeos.is_some()
    }

    fn run(&self, log: &format::Log, t: &target::Target, out: &Path) -> Result<Out> {
        let (Some(nodeos), Some(snapshot), Some(snapshot_block), false) = (
            &self.nodeos,
            &self.snapshot,
            self.snapshot_block,
            self.blocks_dir.is_empty(),
        ) else {
            bail!("replay needs --nodeos, --snapshot, --snapshot-block, and --blocks-dir");
        };
        replay::run(
            log,
            t,
            replay::ReplayOptions {
                nodeos,
                snapshot,
                snapshot_block,
                blocks_dirs: &self.blocks_dir,
                out,
                debug_traces: self.trace_history_debug_mode,
                state_db_size_mb: self.chain_state_db_size_mb,
            },
        )
    }
}

#[derive(Subcommand)]
enum Command {
    /// Find unreadable records and report whether and how they can be repaired. Read-only.
    #[command(visible_alias = "inspect")]
    Scan {
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Find entries that read back but are wrong (silent corruption, e.g. on ext4/xfs):
    /// index trailers, the blocks.log ID chain, SHIP zlib checksums, and optionally a
    /// byte-for-byte comparison with another node's blocks.log. Read-only.
    Verify {
        /// blocks.log, trace_history.log, chain_state_history.log, or a retained range log.
        #[arg(long)]
        log: PathBuf,
        /// Index file (default: LOG with the .index extension).
        #[arg(long)]
        index: Option<PathBuf>,
        /// Verify only blocks A through B (default: the whole log).
        #[arg(long, value_name = "A-B", value_parser = parse_block_range)]
        block_range: Option<(u32, u32)>,
        /// Intact blocks.log from another node, with blocks.index alongside: compares packed
        /// blocks byte for byte (blocks.log) or entry block IDs (SHIP logs). Repeatable.
        #[arg(long)]
        against: Vec<PathBuf>,
        /// Stop after this many problems.
        #[arg(long, default_value_t = 1000)]
        max_findings: usize,
    },
    /// Locate damage, acquire sources, and build a review bundle in one step. Choose one
    /// source: --ship endpoints, replay options, or local files. Never writes the target.
    Prepare {
        #[command(flatten)]
        target: TargetArgs,
        /// ws:// or wss:// state-history endpoint; repeat for independent operators.
        #[arg(long, conflicts_with = "nodeos")]
        ship: Vec<String>,
        #[command(flatten)]
        replay: ReplayArgs,
        #[command(flatten)]
        local: SourceArgs,
        /// Per-endpoint deadline for connect, ABI, and the complete transfer.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
        /// NEW directory: sources/ (or replay/) and bundle/.
        #[arg(long)]
        out: PathBuf,
    },
    /// Fetch irreversible packed blocks (+ traces/deltas for SHIP targets) from SHIP endpoints.
    FetchShip {
        #[command(flatten)]
        target: TargetArgs,
        /// ws:// or wss:// state-history endpoint. Repeat for independent operators; every
        /// endpoint must return byte-identical data.
        #[arg(long, required = true)]
        ship: Vec<String>,
        /// Per-endpoint deadline for connect, ABI, and the complete transfer.
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
        /// NEW source directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Regenerate SHIP history: slice an intact blocklog and replay it from a snapshot.
    ReplayShip {
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        replay: ReplayArgs,
        /// NEW sandbox directory; holds the replay, its logs, and the source files.
        #[arg(long)]
        out: PathBuf,
    },
    /// Verify sources and create a NEW review bundle. Never writes the target.
    Build {
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        sources: SourceArgs,
        /// NEW bundle directory.
        #[arg(long)]
        out: PathBuf,
    },
    /// Revalidate an approved bundle, journal, then write the unreadable extents.
    Apply {
        #[arg(long)]
        bundle: PathBuf,
        /// plan_sha256 printed by build/prepare, after reviewing plan.json and sources.
        #[arg(long)]
        approve: String,
        /// Acknowledge that nodeos and every other reader/writer of this log is stopped.
        #[arg(long)]
        node_stopped: bool,
    },
}

fn parse_integer(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16)
    } else {
        s.parse()
    }
    .map_err(|e| e.to_string())
}

fn parse_records(s: &str) -> std::result::Result<(u64, u64), String> {
    let (a, b) = match s.split_once('-') {
        Some((a, b)) => (parse_integer(a)?, parse_integer(b)?),
        None => {
            let n = parse_integer(s)?;
            (n, n)
        }
    };
    if a > b {
        return Err(format!("range {a}-{b} is reversed"));
    }
    Ok((a, b))
}

fn parse_block_range(s: &str) -> std::result::Result<(u32, u32), String> {
    let (a, b) = s
        .split_once('-')
        .or_else(|| s.split_once(".."))
        .ok_or("expected FIRST-LAST")?;
    let block = |v: &str| v.trim().parse::<u32>().map_err(|e| e.to_string());
    let (a, b) = (block(a)?, block(b)?);
    if a > b {
        return Err(format!("range {a}-{b} is reversed"));
    }
    Ok((a, b))
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli).await {
        Ok(out) if json => println!("{}", serde_json::to_string_pretty(&out.json).unwrap()),
        Ok(out) => print!("{}", out.text),
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "error": format!("{e:#}")})
                );
            } else {
                eprintln!("error: {e:#}");
            }
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<Out> {
    match cli.command {
        Command::Scan { target } => {
            let (log, located) = target.locate()?;
            repair::scan_report(&log, &located)
        }
        Command::Verify {
            log,
            index,
            block_range,
            against,
            max_findings,
        } => {
            ensure!(max_findings > 0, "--max-findings must be at least 1");
            let log = format::Log::open(&log, index.as_deref(), false)?;
            if block_range.is_none() {
                eprintln!(
                    "Verifying all {} of {} (no --block-range given). On a live node this is a \
                     long sequential read competing with nodeos: prefer a range, a quiet node or \
                     copy, or off-peak hours. Nothing is written; Ctrl-C stops it safely.",
                    report::bytes(log.size),
                    log.log.path.display()
                );
            }
            verify::verify(&log, block_range, &against, max_findings)
        }
        Command::Prepare {
            target,
            ship: urls,
            replay,
            local,
            timeout_secs,
            out,
        } => {
            let modes = [!urls.is_empty(), replay.given(), local.given()];
            ensure!(
                modes.iter().filter(|m| **m).count() == 1,
                "choose one source: --ship URL (repeatable); the replay options (--nodeos, \
                 --snapshot, --snapshot-block, --blocks-dir); or local files (--blocks-log, \
                 --blocks, --donor-log, --sources)"
            );
            let (log, t) = target.target()?;
            io::create_dir(&out)?;
            let (sources, acquired) = if local.given() {
                (local.resolve()?, None)
            } else {
                let (dir, acquired) = if replay.given() {
                    let dir = out.join("replay");
                    let acquired = replay.run(&log, &t, &dir)?;
                    (dir, acquired)
                } else {
                    let dir = out.join("sources");
                    let acquired = ship::fetch(&log, &t, &urls, &dir, timeout_secs)
                        .await
                        .with_context(|| {
                            format!("acquisition failed; output is kept in {}", dir.display())
                        })?;
                    (dir, acquired)
                };
                (repair::Sources::from_dir(&dir)?, Some(acquired))
            };
            let built = repair::build(&log, &t, &sources, &out.join("bundle"))?;
            let first_line = acquired
                .as_ref()
                .and_then(|a| a.text.lines().next())
                .map(|l| format!("{l}\n\n"))
                .unwrap_or_default();
            Ok(Out {
                text: format!("{first_line}{}", built.text),
                json: serde_json::json!({
                    "ok": true,
                    "acquisition": acquired.map(|a| a.json),
                    "build": built.json,
                }),
            })
        }
        Command::FetchShip {
            target,
            ship: urls,
            timeout_secs,
            out,
        } => {
            let (log, t) = target.target()?;
            ship::fetch(&log, &t, &urls, &out, timeout_secs).await
        }
        Command::ReplayShip {
            target,
            replay,
            out,
        } => {
            let (log, t) = target.target()?;
            replay.run(&log, &t, &out)
        }
        Command::Build {
            target,
            sources,
            out,
        } => {
            let (log, t) = target.target()?;
            repair::build(&log, &t, &sources.resolve()?, &out)
        }
        Command::Apply {
            bundle,
            approve,
            node_stopped,
        } => repair::apply(&bundle, &approve, node_stopped),
    }
}
