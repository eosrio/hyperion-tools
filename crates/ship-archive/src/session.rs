//! One client connection, replicating a Spring SHiP session (`session.hpp`) over a frozen store.
//!
//! The upgrade is accepted on any path. Over `--max-sessions` it is refused with HTTP 503 and
//! `Retry-After: 1` instead, so a client never sees a session accepted and then dropped. The
//! first message is the ABI, as one Text message; every later one is Binary.
//!
//! As in Spring, a session is a reader and a writer over one state (`SpringState`): the
//! current request, whose `max_messages_in_flight` is the credit and whose `start_block_num` is
//! the cursor, plus the queue of status requests. Both run in the session's task, so when
//! either ends the other is dropped with the connection.
//!
//! * The reader decodes each message with fc semantics and applies it in order. A status
//!   request is queued (more than 1024 queued drops the connection). A `get_blocks` request
//!   replaces the current one, credit and cursor included, after Spring's `have_positions` rule
//!   moves its start back to the lowest position whose id is unknown or different. An ack adds
//!   to the credit, wrapping at `u32` as Spring's plain addition does.
//! * The writer answers every queued status first (no credit), then sends at most one block
//!   while `credit > 0 && cursor <= L* && cursor < end`, advancing the cursor and spending a
//!   credit for it, id-less results included. Nothing is ever sent unasked: at the end of a
//!   request the session idles, open, until the next request. While it writes one block, the
//!   blocks the send rule would allow next (the credit window) are built ahead of it, within
//!   `--prefetch` and the memory budget ([`crate::fetch::Prefetcher`]).
//!
//! A request that does not decode, a full status queue, and a read or integrity error drop the
//! TCP connection without a Close frame, as Spring does on any error. A result is validated
//! before its first frame, so only an I/O error while streaming a large payload can end one
//! mid-message. At shutdown each session finishes the message it is writing and sends Close
//! 1001.

use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::{Sink, SinkExt, Stream, StreamExt};
use hyperion_ship::protocol::{self, BlocksReq, Request};
use tokio::net::TcpStream;
use tokio::sync::{Notify, OwnedSemaphorePermit, watch};
use tokio_tungstenite::accept_hdr_async_with_config;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::server::{
    ErrorResponse, Request as HttpRequest, Response,
};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode, header};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tracing::{debug, info, warn};

use crate::fetch::{FetchCtx, Flags, Io, Job, Prefetcher, Window};
use crate::frames::{self, SendError};
use crate::metrics::Metrics;
use crate::store::Store;

/// Status requests queued beyond this many drop the connection.
pub const MAX_QUEUED_STATUS: usize = 1024;
/// How long a client has to complete the WebSocket upgrade.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a session waits for the client's Close reply after its own Close at shutdown.
const CLOSE_WAIT: Duration = Duration::from_secs(2);

/// What every session of a server shares.
pub(crate) struct Ctx {
    pub fetch: FetchCtx,
    pub ws: WebSocketConfig,
    /// The `Server` header of every upgrade response.
    pub server: HeaderValue,
    pub metrics: Arc<Metrics>,
}

impl Ctx {
    fn store(&self) -> &Arc<Store> {
        &self.fetch.store
    }
}

/// Spring's per-connection state. The credit is `req.max_in_flight` and the cursor is
/// `req.start`, as Spring aliases them into its current request. The initial request has
/// `end == 0`, so nothing is sent before the first `get_blocks`.
#[derive(Debug, Default)]
pub(crate) struct SpringState {
    req: BlocksReq,
    /// Bumped by every `get_blocks`, so that work taken for an older request is told apart.
    generation: u64,
    /// Queued status requests, `true` for v1.
    status_q: VecDeque<bool>,
}

impl SpringState {
    /// Queue a status request; `false` when [`MAX_QUEUED_STATUS`] are already queued.
    fn status(&mut self, v1: bool) -> bool {
        if self.status_q.len() >= MAX_QUEUED_STATUS {
            return false;
        }
        self.status_q.push_back(v1);
        true
    }

    /// Replace the request, credit and cursor included. Its `have_positions` have already been
    /// folded into its start.
    fn blocks(&mut self, mut req: BlocksReq) {
        req.have_positions.clear();
        self.req = req;
        self.generation += 1;
    }

    /// `send_credits += num_messages`, a plain `u32` addition that wraps.
    fn ack(&mut self, n: u32) {
        self.req.max_in_flight = self.req.max_in_flight.wrapping_add(n);
    }

    /// The job Spring's send rule allows now, if any.
    fn sendable(&self, last: u32) -> Option<Job> {
        let r = &self.req;
        (r.max_in_flight > 0 && r.start <= last && r.start < r.end).then(|| Job {
            generation: self.generation,
            n: r.start,
            v1: r.v1,
            flags: Flags::of(r),
        })
    }

    /// One writer iteration: the status requests to answer, and the block to send (its credit
    /// spent and the cursor advanced).
    fn take(&mut self, last: u32) -> (Vec<bool>, Option<Job>) {
        let statuses = self.status_q.drain(..).collect();
        let job = self.sendable(last);
        if job.is_some() {
            self.req.start += 1;
            self.req.max_in_flight -= 1;
        }
        (statuses, job)
    }

    /// The jobs the send rule allows from the cursor if nothing changes, to build ahead: as
    /// many as the credit covers, up to the end of the request or `L*`.
    fn window(&self, last: u32) -> Window {
        let Some(first) = self.sendable(last) else {
            return Window::default();
        };
        let r = &self.req;
        let left = r.end.min(last.saturating_add(1)) - r.start;
        Window {
            first: Some(first),
            count: left.min(r.max_in_flight),
        }
    }
}

/// Counters for the line logged when a session closes.
#[derive(Default)]
struct Stats {
    status_requests: AtomicU64,
    block_requests: AtomicU64,
    acks: AtomicU64,
    results: AtomicU64,
    bytes: AtomicU64,
}

fn bump(counter: &AtomicU64, by: u64) {
    counter.fetch_add(by, Ordering::Relaxed);
}

/// The state the reader and the writer share.
#[derive(Default)]
struct Shared {
    state: Mutex<SpringState>,
    wake: Notify,
    stats: Stats,
}

impl Shared {
    /// No critical section can leave the state half-updated, so a poisoned lock is still valid.
    fn lock(&self) -> MutexGuard<'_, SpringState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Why a session ended.
#[derive(Debug)]
enum End {
    /// The client sent Close (and was answered).
    ClientClosed,
    /// The connection ended without a Close.
    Disconnected,
    /// The WebSocket read failed: a protocol violation, an oversized request.
    ReadFailed(WsError),
    /// A request that does not decode: dropped without Close, as Spring does.
    BadRequest(anyhow::Error),
    /// More than [`MAX_QUEUED_STATUS`] status requests queued.
    StatusFlood,
    /// Writing to the client failed.
    SendFailed(WsError),
    /// A log read or integrity check failed (logged as a warning where it happened).
    DataError,
    /// The server is shutting down; Close 1001 was sent.
    Shutdown,
}

impl fmt::Display for End {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientClosed => f.write_str("closed by the client"),
            Self::Disconnected => f.write_str("disconnected without a close"),
            Self::ReadFailed(e) => write!(f, "read failed: {e}"),
            Self::BadRequest(e) => write!(f, "undecodable request, dropped: {e:#}"),
            Self::StatusFlood => write!(
                f,
                "more than {MAX_QUEUED_STATUS} status requests queued, dropped"
            ),
            Self::SendFailed(e) => write!(f, "send failed: {e}"),
            Self::DataError => f.write_str("log read error, dropped"),
            Self::Shutdown => f.write_str("server shutdown"),
        }
    }
}

/// Serve one accepted TCP connection until it ends. Without a `permit` the server is at
/// `--max-sessions`, and the upgrade is refused with HTTP 503.
pub(crate) async fn run(
    tcp: TcpStream,
    peer: SocketAddr,
    permit: Option<OwnedSemaphorePermit>,
    ctx: Arc<Ctx>,
    mut stop: watch::Receiver<bool>,
) {
    // Spring disables Nagle too: a result's frames go out as soon as they are written.
    let _ = tcp.set_nodelay(true);
    let full = permit.is_none();
    let server = ctx.server.clone();
    // tungstenite fixes the callback's type, error response included.
    #[allow(clippy::result_large_err)]
    let callback = move |_: &HttpRequest, mut response: Response| {
        if full {
            return Err(busy(server));
        }
        response.headers_mut().insert(header::SERVER, server);
        Ok(response)
    };
    let upgrade = accept_hdr_async_with_config(tcp, callback, Some(ctx.ws));
    let ws = match tokio::time::timeout(HANDSHAKE_TIMEOUT, upgrade).await {
        Ok(Ok(ws)) => ws,
        Ok(Err(WsError::Http(_))) if full => {
            ctx.metrics.rejected();
            info!(%peer, "upgrade refused with 503: at --max-sessions");
            return;
        }
        Ok(Err(e)) => {
            debug!(%peer, "upgrade failed: {e}");
            return;
        }
        Err(_) => {
            debug!(%peer, "upgrade timed out");
            return;
        }
    };
    let _permit = permit;
    let _open = ctx.metrics.session_opened();
    let started = Instant::now();
    debug!(%peer, "session open");

    let (mut tx, mut rx) = ws.split();
    if let Err(e) = tx.send(Message::Text(ctx.store().abi_message())).await {
        debug!(%peer, "sending the ABI failed: {e}");
        return;
    }
    let shared = Shared::default();
    let end = {
        let reader = read_loop(&mut rx, &shared, &ctx, peer);
        let writer = write_loop(&mut tx, &shared, &ctx, &mut stop, peer);
        tokio::pin!(reader, writer);
        tokio::select! {
            end = &mut reader => end,
            end = &mut writer => {
                if matches!(end, End::Shutdown) {
                    // Let the client's Close reply arrive before the connection goes.
                    let _ = tokio::time::timeout(CLOSE_WAIT, &mut reader).await;
                }
                end
            }
        }
    };
    let s = &shared.stats;
    info!(
        %peer,
        status_requests = s.status_requests.load(Ordering::Relaxed),
        block_requests = s.block_requests.load(Ordering::Relaxed),
        acks = s.acks.load(Ordering::Relaxed),
        results = s.results.load(Ordering::Relaxed),
        bytes = s.bytes.load(Ordering::Relaxed),
        secs = format_args!("{:.3}", started.elapsed().as_secs_f64()),
        "session closed: {end}"
    );
    // Dropping both halves closes the TCP connection. Unless a Close was exchanged above,
    // the client sees the connection end without one, as from Spring on an error.
}

/// The response refusing an upgrade over `--max-sessions`.
fn busy(server: HeaderValue) -> ErrorResponse {
    const BODY: &str = "at the session limit; retry later\n";
    let mut response = ErrorResponse::new(Some(BODY.to_owned()));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let headers = response.headers_mut();
    headers.insert(header::SERVER, server);
    headers.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("close"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(BODY.len()));
    response
}

/// Read and apply client messages in order until the connection ends or must be dropped.
async fn read_loop<S>(rx: &mut S, shared: &Shared, ctx: &Ctx, peer: SocketAddr) -> End
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    let mut closing = false;
    loop {
        let msg = match rx.next().await {
            Some(Ok(msg)) => msg,
            Some(Err(WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake))) => {
                return End::Disconnected;
            }
            Some(Err(e)) => return End::ReadFailed(e),
            None if closing => return End::ClientClosed,
            None => return End::Disconnected,
        };
        let decoded = match &msg {
            Message::Binary(b) => protocol::decode_request(b),
            Message::Text(t) => protocol::decode_request(t.as_bytes()),
            // Reading on sends tungstenite's Close reply, then ends the stream.
            Message::Close(_) => {
                closing = true;
                continue;
            }
            _ => continue,
        };
        let request = match decoded {
            Ok(request) => request,
            Err(e) => return End::BadRequest(e),
        };
        let stats = &shared.stats;
        match request {
            Request::Status { v1 } => {
                bump(&stats.status_requests, 1);
                ctx.metrics.status_request(v1);
                if !shared.lock().status(v1) {
                    return End::StatusFlood;
                }
            }
            Request::Blocks(mut req) => {
                bump(&stats.block_requests, 1);
                ctx.metrics.blocks_request(req.v1);
                if let Err(e) = apply_positions(ctx.store(), &ctx.fetch.io, &mut req).await {
                    ctx.metrics.read_error(&e);
                    warn!(
                        %peer,
                        start = req.start,
                        "reading the ids of have_positions failed, dropping the connection: {e:#}"
                    );
                    return End::DataError;
                }
                shared.lock().blocks(req);
            }
            Request::Ack(n) => {
                bump(&stats.acks, 1);
                shared.lock().ack(n);
            }
        }
        shared.wake.notify_one();
    }
}

/// Spring's `have_positions` rule: every position below the (possibly already lowered) start
/// whose id this server does not know, or knows differently, moves the start back to it. The
/// positions are then dropped. Ids are read on the io threads.
async fn apply_positions(store: &Arc<Store>, io: &Io, req: &mut BlocksReq) -> anyhow::Result<()> {
    let positions = std::mem::take(&mut req.have_positions);
    let start = req.start;
    if positions.iter().all(|p| p.num >= start) {
        return Ok(());
    }
    let store = Arc::clone(store);
    req.start = io
        .run(move || -> anyhow::Result<u32> {
            let mut start = start;
            for p in &positions {
                if p.num < start && store.id(p.num)? != Some(p.id) {
                    start = p.num;
                }
            }
            Ok(start)
        })
        .await
        .context("have_positions task")??;
    Ok(())
}

/// Answer status requests and send blocks as the state allows, until the client is gone, a
/// read fails, or the server stops.
async fn write_loop<S>(
    tx: &mut S,
    shared: &Shared,
    ctx: &Ctx,
    stop: &mut watch::Receiver<bool>,
    peer: SocketAddr,
) -> End
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let store = ctx.store();
    let last = store.last.num;
    let stats = &shared.stats;
    let mut prefetch = Prefetcher::new(ctx.fetch.clone());
    loop {
        if *stop.borrow() {
            return close_away(tx).await;
        }
        let (statuses, job, window) = {
            let mut state = shared.lock();
            let (statuses, job) = state.take(last);
            (statuses, job, state.window(last))
        };
        if statuses.is_empty() && job.is_none() {
            prefetch.clear();
            tokio::select! {
                () = shared.wake.notified() => continue,
                // A change is the shutdown signal; an error means the server is gone.
                _ = stop.changed() => return close_away(tx).await,
            }
        }

        if !statuses.is_empty() {
            for v1 in statuses {
                let status = store.status_bytes(v1);
                bump(&stats.bytes, status.len() as u64);
                ctx.metrics.status_sent(status.len() as u64);
                if let Err(e) = tx.feed(Message::Binary(status)).await {
                    return End::SendFailed(e);
                }
            }
            // Answered promptly, even while the next block is still being read.
            if let Err(e) = tx.flush().await {
                return End::SendFailed(e);
            }
        }

        let Some(job) = job else { continue };
        let package = match prefetch.take(job).await {
            Ok(package) => package,
            Err(e) => {
                // Nothing of this message was sent.
                ctx.metrics.read_error(&e);
                warn!(%peer, block = job.n, "dropping the connection: {e:#}");
                return End::DataError;
            }
        };
        // The next blocks are read while this one is written.
        prefetch.schedule(window);
        match frames::send_result(tx, &ctx.fetch.io, package).await {
            Ok(bytes) => {
                bump(&stats.results, 1);
                bump(&stats.bytes, bytes);
                ctx.metrics.result_sent(bytes);
            }
            Err(SendError::Ws(e)) => return End::SendFailed(e),
            Err(SendError::Read(e)) => {
                ctx.metrics.read_error(&e);
                warn!(
                    %peer,
                    block = job.n,
                    "dropping the connection in the middle of a result: {e:#}"
                );
                return End::DataError;
            }
        }
    }
}

/// Close 1001 (going away), between two messages.
async fn close_away<S>(tx: &mut S) -> End
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let frame = CloseFrame {
        code: CloseCode::Away,
        reason: "server shutting down".into(),
    };
    let _ = tx.send(Message::Close(Some(frame))).await;
    End::Shutdown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(start: u32, end: u32, credit: u32) -> BlocksReq {
        BlocksReq {
            start,
            end,
            max_in_flight: credit,
            fetch_traces: true,
            ..BlocksReq::default()
        }
    }

    /// Every job a state hands out until it stops, with the block numbers only.
    fn drain(s: &mut SpringState, last: u32) -> Vec<u32> {
        let mut sent = Vec::new();
        while let (_, Some(job)) = s.take(last) {
            sent.push(job.n);
        }
        sent
    }

    #[test]
    fn nothing_is_sent_before_the_first_request() {
        let mut s = SpringState::default();
        s.ack(10);
        assert_eq!(s.take(100), (vec![], None));
    }

    #[test]
    fn the_send_rule_is_springs() {
        // Credit bounds the run.
        let mut s = SpringState::default();
        s.blocks(request(10, 100, 3));
        assert_eq!(drain(&mut s, 1000), [10, 11, 12]);
        // The end is exclusive.
        s.blocks(request(10, 13, 100));
        assert_eq!(drain(&mut s, 1000), [10, 11, 12]);
        // Nothing past the last block, `L*` included.
        s.blocks(request(10, 100, 100));
        assert_eq!(drain(&mut s, 12), [10, 11, 12]);
        s.blocks(request(13, 100, 100));
        assert_eq!(drain(&mut s, 12), Vec::<u32>::new());
        // An empty range sends nothing.
        s.blocks(request(10, 10, 100));
        assert_eq!(drain(&mut s, 1000), Vec::<u32>::new());
    }

    #[test]
    fn a_request_replaces_credit_and_cursor() {
        let mut s = SpringState::default();
        s.blocks(request(10, 100, 5));
        assert_eq!(drain(&mut s, 1000).len(), 5);
        s.ack(2);
        assert_eq!(drain(&mut s, 1000), [15, 16]);
        // Unused credit does not carry over.
        s.blocks(request(50, 100, 1000));
        s.blocks(request(20, 100, 1));
        assert_eq!(drain(&mut s, 1000), [20]);
    }

    #[test]
    fn acks_wrap_at_u32() {
        let mut s = SpringState::default();
        s.blocks(request(10, 100, u32::MAX - 1));
        s.ack(3);
        assert_eq!(s.req.max_in_flight, 1);
        assert_eq!(drain(&mut s, 1000), [10]);
        s.ack(u32::MAX);
        s.ack(1);
        assert_eq!(s.req.max_in_flight, 0);
        assert_eq!(drain(&mut s, 1000), Vec::<u32>::new());
    }

    #[test]
    fn jobs_carry_the_request_generation_version_and_flags() {
        let mut s = SpringState::default();
        let mut v1 = request(10, 100, 2);
        v1.v1 = true;
        v1.fetch_finality_data = true;
        s.blocks(v1);
        let (_, job) = s.take(1000);
        let job = job.unwrap();
        assert_eq!((job.generation, job.n, job.v1), (1, 10, true));
        assert!(job.flags.traces && job.flags.finality && !job.flags.block);
        // The job after it, to build ahead, is of the same request.
        assert_eq!(s.window(1000).first, Some(Job { n: 11, ..job }));
        let (_, next) = s.take(1000);
        assert_eq!(next.map(|j| j.n), Some(11));
        // No credit left for a block after it.
        assert_eq!(s.window(1000).first, None);

        // A v0 request never asks for finality data.
        let mut v0 = request(10, 100, 1);
        v0.fetch_finality_data = true;
        s.blocks(v0);
        let (_, job) = s.take(1000);
        let job = job.unwrap();
        assert_eq!(job.generation, 2);
        assert!(!job.v1 && !job.flags.finality);
    }

    #[test]
    fn the_window_is_what_the_credit_and_the_range_still_allow() {
        let window = |s: &SpringState, last: u32| {
            let w = s.window(last);
            (w.first.map(|j| (j.generation, j.n)), w.count)
        };
        let mut s = SpringState::default();
        assert_eq!(s.window(1000), Window::default());
        // After block 10 is taken: the credit left.
        s.blocks(request(10, 100, 5));
        s.take(1000);
        assert_eq!(window(&s, 1000), (Some((1, 11)), 4));
        // The end of the request.
        s.blocks(request(10, 13, 100));
        s.take(1000);
        assert_eq!(window(&s, 1000), (Some((2, 11)), 2));
        // `L*`.
        s.blocks(request(10, 100, 100));
        s.take(12);
        assert_eq!(window(&s, 12), (Some((3, 11)), 2));
        // No credit, nothing left, or nothing yet.
        s.blocks(request(10, 100, 1));
        s.take(1000);
        assert_eq!(window(&s, 1000), (None, 0));
        s.ack(3);
        assert_eq!(window(&s, 1000), (Some((4, 11)), 3));
        s.blocks(request(20, 20, 100));
        assert_eq!(window(&s, 1000), (None, 0));
    }

    #[test]
    fn status_requests_are_answered_without_credit_and_bounded() {
        let mut s = SpringState::default();
        assert!(s.status(false) && s.status(true));
        assert_eq!(s.take(1000), (vec![false, true], None));
        for _ in 0..MAX_QUEUED_STATUS {
            assert!(s.status(false));
        }
        assert!(!s.status(true));
    }
}
