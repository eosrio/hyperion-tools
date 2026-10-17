//! A SHiP (state-history) WebSocket server over frozen logs: Antelope state-history logs
//! (`trace_history`, `chain_state_history`, `finality_data_history`) and block logs, flat or
//! retained, served exactly as a Spring node serves them, without nodeos.
//!
//! It is the archive tier of a range-split history: each server holds one contiguous range of
//! irreversible blocks and reports its last block as head and LIB, and a SHiP-aware router or
//! proxy stitches such servers and live nodes into one history for its clients.
//!
//! Module map:
//!   * [`store`] opens and validates the logs once (fail closed), computes the served ranges
//!     and the status, and builds each block result.
//!   * [`session`] is the Spring session replica: credit, cursor, `have_positions`, status
//!     queue, and the reader and writer over one connection.
//!   * [`fetch`] holds a result ready to send, builds the next ones ahead of the writer
//!     within the global memory budget, and runs every file read on the io threads.
//!   * [`frames`] writes a result as Spring frames it, streaming payloads above
//!     `--stream-threshold`.
//!   * [`api`] serves nodeos's `/v1/chain` block endpoints from the block logs (`--api`), on
//!     the same io threads and memory budget.
//!   * [`metrics`] counts what the sessions and the chain API do; `http` serves `/health`,
//!     `/ready` and `/metrics` (`--http`).
//!   * [`check`] holds the offline commands: `check` validates the logs without serving them,
//!     `index` rebuilds state-history indexes into a sidecar directory.
//!   * [`cli`] is the command line.
//!
//! Memory is bounded: results built ahead of the writers share `--memory-budget`, and each
//! session holds besides them the result it is sending, at most three payloads of up to
//! `--stream-threshold` bytes and one block (at most 20 MiB), plus its streaming and write
//! buffers ([`fetch::SESSION_BUFFERS`]). Larger payloads are inflated chunk by chunk as they are
//! sent. [`fetch::worst_case_bytes`] is the bound. Chain API requests reserve what they hold
//! from the same budget, never waiting for it, and keep it until their body is written out or
//! the client has taken none of it for the request timeout; besides the budget each of at most
//! `--api-max-connections` connections holds only its buffers and up to 16 KiB of request.

pub mod api;
pub mod check;
pub mod cli;
pub mod fetch;
pub mod frames;
mod http;
pub mod metrics;
pub mod session;
pub mod store;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use futures::StreamExt;
use hyperion_ship::catalog::Finding;
use hyperion_ship::protocol::MAX_BLOCK_BYTES;
use hyperion_ship::shiplog::LogKind;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{error, info, warn};

use crate::api::ApiConfig;
use crate::fetch::{FetchCtx, FetchLimits, Io, MemoryBudget};
use crate::metrics::Metrics;
use crate::session::Ctx;
use crate::store::{BlocksMode, Store, StoreConfig};

/// The `Server` header of every upgrade response: informational, as Spring's
/// `state_history/<version>`.
pub const SERVER_HEADER: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// How long sessions get to finish after shutdown starts, before they are dropped.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// The pause after a failed accept (out of descriptors, for one), so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// The read buffer of a session: client requests are small.
const READ_BUFFER: usize = 16 << 10;

/// Everything `serve` needs.
#[derive(Clone, Debug)]
pub struct ServeConfig {
    pub store: StoreConfig,
    pub listen: SocketAddr,
    /// Upgrades beyond this many open sessions get HTTP 503.
    pub max_sessions: usize,
    /// The largest client message accepted; a larger one drops the connection.
    pub max_request_bytes: usize,
    /// Payloads inflating to more bytes are streamed rather than held in memory.
    pub stream_threshold: u64,
    /// File reads and inflation running at once, at most.
    pub io_threads: usize,
    /// Results each session builds ahead of its writer, at most.
    pub prefetch: usize,
    /// Bytes all results built ahead may hold together.
    pub memory_budget: u64,
    /// Where to serve `/health`, `/ready` and `/metrics`; nowhere when `None`.
    pub http: Option<SocketAddr>,
    /// Where and how to serve the chain API; nowhere when `None`.
    pub api: Option<ApiConfig>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            store: StoreConfig::default(),
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            max_sessions: 256,
            max_request_bytes: 1 << 20,
            stream_threshold: 4 << 20,
            io_threads: default_io_threads(),
            prefetch: 8,
            memory_budget: 1 << 30,
            http: None,
            api: None,
        }
    }
}

/// Twice the available parallelism, at most 16: reads mostly wait on the disk.
pub fn default_io_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (2 * cores).min(16)
}

/// An open store bound to its listening socket, ready to [`run`](Self::run).
pub struct Server {
    store: Arc<Store>,
    findings: Arc<[Finding]>,
    listener: TcpListener,
    http: Option<http::Http>,
    api: Option<api::Listener>,
    metrics: Arc<Metrics>,
    budget: Arc<MemoryBudget>,
    cfg: ServeConfig,
}

impl Server {
    /// Open and validate the store, log what is served, and bind the listeners.
    pub async fn bind(cfg: ServeConfig) -> Result<Self> {
        ensure!(cfg.max_sessions > 0, "--max-sessions must be at least 1");
        ensure!(
            cfg.max_request_bytes >= 64,
            "--max-request-bytes must be at least 64"
        );
        ensure!(cfg.io_threads > 0, "--io-threads must be at least 1");
        if let Some(api) = &cfg.api {
            ensure!(
                cfg.store.blocks == BlocksMode::Required,
                "--api answers from the block logs, so it cannot run with --blocks none"
            );
            ensure!(
                !same_listener(api.listen, cfg.listen),
                "--api {} is the --listen address: the chain API needs a listener of its own",
                api.listen
            );
            if let Some(http) = cfg.http {
                ensure!(
                    !same_listener(api.listen, http),
                    "--api {} is the --http address: the chain API needs a listener of its own",
                    api.listen
                );
            }
        }
        let store_cfg = cfg.store.clone();
        let (store, findings) = tokio::task::spawn_blocking(move || Store::open(&store_cfg))
            .await
            .context("store open task")??;
        log_store(&store, &findings, &cfg);
        let listener = TcpListener::bind(cfg.listen)
            .await
            .with_context(|| format!("bind {}", cfg.listen))?;
        info!(
            "listening on ws://{} (at most {} sessions)",
            listener.local_addr()?,
            cfg.max_sessions
        );
        let http = cfg.http.map(http::Http::bind).transpose()?;
        if let Some(http) = &http {
            info!(
                "serving /health, /ready and /metrics on http://{}",
                http.addr()
            );
        }
        let api = match cfg.api.clone() {
            Some(api) => Some(api::Listener::bind(api).await?),
            None => None,
        };
        if let Some(api) = &api {
            let (begin, end) = store.served.blocks;
            let c = api.config();
            info!(
                "serving the chain API on http://{} for blocks {begin}..{end}: at most {} \
                 connections and {} requests at once, each waiting up to {:?} for its turn; \
                 bodies up to {} bytes",
                api.local_addr(),
                c.max_connections,
                c.max_in_flight,
                c.queue,
                c.max_body_bytes
            );
        }
        let limits = FetchLimits {
            stream_threshold: cfg.stream_threshold,
        };
        info!(
            "{} io threads; each session builds up to {} results ahead within a {}-byte memory \
             budget; at most {} bytes of results in all",
            cfg.io_threads,
            cfg.prefetch,
            cfg.memory_budget,
            fetch::worst_case_bytes(
                cfg.memory_budget,
                cfg.max_sessions,
                &limits,
                MAX_BLOCK_BYTES as u64
            )
        );
        Ok(Self {
            store: Arc::new(store),
            findings: findings.into(),
            listener,
            http,
            api,
            metrics: Arc::new(Metrics::default()),
            budget: MemoryBudget::new(cfg.memory_budget),
            cfg,
        })
    }

    /// The bound address (the port chosen when listening on port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has an address")
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// What opening the store skipped or noticed.
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// The bound address of the HTTP endpoints, when `--http` is given.
    pub fn http_addr(&self) -> Option<SocketAddr> {
        self.http.as_ref().map(http::Http::addr)
    }

    /// The bound address of the chain API, when `--api` is given.
    pub fn api_addr(&self) -> Option<SocketAddr> {
        self.api.as_ref().map(api::Listener::local_addr)
    }

    /// The counters the sessions update.
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    /// The memory budget results built ahead share.
    pub fn budget(&self) -> &Arc<MemoryBudget> {
        &self.budget
    }

    /// Accept sessions and chain API requests until `shutdown` completes. Then stop accepting,
    /// send Close 1001 to every session, let every API connection finish the request it is
    /// answering, give both five seconds to finish, and drop the rest.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let Self {
            store,
            findings,
            listener,
            http,
            api,
            metrics,
            budget,
            cfg,
        } = self;
        let ws = WebSocketConfig::default()
            .read_buffer_size(READ_BUFFER)
            .max_message_size(Some(cfg.max_request_bytes))
            .max_frame_size(Some(cfg.max_request_bytes));
        // Sessions and API requests share the io threads.
        let io = Io::new(cfg.io_threads, Arc::clone(&metrics));
        let ctx = Arc::new(Ctx {
            fetch: FetchCtx {
                store: Arc::clone(&store),
                limits: FetchLimits {
                    stream_threshold: cfg.stream_threshold,
                },
                io: io.clone(),
                budget: Arc::clone(&budget),
                depth: cfg.prefetch,
            },
            ws,
            server: HeaderValue::from_static(SERVER_HEADER),
            metrics: Arc::clone(&metrics),
        });
        let (stop_tx, stop_rx) = watch::channel(false);
        let api_health = api.as_ref().map(|a| http::ApiHealth {
            listen: a.local_addr(),
            max_in_flight: a.config().max_in_flight,
        });
        let mut api = api.map(|a| {
            tokio::spawn(a.run(
                Arc::clone(&store),
                io,
                Arc::clone(&budget),
                Arc::clone(&metrics),
                stop_rx.clone(),
            ))
        });
        let state = Arc::new(http::State {
            store,
            findings,
            metrics,
            budget,
            api: api_health,
            ready: true.into(),
        });
        let serving = http.map(|h| h.spawn(Arc::clone(&state))).transpose()?;
        let permits = Arc::new(Semaphore::new(cfg.max_sessions));
        let mut sessions = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                biased;
                () = &mut shutdown => break,
                Some(done) = sessions.join_next(), if !sessions.is_empty() => {
                    if let Err(e) = done {
                        error!("a session task failed: {e}");
                    }
                }
                accepted = listener.accept() => match accepted {
                    Ok((tcp, peer)) => {
                        let permit = Arc::clone(&permits).try_acquire_owned().ok();
                        let ctx = Arc::clone(&ctx);
                        sessions.spawn(session::run(tcp, peer, permit, ctx, stop_rx.clone()));
                    }
                    Err(e) => {
                        warn!("accept failed: {e}");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                },
            }
        }
        drop(listener);
        state.ready.store(false, Ordering::Relaxed);
        info!("shutting down: closing {} sessions", sessions.len());
        stop_tx.send_replace(true);
        let drain = async {
            while sessions.join_next().await.is_some() {}
            if let Some(api) = &mut api
                && let Err(e) = api.await
            {
                error!("the chain API task failed: {e}");
            }
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            if !sessions.is_empty() {
                warn!(
                    "{} sessions still open after {SHUTDOWN_GRACE:?}; dropping them",
                    sessions.len()
                );
                sessions.shutdown().await;
            }
            if let Some(api) = api.take()
                && !api.is_finished()
            {
                warn!("chain API requests still open after {SHUTDOWN_GRACE:?}; dropping them");
                api.abort();
            }
        }
        drop(serving);
        Ok(())
    }
}

/// Whether listening on `a` and on `b` takes the same port: the same nonzero port on the same
/// address, or on any address when either is the unspecified one.
fn same_listener(a: SocketAddr, b: SocketAddr) -> bool {
    a.port() != 0
        && a.port() == b.port()
        && (a.ip() == b.ip() || a.ip().is_unspecified() || b.ip().is_unspecified())
}

/// Open the store, bind, and serve until `shutdown` completes.
pub async fn serve(cfg: ServeConfig, shutdown: impl Future<Output = ()>) -> Result<()> {
    Server::bind(cfg).await?.run(shutdown).await
}

/// How long [`node_abi`] waits for the connection, and then for the ABI.
pub const ABI_TIMEOUT: Duration = Duration::from_secs(10);

/// The ABI text the SHiP node (or any SHiP server) at `url` (`ws://host:port`) sends first, to
/// serve with `--abi` when it differs from the vendored Spring one. It must be JSON.
pub async fn node_abi(url: &str) -> Result<String> {
    let connect = tokio_tungstenite::connect_async(url);
    let (mut ws, _) = tokio::time::timeout(ABI_TIMEOUT, connect)
        .await
        .with_context(|| format!("no connection to {url} within {ABI_TIMEOUT:?}"))?
        .with_context(|| format!("connect to {url}"))?;
    let first = tokio::time::timeout(ABI_TIMEOUT, ws.next())
        .await
        .with_context(|| format!("no message from {url} within {ABI_TIMEOUT:?}"))?;
    let text = match first {
        Some(Ok(Message::Text(text))) => text,
        Some(Ok(other)) => bail!(
            "the first message from {url} is not the ABI text but a {} message",
            message_kind(&other)
        ),
        Some(Err(e)) => return Err(e).with_context(|| format!("read from {url}")),
        None => bail!("{url} closed the connection before sending its ABI"),
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .with_context(|| format!("the ABI text from {url} is not JSON"))?;
    // A polite close; the ABI is what counts.
    let _ = tokio::time::timeout(Duration::from_secs(2), ws.close(None)).await;
    Ok(text.as_str().to_owned())
}

fn message_kind(m: &Message) -> &'static str {
    match m {
        Message::Text(_) => "text",
        Message::Binary(_) => "binary",
        Message::Ping(_) => "ping",
        Message::Pong(_) => "pong",
        Message::Close(_) => "close",
        Message::Frame(_) => "raw frame",
    }
}

/// The startup table: every file of every catalog, what is served of it, and the findings.
fn log_store(store: &Store, findings: &[Finding], cfg: &ServeConfig) {
    info!("chain {}", hex::encode(store.chain_id));
    let show = |(begin, end): (u32, u32)| {
        if begin == end {
            "nothing".to_owned()
        } else {
            format!("{begin}..{end}")
        }
    };
    for kind in LogKind::ALL {
        let served = show(store.served.ship(kind));
        let Some(catalog) = store.catalog(kind) else {
            info!("{:<11}  no files", kind.label());
            continue;
        };
        for log in catalog.files() {
            let (begin, end) = log.range();
            info!(
                "{:<11}  {begin}..{end}  {}",
                kind.label(),
                log.log_path().display()
            );
        }
        info!("{:<11}  serving {served}", kind.label());
    }
    match &store.blocks {
        Some(catalog) => {
            for log in catalog.files() {
                let (begin, end) = log.range();
                info!(
                    "{:<11}  {begin}..{end}  {}",
                    "blocks",
                    log.log_path().display()
                );
            }
            info!("{:<11}  serving {}", "blocks", show(store.served.blocks));
        }
        None => warn!(
            "--blocks none: results never carry `block`; a client asking for blocks gets them \
             from another source or not at all"
        ),
    }
    for warning in store.uncovered(&cfg.store) {
        warn!("{warning}");
    }
    for finding in findings {
        if finding.is_warning() {
            warn!("{finding}");
        } else {
            info!("{finding}");
        }
    }
    info!(
        "head and LIB: block {} {}",
        store.last.num,
        hex::encode(store.last.id)
    );
}
