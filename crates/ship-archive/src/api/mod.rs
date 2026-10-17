//! The chain API (`--api <ADDR>`): nodeos-compatible `/v1/chain` block endpoints answered from
//! the served block logs, on a listener of its own, for HTTP clients and for an HTTP router or
//! proxy in front of nodeos-compatible APIs. Off by default.
//!
//! | target                         | answer                                                   |
//! |--------------------------------|----------------------------------------------------------|
//! | `/v1/chain/get_block_info`     | the block's info, as nodeos renders it                   |
//! | `/v1/chain/get_block_header`   | the signed header, and with `include_extensions` the block extensions |
//! | `/v1/chain/get_raw_block`      | the whole `signed_block`, transactions as stored          |
//! | `/v1/node/get_supported_apis`  | `{"apis":[...]}`, the three paths above, sorted          |
//! | `/v1/archive/describe`         | what this server serves ([`describe`]), for a router     |
//! | anything else                  | nodeos's `404 Unknown Endpoint`, `get_block` and `get_info` included |
//!
//! A block body is byte for byte what a Spring 1.1.0 to 1.2.2 node holding the same block
//! answers, and so are the request parsing and the error bodies (`hyperion_ship::chainapi`).
//! A block is found when its number is in the served block-log range (`--range` applies), and,
//! asked by id, when the stored block hashes to that id; anything else is `Unknown Block`.
//!
//! HTTP is HTTP/1.1 as nodeos's Beast server speaks it: the whole raw request-target picks the
//! handler (a query string is another target), any method reaches it, `OPTIONS` is `200 {}`,
//! responses are `Content-Type: application/json` with a `Content-Length`, title-case header
//! names and no `Date`, and connections are kept alive as the client asks. A request body is
//! at most `--api-max-body-bytes`: a larger one gets `413` and the connection is closed. The
//! headers of a request must arrive within [`REQUEST_TIMEOUT`], and then its body within as
//! long, or the connection is closed unanswered; a keep-alive connection idle that long is
//! closed too. There is no TLS, authentication, CORS or `Host` check: bind the listener to a
//! private address.
//!
//! **Requests and memory.** A request for a block number outside the served range is answered
//! at once. Any other block request takes one of `--api-max-in-flight` slots, waiting at most
//! `--api-queue-ms` for one, and with it one io thread, where its block is read and rendered;
//! the default leaves at least three quarters of the io threads to SHiP sessions. What it holds is reserved from the memory budget SHiP sessions build ahead in,
//! first and without waiting: the block, and room for the body, 64 KiB for `get_block_info`
//! and `get_block_header` and three times the block more for the whole block. A body that
//! outgrows that room (a block of many deferred receipts) is rendered once more under a
//! reservation no block can outgrow. Once rendered, the body alone keeps its reservation until
//! it is written out, so a client that does not read holds budget, never memory beyond it. A
//! full slot queue, a refused reservation, and a block that cannot be read or does not decode
//! are answered `503 Busy`, which a router may retry elsewhere; nothing here answers 500 but
//! nodeos's own id and length errors.
//!
//! **Shutdown.** The listener stops accepting with the SHiP listener; each connection finishes
//! the request it is answering and closes, within the server's shutdown grace.

mod describe;
mod handlers;

pub use describe::{DESCRIBE, GET_BLOCK, SUPPORTED_APIS};

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;
use tracing::{debug, error, info, warn};

use crate::fetch::{Io, MemoryBudget};
use crate::metrics::{ApiRejection, Metrics};
use crate::store::Store;

/// How long the headers of a request may take to arrive, and then its body; also how long a
/// keep-alive connection may stay idle.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a request waits for a slot by default (`--api-queue-ms`).
pub const DEFAULT_QUEUE: Duration = Duration::from_millis(250);
/// The largest request body by default (`--api-max-body-bytes`): nodeos's `max-body-size`.
pub const DEFAULT_MAX_BODY_BYTES: usize = 2 << 20;
/// hyper's connection buffer, which bounds a request head: requests are small.
const MAX_BUF: usize = 16 << 10;
/// The pause after a failed accept (out of descriptors, for one), so the loop does not spin.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Requests read and rendered at once by default: a quarter of the io threads, at least 2.
pub fn default_max_in_flight(io_threads: usize) -> usize {
    (io_threads / 4).max(2)
}

/// Where and how the chain API is served.
#[derive(Clone, Debug)]
pub struct ApiConfig {
    pub listen: SocketAddr,
    /// Block requests read and rendered at once (`--api-max-in-flight`), each on an io thread.
    pub max_in_flight: usize,
    /// How long a block request waits for a slot before `503 Busy` (`--api-queue-ms`).
    pub queue: Duration,
    /// The largest request body accepted (`--api-max-body-bytes`).
    pub max_body_bytes: usize,
    /// How long the headers, then the body, of a request may take ([`REQUEST_TIMEOUT`]).
    pub request_timeout: Duration,
    /// A pause in each block request's job before it reads, holding its slot and its io
    /// thread: for tests of admission and shutdown.
    #[doc(hidden)]
    pub read_delay: Duration,
}

impl ApiConfig {
    /// The defaults for a server with `io_threads` io threads.
    pub fn new(listen: SocketAddr, io_threads: usize) -> Self {
        Self {
            listen,
            max_in_flight: default_max_in_flight(io_threads),
            queue: DEFAULT_QUEUE,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            request_timeout: REQUEST_TIMEOUT,
            read_delay: Duration::ZERO,
        }
    }
}

/// What every chain API request shares.
pub(crate) struct Ctx {
    pub store: Arc<Store>,
    pub io: Io,
    pub budget: Arc<MemoryBudget>,
    pub metrics: Arc<Metrics>,
    pub cfg: ApiConfig,
    /// `--api-max-in-flight` slots.
    pub slots: Arc<Semaphore>,
}

/// The bound chain API listener, not serving yet.
pub(crate) struct Listener {
    listener: TcpListener,
    cfg: ApiConfig,
}

impl Listener {
    pub async fn bind(cfg: ApiConfig) -> Result<Self> {
        ensure!(
            cfg.max_in_flight > 0,
            "--api-max-in-flight must be at least 1"
        );
        ensure!(
            cfg.max_body_bytes >= 64,
            "--api-max-body-bytes must be at least 64"
        );
        let listener = TcpListener::bind(cfg.listen)
            .await
            .with_context(|| format!("bind --api {}", cfg.listen))?;
        Ok(Self { listener, cfg })
    }

    /// The bound address (the port chosen when listening on port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.listener
            .local_addr()
            .expect("a bound listener has an address")
    }

    pub fn config(&self) -> &ApiConfig {
        &self.cfg
    }

    /// Accept connections until `stop` changes (or its sender is gone), then let every
    /// connection finish the request it is answering, and return once all are closed.
    pub async fn run(
        self,
        store: Arc<Store>,
        io: Io,
        budget: Arc<MemoryBudget>,
        metrics: Arc<Metrics>,
        mut stop: watch::Receiver<bool>,
    ) {
        let Self { listener, cfg } = self;
        let ctx = Arc::new(Ctx {
            store,
            io,
            budget,
            metrics,
            slots: Arc::new(Semaphore::new(cfg.max_in_flight)),
            cfg,
        });
        let mut connections = JoinSet::new();
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = stop.changed() => break,
                Some(done) = connections.join_next(), if !connections.is_empty() => {
                    if let Err(e) = done {
                        error!("a chain API connection task failed: {e}");
                    }
                }
                accepted = listener.accept() => match accepted {
                    Ok((tcp, peer)) => {
                        let ctx = Arc::clone(&ctx);
                        connections.spawn(connection(tcp, peer, ctx, stop.clone()));
                    }
                    Err(e) => {
                        warn!("chain API accept failed: {e}");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                },
            }
        }
        drop(listener);
        if !connections.is_empty() {
            info!(
                "chain API: closing {} connections once their requests are answered",
                connections.len()
            );
        }
        while connections.join_next().await.is_some() {}
    }
}

/// Serve one connection until the client closes it, it times out, or the server stops.
async fn connection(
    tcp: TcpStream,
    peer: SocketAddr,
    ctx: Arc<Ctx>,
    mut stop: watch::Receiver<bool>,
) {
    if let Err(e) = tcp.set_nodelay(true) {
        debug!(%peer, "setting TCP_NODELAY on a chain API connection failed: {e}");
    }
    let requests = Arc::new(AtomicU64::new(0));
    let service = {
        let (ctx, requests) = (Arc::clone(&ctx), Arc::clone(&requests));
        service_fn(move |req| {
            requests.fetch_add(1, Ordering::Relaxed);
            handlers::handle(req, Arc::clone(&ctx), peer)
        })
    };
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(ctx.cfg.request_timeout)
        .auto_date_header(false)
        .title_case_headers(true)
        .max_buf_size(MAX_BUF)
        .keep_alive(true)
        // Bodies are queued as they are, never copied into the write buffer.
        .writev(true);
    let conn = http.serve_connection(TokioIo::new(tcp), service);
    tokio::pin!(conn);
    let done = tokio::select! {
        done = conn.as_mut() => done,
        // A change is the shutdown signal; an error means the server is gone.
        _ = stop.changed() => {
            conn.as_mut().graceful_shutdown();
            conn.await
        }
    };
    match done {
        Ok(()) => {}
        Err(e) if e.is_timeout() => {
            // The head timer also ends a keep-alive connection left idle: that is no rejection.
            if requests.load(Ordering::Relaxed) == 0 {
                ctx.metrics.api_rejected(ApiRejection::Timeout);
            }
            debug!(
                %peer,
                "chain API connection closed: no request head within {:?}",
                ctx.cfg.request_timeout
            );
        }
        Err(e) => debug!(%peer, "chain API connection ended: {e}"),
    }
}
