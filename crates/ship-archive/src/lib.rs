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
//!   * [`fetch`] holds a result ready to send and builds the next one ahead of the writer.
//!   * [`frames`] writes a result as Spring frames it, streaming payloads above
//!     `--stream-threshold`.
//!   * [`cli`] is the command line.
//!
//! Memory is bounded per session: a session holds the result it is sending and at most one
//! built ahead, each at most three payloads of up to `--stream-threshold` bytes and one block
//! (at most 20 MiB), plus 2 MiB of streaming buffers. Larger payloads are inflated chunk by
//! chunk as they are sent.

pub mod cli;
pub mod fetch;
pub mod frames;
pub mod session;
pub mod store;

use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use hyperion_ship::catalog::Finding;
use hyperion_ship::shiplog::LogKind;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tracing::{error, info, warn};

use crate::fetch::FetchLimits;
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
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            store: StoreConfig::default(),
            listen: SocketAddr::from((Ipv4Addr::LOCALHOST, 8080)),
            max_sessions: 256,
            max_request_bytes: 1 << 20,
            stream_threshold: 4 << 20,
        }
    }
}

/// An open store bound to its listening socket, ready to [`run`](Self::run).
pub struct Server {
    store: Arc<Store>,
    findings: Vec<Finding>,
    listener: TcpListener,
    cfg: ServeConfig,
}

impl Server {
    /// Open and validate the store, log what is served, and bind the listener.
    pub async fn bind(cfg: ServeConfig) -> Result<Self> {
        ensure!(cfg.max_sessions > 0, "--max-sessions must be at least 1");
        ensure!(
            cfg.max_request_bytes >= 64,
            "--max-request-bytes must be at least 64"
        );
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
        Ok(Self {
            store: Arc::new(store),
            findings,
            listener,
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

    /// Accept sessions until `shutdown` completes, then send Close 1001 to every session, give
    /// them five seconds to finish the message they are writing, and drop the rest.
    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<()> {
        let Self {
            store,
            listener,
            cfg,
            ..
        } = self;
        let ws = WebSocketConfig::default()
            .read_buffer_size(READ_BUFFER)
            .max_message_size(Some(cfg.max_request_bytes))
            .max_frame_size(Some(cfg.max_request_bytes));
        let ctx = Arc::new(Ctx {
            store,
            limits: FetchLimits {
                stream_threshold: cfg.stream_threshold,
            },
            ws,
            server: HeaderValue::from_static(SERVER_HEADER),
        });
        let permits = Arc::new(Semaphore::new(cfg.max_sessions));
        let (stop_tx, stop_rx) = watch::channel(false);
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
        info!("shutting down: closing {} sessions", sessions.len());
        stop_tx.send_replace(true);
        let drain = async { while sessions.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            warn!(
                "{} sessions still open after {SHUTDOWN_GRACE:?}; dropping them",
                sessions.len()
            );
            sessions.shutdown().await;
        }
        Ok(())
    }
}

/// Open the store, bind, and serve until `shutdown` completes.
pub async fn serve(cfg: ServeConfig, shutdown: impl Future<Output = ()>) -> Result<()> {
    Server::bind(cfg).await?.run(shutdown).await
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
    if cfg.store.blocks == BlocksMode::Required {
        for kind in LogKind::ALL {
            if let Some(catalog) = store.catalog(kind) {
                let (held, served) = (catalog.range(), store.served.ship(kind));
                if held != served && cfg.store.range.is_none() {
                    warn!(
                        "the {} logs hold {}..{} but only {} is served: the block logs do not \
                         cover the rest",
                        kind.label(),
                        held.0,
                        held.1,
                        show(served)
                    );
                }
            }
        }
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
