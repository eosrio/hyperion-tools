//! The command line.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::ServeConfig;
use crate::check::{CheckConfig, IndexConfig, WARN_ENTRY_BYTES};
use crate::store::{BlocksMode, StoreConfig, VerifyIds};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Serve frozen Antelope state-history and block logs over the SHiP WebSocket protocol",
    long_about = "Serve frozen Antelope state-history logs (trace_history, chain_state_history, \
        finality_data_history) and block logs, flat or retained, over the SHiP WebSocket \
        protocol exactly as a Spring node does, without nodeos. Each server holds one \
        contiguous range of irreversible blocks and reports its last block as head and LIB; a \
        SHiP-aware router or proxy stitches such servers and live nodes into one history.\n\n\
        Every file is opened read-only and validated at startup: anything a strict reader \
        refuses, an overlap, a hole, or block ids that disagree at a seam stops the server. \
        There is no authentication: listen on a private network, behind a SHiP-aware router \
        or proxy."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Serve the logs of one contiguous range.
    Serve(ServeArgs),
    /// Validate the logs as `serve` would, without serving them, and report what is served.
    /// Exits 0 when clean, 1 on errors, and with --strict 2 on warnings.
    Check(CheckArgs),
    /// Rebuild missing or wrong state-history indexes (`<stem>.index`) into a sidecar
    /// directory, for `serve --index-dir`. Nothing is written beside the logs.
    Index(IndexArgs),
    /// Print the ABI text a SHiP node sends, to serve it with `--abi`.
    Abi(AbiArgs),
}

/// Where the logs are and what to serve of them.
#[derive(Args, Debug, Clone)]
pub struct StoreArgs {
    /// A directory of state-history and block logs, as nodeos writes them (`trace_history.log`)
    /// or retains them (`trace_history-<first>-<last>.log`); repeatable.
    #[arg(long = "dir", value_name = "DIR", required = true)]
    pub dirs: Vec<PathBuf>,
    /// Also scan the sub-directories of every --dir.
    #[arg(long)]
    pub recursive: bool,
    /// A directory of sidecar indexes (`<stem>.index`), used instead of the index beside a log;
    /// repeatable.
    #[arg(long = "index-dir", value_name = "DIR")]
    pub index_dirs: Vec<PathBuf>,
    /// Serve only blocks BEGIN to END - 1.
    #[arg(long, value_name = "BEGIN-END", value_parser = parse_range)]
    pub range: Option<(u32, u32)>,
    /// Where results get their `block` from.
    #[arg(long, value_enum, default_value_t = BlocksMode::Required)]
    pub blocks: BlocksMode,
    /// The chain id (64 hex digits): required with --blocks none, checked against the block
    /// logs otherwise.
    #[arg(long, value_name = "HEX64", value_parser = parse_chain_id)]
    pub chain_id: Option<[u8; 32]>,
    /// Which block ids are cross-checked before serving.
    #[arg(long, value_enum, default_value_t = VerifyIds::Seams)]
    pub verify_ids: VerifyIds,
    /// Log files kept open at once (least recently used closed first).
    #[arg(long, value_name = "N", default_value_t = 256)]
    pub max_open_files: usize,
}

impl StoreArgs {
    pub fn config(&self, abi: Option<PathBuf>) -> StoreConfig {
        StoreConfig {
            dirs: self.dirs.clone(),
            recursive: self.recursive,
            index_dirs: self.index_dirs.clone(),
            range: self.range,
            blocks: self.blocks,
            chain_id: self.chain_id,
            verify_ids: self.verify_ids,
            max_open_files: self.max_open_files,
            abi,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct ServeArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// The address to listen on.
    #[arg(long, value_name = "ADDR", default_value = "127.0.0.1:8080")]
    pub listen: SocketAddr,
    /// Send this ABI text as the first message instead of the vendored Spring SHiP ABI.
    #[arg(long, value_name = "FILE")]
    pub abi: Option<PathBuf>,
    /// Upgrades beyond this many open sessions get HTTP 503 with `Retry-After: 1`.
    #[arg(long, value_name = "N", default_value_t = 256)]
    pub max_sessions: usize,
    /// The largest client message accepted; a larger one drops the connection.
    #[arg(long, value_name = "BYTES", default_value_t = 1 << 20)]
    pub max_request_bytes: usize,
    /// Runtime worker threads (default: the available parallelism).
    #[arg(long, value_name = "N")]
    pub worker_threads: Option<usize>,
    /// Payloads inflating to more bytes are streamed in 1 MiB frames instead of being held in
    /// memory, after a full validation pass.
    #[arg(long, value_name = "BYTES", default_value_t = 4 << 20)]
    pub stream_threshold: u64,
    /// File reads and inflation running at once (default: twice the available parallelism, at
    /// most 16).
    #[arg(long, value_name = "N")]
    pub io_threads: Option<usize>,
    /// Results each session builds ahead of sending them, at most (0: none).
    #[arg(long, value_name = "N", default_value_t = 8)]
    pub prefetch: usize,
    /// Bytes all results built ahead may hold together; a result that does not fit is built
    /// when it is sent instead.
    #[arg(long, value_name = "BYTES", default_value_t = 1 << 30)]
    pub memory_budget: u64,
    /// Serve /health, /ready and /metrics over HTTP at this address.
    #[arg(long, value_name = "ADDR")]
    pub http: Option<SocketAddr>,
    /// Log line format; filter with RUST_LOG (default `info`).
    #[arg(long, value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,
}

impl ServeArgs {
    pub fn config(&self) -> ServeConfig {
        let defaults = ServeConfig::default();
        ServeConfig {
            store: self.store.config(self.abi.clone()),
            listen: self.listen,
            max_sessions: self.max_sessions,
            max_request_bytes: self.max_request_bytes,
            stream_threshold: self.stream_threshold,
            io_threads: self.io_threads.unwrap_or(defaults.io_threads),
            prefetch: self.prefetch,
            memory_budget: self.memory_budget,
            http: self.http,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct CheckArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    /// Read every served block: entry headers, trailers and whole zlib streams, block-log
    /// trailers, sizes and `previous` links, and the block ids every log gives.
    #[arg(long)]
    pub deep: bool,
    /// Also recompute every block id from its block header (implies --deep).
    #[arg(long)]
    pub hash: bool,
    /// Entries inflating to more bytes are warnings: a SHiP-aware router or proxy with a 256
    /// MiB message limit cannot relay them. From 4 GiB they are errors.
    #[arg(long, value_name = "N", default_value_t = WARN_ENTRY_BYTES)]
    pub warn_entry_bytes: u64,
    /// Exit 2 when there are warnings.
    #[arg(long)]
    pub strict: bool,
    /// Print the report as JSON.
    #[arg(long)]
    pub json: bool,
}

impl CheckArgs {
    pub fn config(&self) -> CheckConfig {
        CheckConfig {
            store: self.store.config(None),
            deep: self.deep || self.hash,
            hash: self.hash,
            warn_entry_bytes: self.warn_entry_bytes,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct IndexArgs {
    /// A directory of state-history logs; repeatable.
    #[arg(long = "dir", value_name = "DIR", required = true)]
    pub dirs: Vec<PathBuf>,
    /// Also scan the sub-directories of every --dir.
    #[arg(long)]
    pub recursive: bool,
    /// Where the indexes go (created if missing): never a directory holding the logs.
    #[arg(long, value_name = "DIR")]
    pub out: PathBuf,
}

impl IndexArgs {
    pub fn config(&self) -> IndexConfig {
        IndexConfig {
            dirs: self.dirs.clone(),
            recursive: self.recursive,
            out: self.out.clone(),
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct AbiArgs {
    /// The node's SHiP endpoint, `ws://host:port`.
    #[arg(long, value_name = "URL")]
    pub from: String,
    /// Write the ABI to this file instead of standard output.
    #[arg(long, value_name = "FILE")]
    pub out: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum LogFormat {
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

/// `BEGIN-END`, the blocks `[BEGIN, END)`.
fn parse_range(s: &str) -> Result<(u32, u32), String> {
    let (begin, end) = s.split_once('-').ok_or("expected BEGIN-END")?;
    let block = |v: &str| v.trim().parse::<u32>().map_err(|e| format!("{v:?}: {e}"));
    let (begin, end) = (block(begin)?, block(end)?);
    if begin >= end {
        return Err(format!(
            "{begin}-{end} holds no block (END is exclusive and must be above BEGIN)"
        ));
    }
    Ok((begin, end))
}

/// 64 hex digits.
fn parse_chain_id(s: &str) -> Result<[u8; 32], String> {
    let bytes = hex::decode(s.trim()).map_err(|e| format!("not hex: {e}"))?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| format!("{} bytes, a chain id has 32", b.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_half_open() {
        assert_eq!(parse_range("2-1001"), Ok((2, 1001)));
        assert_eq!(parse_range(" 5 - 6 "), Ok((5, 6)));
        assert!(parse_range("7-7").is_err());
        assert!(parse_range("9-3").is_err());
        assert!(parse_range("12").is_err());
        assert!(parse_range("1-x").is_err());
    }

    #[test]
    fn chain_ids_are_32_bytes_of_hex() {
        let id = "aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906";
        assert_eq!(parse_chain_id(id).map(hex::encode), Ok(id.to_owned()));
        assert!(parse_chain_id(&id[2..]).is_err());
        assert!(parse_chain_id("zz").is_err());
    }

    #[test]
    fn serve_defaults_match_the_documented_ones() {
        let cli = Cli::try_parse_from(["prog", "serve", "--dir", "logs"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("a serve command")
        };
        let cfg = args.config();
        let default = ServeConfig::default();
        assert_eq!(cfg.listen, default.listen);
        assert_eq!(cfg.max_sessions, default.max_sessions);
        assert_eq!(cfg.max_request_bytes, default.max_request_bytes);
        assert_eq!(cfg.stream_threshold, default.stream_threshold);
        let (store, want) = (cfg.store, StoreConfig::default());
        assert_eq!(store.dirs, [PathBuf::from("logs")]);
        assert_eq!(
            (store.blocks, store.verify_ids, store.max_open_files),
            (want.blocks, want.verify_ids, want.max_open_files)
        );
        assert_eq!((store.range, store.chain_id, store.abi), (None, None, None));
        assert_eq!(args.log_format, LogFormat::Text);

        let cli = Cli::try_parse_from([
            "prog",
            "serve",
            "--dir",
            "a",
            "--dir",
            "b",
            "--range",
            "10-20",
            "--blocks",
            "none",
            "--verify-ids",
            "off",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("a serve command")
        };
        let store = args.config().store;
        assert_eq!(store.dirs.len(), 2);
        assert_eq!(store.range, Some((10, 20)));
        assert_eq!(
            (store.blocks, store.verify_ids),
            (BlocksMode::None, VerifyIds::Off)
        );
        assert!(Cli::try_parse_from(["prog", "serve"]).is_err());
    }

    #[test]
    fn hardening_defaults_match_the_documented_ones() {
        let cli = Cli::try_parse_from(["prog", "serve", "--dir", "logs"]).unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("a serve command")
        };
        let cfg = args.config();
        let default = ServeConfig::default();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        assert_eq!(cfg.io_threads, (2 * cores).min(16));
        assert_eq!(cfg.io_threads, default.io_threads);
        assert_eq!((cfg.prefetch, cfg.memory_budget), (8, 1 << 30));
        assert_eq!(
            (cfg.prefetch, cfg.memory_budget, cfg.http),
            (default.prefetch, default.memory_budget, default.http)
        );

        let cli = Cli::try_parse_from([
            "prog",
            "serve",
            "--dir",
            "logs",
            "--io-threads",
            "3",
            "--prefetch",
            "0",
            "--memory-budget",
            "1000",
            "--http",
            "127.0.0.1:9180",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("a serve command")
        };
        let cfg = args.config();
        assert_eq!(
            (cfg.io_threads, cfg.prefetch, cfg.memory_budget),
            (3, 0, 1000)
        );
        assert_eq!(cfg.http, Some("127.0.0.1:9180".parse().unwrap()));
    }

    #[test]
    fn check_index_and_abi_take_their_options() {
        let cli =
            Cli::try_parse_from(["prog", "check", "--dir", "a", "--hash", "--strict"]).unwrap();
        let Command::Check(args) = cli.command else {
            panic!("a check command")
        };
        let cfg = args.config();
        assert!(cfg.deep && cfg.hash, "--hash implies --deep");
        assert_eq!(cfg.warn_entry_bytes, 268_435_456);
        assert!(args.strict && !args.json);
        assert_eq!(cfg.store.dirs, [PathBuf::from("a")]);

        let cli = Cli::try_parse_from([
            "prog",
            "check",
            "--dir",
            "a",
            "--warn-entry-bytes",
            "10",
            "--json",
            "--blocks",
            "none",
            "--chain-id",
            "aca376f206b8fc25a6ed44dbdc66547c36c6c33e3a119ffbeaef943642f0e906",
        ])
        .unwrap();
        let Command::Check(args) = cli.command else {
            panic!("a check command")
        };
        let cfg = args.config();
        assert!(!cfg.deep && !cfg.hash && args.json);
        assert_eq!(cfg.warn_entry_bytes, 10);
        assert_eq!(cfg.store.blocks, BlocksMode::None);

        let cli =
            Cli::try_parse_from(["prog", "index", "--dir", "a", "--dir", "b", "--out", "side"])
                .unwrap();
        let Command::Index(args) = cli.command else {
            panic!("an index command")
        };
        let cfg = args.config();
        assert_eq!((cfg.dirs.len(), cfg.out), (2, PathBuf::from("side")));
        assert!(Cli::try_parse_from(["prog", "index", "--dir", "a"]).is_err());

        let cli = Cli::try_parse_from(["prog", "abi", "--from", "ws://127.0.0.1:8080"]).unwrap();
        let Command::Abi(args) = cli.command else {
            panic!("an abi command")
        };
        assert_eq!(
            (args.from.as_str(), args.out),
            ("ws://127.0.0.1:8080", None)
        );
    }
}
