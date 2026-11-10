//! range-server: a read-only disk range service next to a Wire node's logs.
//!
//! Hyperion's master pulls block ranges from it; it never pushes and never writes to
//! Elasticsearch. It reads the node's `state-history` (trace and chain-state logs) and `blocks`
//! directories, and a directory of sealed ABI history catalogs.
//!
//! * The genesis catalog is pinned by its manifest's SHA-256. Every extension found under the
//!   catalog directory is verified against its base ([`hyperion_ship::wire_catalog`]) and the
//!   longest verified chain is served. With `--follow`, new extensions are adopted while running.
//! * The **sealed LIB** is the newest catalog's end, bounded by what the logs hold. Every block a
//!   range serves is a catalog member: its trace-log, chain-state and block-log ids must equal the
//!   catalog's id for it, and its header must chain to the block before.
//! * With `--seal`, the service extends the chain itself: every `--seal-blocks` irreversible
//!   blocks (or after `--seal-max-wait-ms`) it seals them as one incremental segment, verifies it
//!   like any consumer and serves it ([`seal`]).
//! * The logs' indexes are re-measured on every request, so a node's growing logs are followed;
//!   only blocks up to the sealed LIB are served, and `blocks.log` holds irreversible blocks only.
//! * Documents are exactly what `action-proto` and `delta-proto` write (the same producers), as
//!   `_bulk` lines grouped per block ([`range`]).
//!
//! API (JSON unless noted):
//! * `GET /v1/info`: chain, profile, catalog chain, sealed LIB, log ranges, limits.
//! * `GET /v1/catalog?after=<manifest sha>`: the verified segments after that one.
//! * `GET /v1/catalog/<manifest sha>/<member>`: a segment's manifest or member file, verbatim.
//! * `POST /v1/ranges`: `{from, to, kinds, profile, docProfile, index, credits, threads}` streams
//!   framed NDJSON (chunked). `X-Range-Stream` names the stream for credits.
//! * `POST /v1/ranges/<stream>/credits`: `{frames}` grants block frames to a credited stream.
//! * `DELETE /v1/ranges/<stream>`: cancel a stream.

mod range;
mod seal;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use hyperion_ship::provider::{ProtocolProvider, TimestampProfile};
use hyperion_ship::slab::{BlockFile, ShipFile};
use hyperion_ship::wire_catalog::{Chain, Loaded, Pin, Segment};
use hyperion_wire_protocol::codec::CodecProfile;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Response, Server};

#[derive(Parser, Debug)]
#[command(about = "Serve sealed block ranges of Hyperion 4.5 Wire documents from a node's logs.")]
struct Args {
    /// The node's protocol profile (a Wire profile).
    #[arg(long)]
    protocol: ProtocolProvider,
    /// The node's state-history directory (trace_history and chain_state_history logs).
    #[arg(long)]
    state_history: PathBuf,
    /// The node's blocks directory (blocks.log and blocks.index).
    #[arg(long)]
    blocks_dir: PathBuf,
    /// Directory holding sealed ABI history catalogs (searched two levels deep).
    #[arg(long)]
    catalog_dir: PathBuf,
    /// SHA-256 of the genesis catalog's manifest: the one catalog this service trusts by pin.
    #[arg(long)]
    genesis_catalog_sha256: String,
    /// Chain id (64 lowercase hex).
    #[arg(long)]
    chain_id: String,
    /// Address to listen on (port 0 picks one; the address is printed on stdout).
    #[arg(long, default_value = "127.0.0.1:7180")]
    listen: String,
    /// Chunks produced at once over all ranges (0: the available parallelism).
    #[arg(long, default_value_t = 0)]
    threads: usize,
    /// Worker threads one range uses unless it asks for fewer (0: all of --threads).
    #[arg(long, default_value_t = 0)]
    range_threads: usize,
    /// Largest range, in blocks.
    #[arg(long, default_value_t = 1_000_000)]
    max_range: u32,
    /// Adopt catalog extensions added to --catalog-dir while running.
    #[arg(long, default_value_t = false)]
    follow: bool,
    /// How often --follow looks for new catalogs, in milliseconds.
    #[arg(long, default_value_t = 1000)]
    poll_ms: u64,
    /// Seal the node's new irreversible blocks as incremental catalog segments while running.
    #[arg(long, default_value_t = false, requires = "ship_abi")]
    seal: bool,
    /// The node's SHiP ABI (handshake JSON), as the catalog chain was sealed with it.
    #[arg(long)]
    ship_abi: Option<PathBuf>,
    /// Where segments are written (default: `segments` under --catalog-dir).
    #[arg(long)]
    seal_dir: Option<PathBuf>,
    /// Seal once this many irreversible blocks follow the chain.
    #[arg(long, default_value_t = 120)]
    seal_blocks: u32,
    /// Seal fewer blocks once the oldest unsealed one has waited this long, in milliseconds.
    #[arg(long, default_value_t = 60_000)]
    seal_max_wait_ms: u64,
    /// How often the sealer looks at the block log, in milliseconds.
    #[arg(long, default_value_t = 500)]
    seal_poll_ms: u64,
    /// Threads one seal reads with.
    #[arg(long, default_value_t = 4)]
    seal_threads: usize,
    /// How long a credited stream waits for a grant before it fails, in seconds.
    #[arg(long, default_value_t = 60)]
    credit_timeout: u64,
    /// Default `protocol.profileId` of documents (default: the --protocol profile id).
    #[arg(long)]
    doc_profile_id: Option<String>,
    /// Default index prefix of `_bulk` lines (`<prefix>-action-<version>-<partition>`).
    #[arg(long, default_value = "wire")]
    index_prefix: String,
    /// Default index version.
    #[arg(long, default_value = "v1")]
    index_version: String,
    /// Default `INDEX_PARTITION_SIZE`.
    #[arg(long, default_value_t = 10_000_000)]
    partition_size: u64,
    /// Default for `features.index_transfer_memo`.
    #[arg(long, default_value_t = false)]
    index_transfer_memo: bool,
    /// System contract account whose tables Hyperion's table handlers reshape.
    #[arg(long, default_value = "sysio")]
    system_contract: String,
}

pub struct Defaults {
    pub doc_profile: String,
    pub index_prefix: String,
    pub index_version: String,
    pub partition_size: u64,
    pub index_transfer_memo: bool,
    pub system_contract: String,
}

/// The catalog chain a request is served from, with the producers' views of its records.
pub struct Sealed {
    pub chain_segments: Vec<Segment>,
    pub newest: Arc<Loaded>,
    pub actions: action_proto::AbiIndex,
    pub deltas: delta_proto::wire::Catalog,
}

pub struct Service {
    pub protocol: ProtocolProvider,
    pub profile: CodecProfile,
    pub timestamps: TimestampProfile,
    pub chain_id: String,
    pub trace: ShipFile,
    pub state: ShipFile,
    pub blocks: BlockFile,
    pub defaults: Defaults,
    pub threads: usize,
    pub range_threads: usize,
    pub max_range: u32,
    pub credit_timeout: Duration,
    pub state_history: PathBuf,
    catalog_dir: PathBuf,
    follow: bool,
    pub chain: Mutex<Chain>,
    pub current: RwLock<Arc<Sealed>>,
    /// `(every blocks, max wait ms)` when the service seals its own chain.
    sealing: Option<(u32, u64)>,
    pub seal_status: Mutex<seal::Status>,
    /// One permit per worker thread that may produce a chunk at a time, over all ranges.
    pub permits: range::Permits,
    /// Ranges streaming now, and the window each may run ahead of its fold: the budget's share,
    /// so the documents all ranges hold stay near `threads` chunks.
    active: AtomicUsize,
    pub window_limit: AtomicUsize,
    next_stream: AtomicU64,
    streams: Mutex<HashMap<u64, Arc<range::Credits>>>,
}

/// What the logs and the catalog allow right now.
struct Bounds {
    trace: (u32, u32),
    state: (u32, u32),
    blocks: (u32, u32),
    sealed_lib: u32,
}

fn log_range(first: u32, last: Option<u32>) -> (u32, u32) {
    (first, last.unwrap_or(first.saturating_sub(1)))
}

impl Service {
    /// Ranges streaming now.
    pub fn active_ranges(&self) -> usize {
        self.active.load(Relaxed)
    }

    fn sealed(&self) -> Arc<Sealed> {
        self.current.read().unwrap().clone()
    }

    fn bounds(&self, sealed: &Sealed) -> Result<Bounds> {
        let trace = log_range(self.trace.first_block(), self.trace.last_block()?);
        let state = log_range(self.state.first_block(), self.state.last_block()?);
        let blocks = log_range(self.blocks.first_block(), self.blocks.last_block()?);
        let sealed_lib = sealed
            .newest
            .segment
            .end_block
            .min(trace.1)
            .min(state.1)
            .min(blocks.1);
        Ok(Bounds {
            trace,
            state,
            blocks,
            sealed_lib,
        })
    }

    /// Build what requests are served from, after checking that the logs still hold the newest
    /// catalog's last block (when they reach it).
    pub fn view(&self, chain: &Chain) -> Result<Sealed> {
        check_logs(&self.trace, &self.state, &chain.newest)?;
        build(chain)
    }

    fn info(&self) -> Result<Value> {
        let sealed = self.sealed();
        let b = self.bounds(&sealed)?;
        let newest = &sealed.newest.segment;
        let genesis = &sealed.chain_segments[0];
        Ok(json!({
            "service":"hyperion-range-server","version":env!("CARGO_PKG_VERSION"),"api":1,
            "chainId":self.chain_id,"profile":self.protocol.id(),"nodeRevision":self.profile.upstream_revision,
            "shipAbiSha256":genesis.ship_abi_sha256,"docProfile":self.defaults.doc_profile,
            "genesisCatalog":genesis.manifest_sha256,
            "catalog":{"manifestSha256":newest.manifest_sha256,"endBlock":newest.end_block,
                "lastBlockId":newest.last_block_id,"segments":sealed.chain_segments.len()},
            "sealedLib":b.sealed_lib,
            "sealedLibId":if b.sealed_lib >= 1 {Value::from(hex::encode(sealed.newest.block_id(b.sealed_lib)?))} else {Value::Null},
            "lib":b.blocks.1,"head":b.trace.1.min(b.state.1),
            "logs":{"trace":[b.trace.0,b.trace.1],"chainState":[b.state.0,b.state.1],"blocks":[b.blocks.0,b.blocks.1]},
            "kinds":range::KINDS,"chunkBlocks":range::CHUNK,"maxRangeBlocks":self.max_range,
            "threads":self.threads,"follow":self.follow,
            "seal":self.sealing.map(|(blocks, wait)| {
                let st = self.seal_status.lock().unwrap();
                json!({"everyBlocks":blocks,"maxWaitMs":wait,"segmentsSealed":st.segments,
                    "lastSealMs":st.last_seal_ms,"lastError":st.last_error,
                    "unsealedBlocks":b.blocks.1.saturating_sub(newest.end_block)})
            }),
            "index":{"prefix":self.defaults.index_prefix,"version":self.defaults.index_version,
                "partitionSize":self.defaults.partition_size},
        }))
    }
}

/// The trace and chain-state logs must name the catalog's last block as the catalog does.
fn check_logs(trace: &ShipFile, state: &ShipFile, newest: &Loaded) -> Result<()> {
    let end = newest.segment.end_block;
    for (log, label) in [(trace, "trace"), (state, "chain-state")] {
        if log.last_block()?.is_some_and(|last| last >= end) && log.first_block() <= end {
            let slab = log.slab(end, end)?;
            let id = hex::encode(slab.entry(end)?.id());
            ensure!(
                id == newest.segment.last_block_id,
                "the {label} log names {id} at block {end}, but catalog {} sealed {}; the logs forked past it or belong to another node",
                newest.segment.manifest_sha256,
                newest.segment.last_block_id
            );
        }
    }
    Ok(())
}

fn build(chain: &Chain) -> Result<Sealed> {
    let records = &chain.newest.deployments[..];
    Ok(Sealed {
        chain_segments: chain.segments.clone(),
        newest: chain.newest.clone(),
        actions: action_proto::abi_index_from(records)?,
        deltas: delta_proto::wire::catalog_from(records)?,
    })
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(format!("{body}\n").into_bytes())
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
}

fn error(status: u16, code: &str, message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(status, &json!({"error":code,"message":message}))
}

fn read_body(req: &mut tiny_http::Request) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    req.as_reader().take(65_537).read_to_end(&mut body)?;
    ensure!(body.len() <= 65_536, "request body exceeds 64 KiB");
    Ok(body)
}

/// Counts a streaming range and resizes every range's window to the budget's share.
struct Active<'a>(&'a Service);

impl<'a> Active<'a> {
    fn enter(svc: &'a Service) -> Self {
        let n = svc.active.fetch_add(1, Relaxed) + 1;
        svc.window_limit
            .store((svc.threads / n).max(1) + 2, Relaxed);
        Self(svc)
    }
}

impl Drop for Active<'_> {
    fn drop(&mut self) {
        let n = self.0.active.fetch_sub(1, Relaxed) - 1;
        self.0
            .window_limit
            .store((self.0.threads / n.max(1)).max(1) + 2, Relaxed);
    }
}

fn serve_range(svc: &Service, mut req: tiny_http::Request) -> Result<()> {
    let body = match read_body(&mut req) {
        Ok(b) => b,
        Err(e) => return Ok(req.respond(error(413, "invalid_request", &format!("{e:#}")))?),
    };
    let sealed = svc.sealed();
    let bounds = svc.bounds(&sealed)?;
    let request = match range::parse(svc, &body, bounds.sealed_lib) {
        Ok(r) => r,
        Err(r) => {
            let mut v = json!({"error":r.code,"message":r.message});
            if let Value::Object(extra) = r.extra {
                v.as_object_mut().unwrap().extend(extra);
            }
            return Ok(req.respond(json_response(r.status, &v))?);
        }
    };
    if let Err(e) = sealed.newest.revalidate() {
        return Ok(req.respond(error(503, "catalog_changed", &format!("{e:#}")))?);
    }
    let _active = Active::enter(svc);
    let plan = range::plan(svc, &sealed, &request)?;

    let stream_id = svc.next_stream.fetch_add(1, Relaxed);
    let credits = (request.credits > 0).then(|| Arc::new(range::Credits::new(request.credits)));
    if let Some(c) = &credits {
        svc.streams.lock().unwrap().insert(stream_id, c.clone());
    }
    let started = Instant::now();
    let mut out = req.into_writer();
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nTransfer-Encoding: chunked\r\nConnection: close\r\nX-Range-Stream: {stream_id}\r\nX-Range-Catalog: {}\r\n\r\n",
        plan.catalog_sha
    );
    let result = out
        .write_all(head.as_bytes())
        .map_err(anyhow::Error::from)
        .and_then(|()| range::stream(svc, &sealed, &request, &plan, credits.as_ref(), &mut *out));
    svc.streams.lock().unwrap().remove(&stream_id);
    let blocks = request.to - request.from + 1;
    let secs = started.elapsed().as_secs_f64();
    match &result {
        Ok(()) => eprintln!(
            "[range-server] stream {stream_id}: {}..={} ({blocks} blocks) in {secs:.2}s ({:.0} blocks/s, {} threads)",
            request.from,
            request.to,
            blocks as f64 / secs.max(1e-9),
            plan.threads
        ),
        Err(e) => eprintln!(
            "[range-server] stream {stream_id}: {}..={} failed after {secs:.2}s: {e:#}",
            request.from, request.to
        ),
    }
    Ok(())
}

fn handle(svc: &Service, mut req: tiny_http::Request) -> Result<()> {
    let url = req.url().to_owned();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let method = req.method().clone();
    match (&method, parts.as_slice()) {
        (Method::Get, ["v1", "health"]) => {
            Ok(req.respond(json_response(200, &json!({"ok":true})))?)
        }
        (Method::Get, ["v1", "info"]) => match svc.info() {
            Ok(v) => Ok(req.respond(json_response(200, &v))?),
            Err(e) => Ok(req.respond(error(503, "unavailable", &format!("{e:#}")))?),
        },
        (Method::Get, ["v1", "catalog"]) => {
            let after = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("after="))
                .filter(|s| !s.is_empty());
            let sealed = svc.sealed();
            let start = match after {
                None => 0,
                Some(sha) => match sealed
                    .chain_segments
                    .iter()
                    .position(|s| s.manifest_sha256 == sha)
                {
                    Some(i) => i + 1,
                    None => {
                        return Ok(req.respond(error(
                            404,
                            "unknown_catalog",
                            &format!("catalog {sha} is not in the verified chain"),
                        ))?);
                    }
                },
            };
            let segments: Vec<Value> = sealed.chain_segments[start..]
                .iter()
                .map(|s| {
                    json!({"manifestSha256":s.manifest_sha256,"endBlock":s.end_block,"lastBlockId":s.last_block_id,
                        "base":s.base.as_ref().map(|b| json!({"manifestSha256":b.manifest_sha256,"endBlock":b.end_block,"lastBlockId":b.last_block_id})),
                        "records":s.records,"manifest":s.manifest})
                })
                .collect();
            Ok(req.respond(json_response(
                200,
                &json!({"genesis":sealed.chain_segments[0].manifest_sha256,
                    "newest":sealed.newest.segment.manifest_sha256,"segments":segments}),
            ))?)
        }
        (Method::Get, ["v1", "catalog", sha, name]) => {
            let sealed = svc.sealed();
            let file = sealed
                .chain_segments
                .iter()
                .find(|s| s.manifest_sha256 == *sha)
                .and_then(|s| member(s, name));
            match file {
                Some(path) => match std::fs::File::open(&path) {
                    Ok(f) => Ok(req.respond(Response::from_file(f).with_header(
                        Header::from_bytes("Content-Type", "application/octet-stream").unwrap(),
                    ))?),
                    Err(e) => Ok(req.respond(error(503, "unavailable", &e.to_string()))?),
                },
                None => Ok(req.respond(error(404, "not_found", "no such catalog member"))?),
            }
        }
        (Method::Post, ["v1", "ranges"]) => serve_range(svc, req),
        (Method::Post, ["v1", "ranges", id, "credits"]) => {
            let body = read_body(&mut req).unwrap_or_default();
            let frames = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|v| v["frames"].as_u64());
            let stream = id
                .parse::<u64>()
                .ok()
                .and_then(|id| svc.streams.lock().unwrap().get(&id).cloned());
            match (stream, frames) {
                (Some(s), Some(frames)) => {
                    let left = s.grant(frames);
                    Ok(req.respond(json_response(200, &json!({"stream":id,"credits":left})))?)
                }
                (None, _) => {
                    Ok(req.respond(error(404, "unknown_stream", "no such credited stream"))?)
                }
                (_, None) => {
                    Ok(req.respond(error(400, "invalid_request", "`frames` must be a count"))?)
                }
            }
        }
        (Method::Delete, ["v1", "ranges", id]) => {
            let stream = id
                .parse::<u64>()
                .ok()
                .and_then(|id| svc.streams.lock().unwrap().get(&id).cloned());
            match stream {
                Some(s) => {
                    s.cancel();
                    Ok(req.respond(json_response(200, &json!({"stream":id,"cancelled":true})))?)
                }
                None => Ok(req.respond(error(404, "unknown_stream", "no such credited stream"))?),
            }
        }
        _ => Ok(req.respond(error(404, "not_found", "no such endpoint"))?),
    }
}

/// A segment's manifest or one of its members, by the name the manifest gives it.
fn member(segment: &Segment, name: &str) -> Option<PathBuf> {
    let dir = segment.manifest_path.parent()?;
    let manifest_name = segment.manifest_path.file_name()?.to_str()?;
    let names = [
        manifest_name,
        segment.manifest["deployments"]["name"].as_str()?,
        segment.manifest["blockIds"]["name"].as_str()?,
    ];
    names.contains(&name).then(|| dir.join(name))
}

fn main() -> Result<()> {
    let args = Args::parse();
    args.protocol.validate()?;
    let ProtocolProvider::Wire(profile) = &args.protocol else {
        bail!("the range service serves Wire logs; select a Wire --protocol");
    };
    ensure!(
        args.chain_id.len() == 64
            && args
                .chain_id
                .bytes()
                .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')),
        "--chain-id must be 64 lowercase hex characters"
    );
    action_proto::init_protocol(&args.protocol)?;
    let timestamps = args.protocol.timestamp_profile()?;
    let trace = ShipFile::open(&args.state_history, "trace_history")?;
    let state = ShipFile::open(&args.state_history, "chain_state_history")?;
    let blocks = BlockFile::open(&args.blocks_dir)?;
    let pin = Pin {
        chain_id: args.chain_id.clone(),
        node_revision: profile.upstream_revision.clone(),
        producer_profile_id: args.protocol.id().to_owned(),
    };
    let t0 = Instant::now();
    let chain = Chain::discover(&args.catalog_dir, &args.genesis_catalog_sha256, pin)?;
    check_logs(&trace, &state, &chain.newest)?;
    let sealed = build(&chain)?;
    eprintln!(
        "[range-server] catalog chain verified in {:.2}s: {} segment(s), sealed through block {} ({})",
        t0.elapsed().as_secs_f64(),
        chain.segments.len(),
        chain.newest.segment.end_block,
        chain.newest.segment.manifest_sha256
    );
    let threads = match args.threads {
        0 => std::thread::available_parallelism().map_or(1, usize::from),
        n => n,
    };
    let svc = Arc::new(Service {
        defaults: Defaults {
            doc_profile: args
                .doc_profile_id
                .clone()
                .unwrap_or_else(|| profile.id.clone()),
            index_prefix: args.index_prefix.clone(),
            index_version: args.index_version.clone(),
            partition_size: args.partition_size.max(1),
            index_transfer_memo: args.index_transfer_memo,
            system_contract: args.system_contract.clone(),
        },
        profile: profile.clone(),
        protocol: args.protocol.clone(),
        timestamps,
        chain_id: args.chain_id.clone(),
        trace,
        state,
        blocks,
        threads,
        range_threads: match args.range_threads {
            0 => threads,
            n => n.min(threads),
        },
        max_range: args.max_range.max(1),
        credit_timeout: Duration::from_secs(args.credit_timeout.max(1)),
        state_history: args.state_history.clone(),
        catalog_dir: args.catalog_dir.clone(),
        follow: args.follow,
        sealing: args
            .seal
            .then_some((args.seal_blocks.max(1), args.seal_max_wait_ms)),
        seal_status: Mutex::new(seal::Status::default()),
        chain: Mutex::new(chain),
        current: RwLock::new(Arc::new(sealed)),
        permits: range::Permits::new(threads),
        active: AtomicUsize::new(0),
        window_limit: AtomicUsize::new(threads + 2),
        next_stream: AtomicU64::new(1),
        streams: Mutex::new(HashMap::new()),
    });

    if svc.follow {
        let svc = svc.clone();
        let every = Duration::from_millis(args.poll_ms.max(50));
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(every);
                let mut chain = svc.chain.lock().unwrap();
                match chain.follow(&svc.catalog_dir) {
                    Ok(false) => {}
                    Ok(true) => match svc.view(&chain) {
                        Ok(sealed) => {
                            eprintln!(
                                "[range-server] adopted catalog {}: sealed through block {}",
                                chain.newest.segment.manifest_sha256,
                                chain.newest.segment.end_block
                            );
                            *svc.current.write().unwrap() = Arc::new(sealed);
                        }
                        Err(e) => eprintln!("[range-server] catalog not served: {e:#}"),
                    },
                    Err(e) => eprintln!(
                        "[range-server] following {}: {e:#}",
                        svc.catalog_dir.display()
                    ),
                }
            }
        });
    }

    if args.seal {
        let cfg = seal::Config {
            schema: std::fs::read_to_string(args.ship_abi.as_deref().unwrap())
                .context("read --ship-abi")?,
            dir: seal::dir(&args.catalog_dir, args.seal_dir.as_deref()),
            blocks: args.seal_blocks.max(1),
            max_wait: Duration::from_millis(args.seal_max_wait_ms),
            poll: Duration::from_millis(args.seal_poll_ms.max(10)),
            threads: args.seal_threads.max(1),
        };
        std::fs::create_dir_all(&cfg.dir)
            .with_context(|| format!("create {}", cfg.dir.display()))?;
        let svc = svc.clone();
        std::thread::spawn(move || seal::run(svc, cfg));
    }

    let server = Server::http(&args.listen)
        .map_err(|e| anyhow::anyhow!("listen on {}: {e}", args.listen))?;
    let addr = server
        .server_addr()
        .to_ip()
        .map_or_else(|| args.listen.clone(), |a| a.to_string());
    println!("listening on {addr}");
    std::io::stdout().flush().ok();
    eprintln!("[range-server] listening on {addr} ({threads} worker threads)");
    for req in server.incoming_requests() {
        let svc = svc.clone();
        std::thread::spawn(move || {
            if let Err(e) = handle(&svc, req) {
                eprintln!("[range-server] request failed: {e:#}");
            }
        });
    }
    Ok(())
}
