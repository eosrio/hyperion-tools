//! One chain API request, from its head to its answer:
//!
//!  1. The body is read: at most `--api-max-body-bytes` (a larger one, declared or streamed,
//!     gets `413` and the connection is closed), within the request timeout (otherwise the
//!     connection is closed unanswered). Past its first [`UNRESERVED_BODY`] bytes it holds
//!     room reserved from the memory budget, taken as it arrives; a refusal is `503 Busy` and
//!     the connection is closed.
//!  2. The request-target must be origin-form and hold no `..`, Beast's check: anything else
//!     (`OPTIONS *`, an absolute or authority form) is `400 Illegal request-target`.
//!  3. `OPTIONS`, on any path, is `200 {}`.
//!  4. The handler is the one whose path is the whole target, query string included:
//!     a block endpoint, `describe` or `get_supported_apis`; anything else is nodeos's 404.
//!
//! `Host` is not checked: a request with any `Host`, an empty one or none is answered. nodeos
//! refuses one without a `Host` (`400 Disallowed HTTP HOST header`, even with
//! `http-validate-host = false`), which would turn away an HTTP/1.0 health check that sends none.
//!
//! Every answer this module writes carries nodeos's headers in nodeos's order ([`Headers`]).
//! hyper answers two kinds of target before this module sees them, unlike Beast: one holding a
//! byte that is not a URI character (such as `<`, `>`, or bytes that are not UTF-8) gets
//! hyper's bare `400 Bad Request`, and a `#fragment` is dropped, so `/v1/chain/get_raw_block#x`
//! is `get_raw_block` here and nodeos's 404 there. HTTP clients send neither.
//!
//! A block request is then parsed as nodeos parses it (`hyperion_ship::chainapi::parse`); a
//! block number outside the served block-log range is `Unknown Block` at once. Otherwise the
//! request takes a slot ([`admit`]) and runs one [`Job`] on an io thread: it reserves what it
//! will hold from the memory budget, reads the block, checks it is the block asked for, walks
//! it and renders the body. Once the block is dropped the reservation shrinks to the body,
//! which keeps it until hyper has written it out: a client that does not read its answer holds
//! budget, never memory beyond it, and only until the connection's socket gives up on it
//! (`super::Socket`).

use std::any::Any;
use std::fmt;
use std::net::SocketAddr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as _, Bytes, Incoming};
use hyper::header::{CONNECTION, CONTENT_TYPE, HeaderValue, SERVER};
use hyper::http::request::Parts;
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyperion_ship::blocklog::{BlockLog, block_header};
use hyperion_ship::chainapi::{self, ApiError, Endpoint, parse_signed_block, render};
use hyperion_ship::protocol::MAX_BLOCK_BYTES;
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, warn};

use super::Ctx;
use super::describe::{self, DESCRIBE, SUPPORTED_APIS};
use crate::SERVER_HEADER;
use crate::fetch::{MemoryBudget, Reservation};
use crate::metrics::{ApiFailure, ApiRejection, ApiRoute, Metrics};

/// What a body may take besides what its block's size calls for: the header fields, the
/// signature, the list brackets.
const SLACK: u64 = 64 << 10;
/// No block renders to more than this many times its packed size: the densest element, a
/// packed receipt with no signatures and empty data, takes 11 bytes and renders to under 170.
const MAX_EXPANSION: u64 = 16;
/// What of a request body is held without a reservation: every real request, a block number
/// or id in a small JSON object, is far smaller, so it is answered even when the budget is
/// full; a larger body reserves the rest.
const UNRESERVED_BODY: usize = 16 << 10;

/// The connection is closed without an answer (a body that did not arrive in time, or could
/// not be read).
#[derive(Debug)]
pub(super) struct Dropped(&'static str);

impl fmt::Display for Dropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Dropped {}

/// The answer to a request.
struct Reply {
    status: u16,
    body: Vec<u8>,
    /// Close the connection after it.
    close: bool,
    /// The memory budget the body holds, given back once hyper has written it.
    held: Vec<Reservation>,
}

impl Reply {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            body,
            close: false,
            held: Vec::new(),
        }
    }

    fn error(e: &ApiError) -> Self {
        Self {
            status: e.status(),
            body: e.body(),
            close: matches!(e, ApiError::TooLarge(_)),
            held: Vec::new(),
        }
    }
}

/// A body and the budget it holds: the owner of the bytes hyper writes, dropped (and the
/// budget given back) when hyper has written the last of them.
struct Held {
    body: Vec<u8>,
    _reserved: Vec<Reservation>,
}

impl AsRef<[u8]> for Held {
    fn as_ref(&self) -> &[u8] {
        &self.body
    }
}

/// `body` as hyper takes it. Budget it holds stays held until the last of its bytes is
/// dropped, which hyper does once it has written them.
fn into_bytes(body: Vec<u8>, held: Vec<Reservation>) -> Bytes {
    match held.is_empty() {
        true => Bytes::from(body),
        false => Bytes::from_owner(Held {
            body,
            _reserved: held,
        }),
    }
}

/// Answer one request (hyper's service).
pub(super) async fn handle(
    req: Request<Incoming>,
    ctx: Arc<Ctx>,
    peer: SocketAddr,
) -> Result<Response<Full<Bytes>>, Dropped> {
    let started = Instant::now();
    let (head, body) = req.into_parts();
    let target = origin_form(&head.uri);
    let route = match target {
        Some(t) if head.method != Method::OPTIONS => route(t),
        _ => ApiRoute::Other,
    };
    let headers = Headers::new(&head, route);
    let (cap, timeout) = (ctx.cfg.max_body_bytes, ctx.cfg.request_timeout);
    let body = match read_body(body, cap, timeout, &ctx.budget).await {
        Ok(body) => body,
        Err(Unread::TooLarge) => {
            ctx.metrics.api_rejected(ApiRejection::BodyTooLarge);
            let reply = Reply::error(&ApiError::TooLarge(ctx.cfg.max_body_bytes));
            return Ok(respond(&ctx, peer, route, headers, None, reply, started));
        }
        Err(Unread::Refused(used)) => {
            ctx.metrics.api_rejected(ApiRejection::Memory);
            debug!(%peer, "chain API: the memory budget cannot hold the request body");
            // The rest of the body is not read, so the connection cannot carry another request.
            let reply = Reply {
                close: true,
                ..Reply::error(&ApiError::too_many_bytes(
                    usize::try_from(used).unwrap_or(usize::MAX),
                ))
            };
            return Ok(respond(&ctx, peer, route, headers, None, reply, started));
        }
        Err(Unread::Late) => {
            ctx.metrics.api_rejected(ApiRejection::Timeout);
            debug!(
                %peer,
                "chain API request body not complete within {:?}; closing the connection",
                ctx.cfg.request_timeout
            );
            return Err(Dropped("the request body did not arrive in time"));
        }
        Err(Unread::Failed(e)) => {
            debug!(%peer, "reading a chain API request body failed: {e}");
            return Err(Dropped("the request body could not be read"));
        }
    };
    let (reply, block) = match target {
        None => (Reply::error(&ApiError::IllegalTarget), None),
        Some(_) if head.method == Method::OPTIONS => (Reply::ok(b"{}".to_vec()), None),
        Some(target) => answer(&ctx, peer, target, body).await,
    };
    Ok(respond(&ctx, peer, route, headers, block, reply, started))
}

/// The whole response refusing a connection over `--api-max-connections`, sent before its
/// request is read: `503 Busy` with the headers [`respond`] gives an answer no handler writes,
/// and `Connection: close`.
pub(super) fn refusal(max_connections: usize) -> Vec<u8> {
    let body = ApiError::Busy(format!("Too many connections: {max_connections}")).body();
    let mut out = format!(
        "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\n\
         Connection: close\r\nServer: {SERVER_HEADER}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(&body);
    out
}

/// The raw request-target, when it is one nodeos serves: origin-form (no scheme, no
/// authority), starting with `/` and holding no `..`.
fn origin_form(uri: &Uri) -> Option<&str> {
    if uri.scheme().is_some() || uri.authority().is_some() {
        return None;
    }
    let target = uri.path_and_query()?.as_str();
    (target.starts_with('/') && !target.contains("..")).then_some(target)
}

/// The headers of an answer, laid out as nodeos lays them out (Spring `beast_http_session.hpp`
/// `handle_request`): it sets `Content-Type`, then the connection header the request calls for,
/// then `Server`; a handler then sets `Content-Type` once more, which moves it after `Server`.
/// `Content-Length` comes last, from hyper. So an answer no handler writes (an illegal target,
/// `OPTIONS`, `get_supported_apis`, the 404) starts with `Content-Type`, and a handler's (the
/// block endpoints, `describe`) with the connection header or `Server`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Headers {
    /// The request's HTTP version, which the answer takes (as Beast's `res.version`).
    version: Version,
    /// The answer comes from a handler.
    handler: bool,
    /// Beast's `keep_alive(req.keep_alive())`: `close` when an HTTP/1.1 request asks to close,
    /// `keep-alive` when an HTTP/1.0 request asks to be kept alive, nothing otherwise.
    connection: Option<&'static str>,
}

impl Headers {
    fn new(head: &Parts, route: ApiRoute) -> Self {
        let says = |value: &HeaderValue, token: &str| {
            value
                .to_str()
                .is_ok_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
        };
        let lines = head.headers.get_all(CONNECTION);
        let connection = match head.version {
            // Beast reads the first `Connection` line alone. hyper keeps an HTTP/1.0
            // connection alive when the answer says `keep-alive` and no line of the request says
            // `close`; one whose first line keeps alive and a later one closes gets nodeos's
            // answer, then the connection is closed.
            Version::HTTP_10 => lines
                .iter()
                .next()
                .is_some_and(|v| says(v, "keep-alive"))
                .then_some("keep-alive"),
            // hyper closes an HTTP/1.1 connection once any line says `close` (nodeos, reading
            // the first alone, may keep it) and would then add its own `Connection: close`
            // after `Server`: the answer says it where nodeos would.
            _ => lines.iter().any(|v| says(v, "close")).then_some("close"),
        };
        let handler = matches!(
            route,
            ApiRoute::GetBlockInfo
                | ApiRoute::GetBlockHeader
                | ApiRoute::GetRawBlock
                | ApiRoute::Describe
        );
        Self {
            version: head.version,
            handler,
            connection,
        }
    }

    /// Put the headers on `response`, which closes the connection after it when `close`.
    fn apply(self, response: &mut Response<Full<Bytes>>, close: bool) {
        *response.version_mut() = self.version;
        let connection = if close {
            Some("close")
        } else {
            self.connection
        };
        let json = HeaderValue::from_static("application/json");
        // A header map keeps the order names are first inserted in, and hyper writes it so.
        let headers = response.headers_mut();
        if !self.handler {
            headers.insert(CONTENT_TYPE, json.clone());
        }
        if let Some(connection) = connection {
            headers.insert(CONNECTION, HeaderValue::from_static(connection));
        }
        headers.insert(SERVER, HeaderValue::from_static(SERVER_HEADER));
        if self.handler {
            headers.insert(CONTENT_TYPE, json);
        }
    }
}

/// The endpoint a legal target is counted under.
fn route(target: &str) -> ApiRoute {
    match target {
        DESCRIBE => ApiRoute::Describe,
        SUPPORTED_APIS => ApiRoute::GetSupportedApis,
        "/v1/chain/get_block" => ApiRoute::GetBlock,
        t => Endpoint::from_target(t).map_or(ApiRoute::Other, ApiRoute::of),
    }
}

/// Why a body was not read.
enum Unread {
    /// Over the cap, declared or streamed.
    TooLarge,
    /// Not complete in time.
    Late,
    /// The memory budget could not hold it; the bytes reserved then.
    Refused(u64),
    /// The connection failed while it was read.
    Failed(hyper::Error),
}

/// A request body, and the budget its room past [`UNRESERVED_BODY`] holds.
#[derive(Default)]
struct Body {
    bytes: Vec<u8>,
    reserved: Vec<Reservation>,
}

impl Body {
    /// Make room for `len` bytes, at most `limit`: the room grows to the next power of two, and
    /// what of it lies past [`UNRESERVED_BODY`] is reserved before it is allocated.
    fn make_room(
        &mut self,
        len: usize,
        limit: usize,
        budget: &Arc<MemoryBudget>,
    ) -> Result<(), Unread> {
        if len <= self.bytes.capacity() {
            return Ok(());
        }
        let room = len
            .checked_next_power_of_two()
            .unwrap_or(len)
            .min(limit)
            .max(len);
        let held: u64 = self.reserved.iter().map(Reservation::bytes).sum();
        let needed = room.saturating_sub(UNRESERVED_BODY) as u64;
        if needed > held {
            let more = budget
                .try_reserve(needed - held)
                .ok_or_else(|| Unread::Refused(budget.used()))?;
            self.reserved.push(more);
        }
        self.bytes.reserve_exact(room - self.bytes.len());
        Ok(())
    }
}

/// The whole body, if it is at most `cap` bytes, arrives within `timeout`, and `budget` holds
/// what of it lies past [`UNRESERVED_BODY`].
async fn read_body(
    body: Incoming,
    cap: usize,
    timeout: Duration,
    budget: &Arc<MemoryBudget>,
) -> Result<Body, Unread> {
    let hint = body.size_hint();
    if hint.lower() > cap as u64 {
        return Err(Unread::TooLarge);
    }
    // No room past a declared length.
    let limit = hint.exact().map_or(cap, |n| to_usize(n).min(cap));
    let read = async {
        let mut body = body;
        let mut out = Body::default();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(Unread::Failed)?;
            if let Ok(data) = frame.into_data() {
                let len = out.bytes.len() + data.len();
                if len > cap {
                    return Err(Unread::TooLarge);
                }
                out.make_room(len, limit, budget)?;
                out.bytes.extend_from_slice(&data);
            }
        }
        Ok(out)
    };
    tokio::time::timeout(timeout, read)
        .await
        .unwrap_or(Err(Unread::Late))
}

/// The answer to a request with a legal target other than `OPTIONS`, and the block it named.
async fn answer(ctx: &Ctx, peer: SocketAddr, target: &str, body: Body) -> (Reply, Option<u32>) {
    if let Some(endpoint) = Endpoint::from_target(target) {
        return block(ctx, peer, endpoint, body).await;
    }
    let reply = match target {
        DESCRIBE => {
            let in_flight = ctx.metrics.api_in_flight();
            Reply::ok(describe::describe(
                &ctx.store,
                ctx.cfg.max_in_flight,
                in_flight,
            ))
        }
        SUPPORTED_APIS => Reply::ok(describe::supported()),
        _ => Reply::error(&ApiError::NotFound),
    };
    (reply, None)
}

/// A block endpoint's answer.
async fn block(
    ctx: &Ctx,
    peer: SocketAddr,
    endpoint: Endpoint,
    body: Body,
) -> (Reply, Option<u32>) {
    let parsed = chainapi::parse(endpoint, &body.bytes);
    // The request is all that is needed of the body: its budget goes back before the block's
    // is taken.
    drop(body);
    let req = match parsed {
        Ok(req) => req,
        Err(e) => return (Reply::error(&e), None),
    };
    let n = req.block.num();
    let unknown = req.unknown_block();
    let Some(log) = ctx.store.block_log(n) else {
        return (Reply::error(&unknown), Some(n));
    };
    let slot = match admit(ctx).await {
        Ok(slot) => slot,
        Err(busy) => return (Reply::error(&busy), Some(n)),
    };
    let job = Job {
        endpoint,
        req,
        log: Arc::clone(log),
        budget: Arc::clone(&ctx.budget),
        delay: ctx.cfg.read_delay,
    };
    // The slot is held until the job ends, even if the client leaves before.
    let done = ctx
        .io
        .run(move || {
            let _slot = slot;
            panic::catch_unwind(AssertUnwindSafe(|| job.run()))
        })
        .await;
    let metrics = &ctx.metrics;
    let what = endpoint.name();
    let reply = match done {
        Ok(Ok(Ok(Rendered { body, reserved }))) => Reply {
            held: reserved,
            ..Reply::ok(body)
        },
        Ok(Ok(Err(Failed::Unknown))) => Reply::error(&unknown),
        Ok(Ok(Err(Failed::Refused(used)))) => {
            metrics.api_rejected(ApiRejection::Memory);
            debug!(%peer, endpoint = what, block = n, "chain API: the memory budget is full");
            Reply::error(&ApiError::too_many_bytes(
                usize::try_from(used).unwrap_or(usize::MAX),
            ))
        }
        Ok(Ok(Err(Failed::Read(e)))) => {
            metrics.api_failure(ApiFailure::Read);
            warn!(%peer, endpoint = what, block = n, "chain API: reading the block failed: {e:#}");
            Reply::error(&ApiError::read_failed())
        }
        Ok(Ok(Err(Failed::Integrity(e)))) => {
            metrics.api_failure(ApiFailure::Integrity);
            warn!(%peer, endpoint = what, block = n, "chain API: the block failed validation: {e:#}");
            Reply::error(&ApiError::integrity_failed())
        }
        Ok(Err(panicked)) => {
            metrics.api_failure(ApiFailure::Panic);
            warn!(
                %peer,
                endpoint = what,
                block = n,
                "chain API: the request panicked: {}",
                panic_message(&*panicked)
            );
            Reply::error(&ApiError::internal())
        }
        Err(e) => {
            warn!(%peer, endpoint = what, block = n, "chain API: the request's job failed: {e:#}");
            Reply::error(&ApiError::internal())
        }
    };
    (reply, Some(n))
}

/// A request slot, given back (and the in-flight gauge lowered) when dropped.
struct Slot {
    _permit: OwnedSemaphorePermit,
    metrics: Arc<Metrics>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.metrics.api_finished();
    }
}

/// Take a request slot, waiting at most `--api-queue-ms` for one.
async fn admit(ctx: &Ctx) -> Result<Slot, ApiError> {
    let wait = Arc::clone(&ctx.slots).acquire_owned();
    match tokio::time::timeout(ctx.cfg.queue, wait).await {
        Ok(Ok(permit)) => {
            ctx.metrics.api_admitted();
            Ok(Slot {
                _permit: permit,
                metrics: Arc::clone(&ctx.metrics),
            })
        }
        // The slots are never closed.
        Ok(Err(_)) => Err(ApiError::internal()),
        Err(_) => {
            ctx.metrics.api_rejected(ApiRejection::InFlight);
            let in_flight = ctx.metrics.api_in_flight();
            Err(ApiError::too_many_requests(
                usize::try_from(in_flight).unwrap_or(usize::MAX),
            ))
        }
    }
}

/// A block request's work, on an io thread.
struct Job {
    endpoint: Endpoint,
    req: chainapi::Request,
    log: Arc<BlockLog>,
    budget: Arc<MemoryBudget>,
    delay: Duration,
}

/// A rendered body and the budget it was built under.
struct Rendered {
    body: Vec<u8>,
    reserved: Vec<Reservation>,
}

/// Why a job rendered nothing.
enum Failed {
    /// The block at that number has another id.
    Unknown,
    /// The memory budget could not hold the request; the bytes reserved then.
    Refused(u64),
    /// The block could not be read.
    Read(anyhow::Error),
    /// The block is too large, does not decode, or does not render within any bound.
    Integrity(anyhow::Error),
}

impl Job {
    fn run(self) -> Result<Rendered, Failed> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        let n = self.req.block.num();
        let ext = self.req.include_extensions;
        let (_, len) = self.log.bounds(n).map_err(Failed::Read)?;
        if len > MAX_BLOCK_BYTES as u64 {
            return Err(Failed::Integrity(anyhow!(
                "{}: block {n} is {len} bytes, more than the {MAX_BLOCK_BYTES} a block takes",
                self.log.log_path().display()
            )));
        }
        let first = first_reservation(self.endpoint, ext, len);
        let mut reserved = vec![self.reserve(first)?];
        let mut block = Vec::new();
        self.log.body(n, &mut block).map_err(Failed::Read)?;
        let header = block_header(&block).map_err(Failed::Integrity)?;
        if header.number != n {
            return Err(Failed::Integrity(anyhow!(
                "{}: the block stored as {n} is block {}",
                self.log.log_path().display(),
                header.number
            )));
        }
        if !self.req.block.matches(&header.id) {
            return Err(Failed::Unknown);
        }
        let b = parse_signed_block(&block).map_err(Failed::Integrity)?;
        let mut out = Vec::new();
        let limit = to_usize(first - len);
        out.reserve_exact(limit);
        if render(self.endpoint, &b, ext, limit, &mut out).is_err() {
            // Rare: a block of many id-only receipts renders to almost four times its size.
            // Once more, with room for any block.
            let full = len.saturating_mul(MAX_EXPANSION + 1).saturating_add(SLACK);
            reserved.push(self.reserve(full - first)?);
            let limit = to_usize(full - len);
            out.reserve_exact(limit);
            render(self.endpoint, &b, ext, limit, &mut out).map_err(Failed::Integrity)?;
        }
        drop(block);
        out.shrink_to_fit();
        // From here on only the body is held, until it is written.
        let mut keep = out.capacity() as u64;
        for r in &mut reserved {
            let kept = keep.min(r.bytes());
            r.shrink_to(kept);
            keep -= kept;
        }
        Ok(Rendered {
            body: out,
            reserved,
        })
    }

    fn reserve(&self, bytes: u64) -> Result<Reservation, Failed> {
        self.budget
            .try_reserve(bytes)
            .ok_or_else(|| Failed::Refused(self.budget.used()))
    }
}

/// What a request for a block of `len` bytes reserves first: the block, and room for the body.
/// `get_block_info` and `get_block_header` render the header only; `get_raw_block` and the
/// header with the block extensions render the whole block, hex doubling its byte fields.
fn first_reservation(endpoint: Endpoint, include_extensions: bool, len: u64) -> u64 {
    let body = match (endpoint, include_extensions) {
        (Endpoint::GetRawBlock, _) | (Endpoint::GetBlockHeader, true) => len.saturating_mul(3),
        _ => 0,
    };
    len.saturating_add(body).saturating_add(SLACK)
}

/// Every size here is under the 20 MiB block cap times [`MAX_EXPANSION`].
fn to_usize(v: u64) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("a non-text panic")
}

/// The response for `reply`, counted and logged. Its body keeps its reservation until hyper
/// has written it.
fn respond(
    ctx: &Ctx,
    peer: SocketAddr,
    route: ApiRoute,
    headers: Headers,
    block: Option<u32>,
    reply: Reply,
    started: Instant,
) -> Response<Full<Bytes>> {
    let Reply {
        status,
        body,
        close,
        held,
    } = reply;
    let bytes = body.len() as u64;
    let mut response = Response::new(Full::new(into_bytes(body, held)));
    *response.status_mut() =
        StatusCode::from_u16(status).expect("the chain API answers valid statuses");
    headers.apply(&mut response, close);
    let elapsed = started.elapsed();
    ctx.metrics.api_request(route, status, bytes, elapsed);
    debug!(
        %peer,
        endpoint = route.label(),
        block = ?block,
        status,
        bytes,
        us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX),
        "chain API request"
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_origin_form_targets_without_dot_dot_are_legal() {
        let target = |t: &str| {
            let uri: Uri = t.parse().unwrap();
            origin_form(&uri).map(str::to_owned)
        };
        for legal in [
            "/",
            "/v1/chain/get_raw_block",
            "/v1/chain/get_raw_block?x=1",
            "/a/./b",
            "/%2e%2e/x",
        ] {
            assert_eq!(target(legal).as_deref(), Some(legal));
        }
        for illegal in [
            "*",
            "http://127.0.0.1/v1/chain/get_raw_block",
            "127.0.0.1:8888",
            "/v1/../chain/get_raw_block",
            "/v1/chain/get_raw_block?a=..",
            "/..",
        ] {
            assert_eq!(target(illegal), None, "{illegal}");
        }
    }

    #[test]
    fn answers_lay_out_their_headers_as_nodeos_does() {
        let server = format!("server: {SERVER_HEADER}");
        let s = server.as_str();
        let (ct, close, keep) = (
            "content-type: application/json",
            "connection: close",
            "connection: keep-alive",
        );
        let layout = |version, connection: &[&str], route, closing| {
            let mut req = Request::builder().version(version).uri("/");
            for c in connection {
                req = req.header(CONNECTION, *c);
            }
            let (head, ()) = req.body(()).unwrap().into_parts();
            let mut response = Response::new(Full::new(Bytes::new()));
            Headers::new(&head, route).apply(&mut response, closing);
            let lines: Vec<String> = response
                .headers()
                .iter()
                .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap()))
                .collect();
            (response.version(), lines)
        };
        let (v10, v11) = (Version::HTTP_10, Version::HTTP_11);
        use ApiRoute::*;
        let (none, close_keep, keep_close) = (
            &[][..],
            &["close", "keep-alive"][..],
            &["keep-alive", "close"][..],
        );
        let cases = [
            // A handler sets Content-Type last; the HTTP layer's own answers first.
            (v11, none, GetRawBlock, false, vec![s, ct]),
            (v11, none, Describe, false, vec![s, ct]),
            (v11, none, Other, false, vec![ct, s]),
            (v11, none, GetSupportedApis, false, vec![ct, s]),
            (v11, none, GetBlock, false, vec![ct, s]),
            // The connection header the request asks for comes before Server.
            (
                v11,
                &["keep-alive, Close"],
                GetBlockInfo,
                false,
                vec![close, s, ct],
            ),
            (v11, &["close"], Other, false, vec![ct, close, s]),
            (v11, &["keep-alive"], GetBlockHeader, false, vec![s, ct]),
            (v10, none, GetRawBlock, false, vec![s, ct]),
            (v10, &["Keep-Alive"], GetRawBlock, false, vec![keep, s, ct]),
            (v10, &["keep-alive"], Other, false, vec![ct, keep, s]),
            // HTTP/1.0: the first Connection line alone counts, as Beast reads it.
            (v10, close_keep, GetRawBlock, false, vec![s, ct]),
            (v10, keep_close, GetRawBlock, false, vec![keep, s, ct]),
            // HTTP/1.1: any line that closes, as hyper then closes the connection.
            (v11, keep_close, GetRawBlock, false, vec![close, s, ct]),
            (v11, close_keep, Other, false, vec![ct, close, s]),
            // An answer that closes the connection says so in the same place.
            (v11, none, GetRawBlock, true, vec![close, s, ct]),
            (v10, &["keep-alive"], Other, true, vec![ct, close, s]),
        ];
        for (version, connection, route, closing, want) in cases {
            let (got_version, lines) = layout(version, connection, route, closing);
            assert_eq!(got_version, version);
            assert_eq!(
                lines, want,
                "{version:?} {connection:?} {route:?} {closing}"
            );
        }
    }

    #[test]
    fn targets_are_counted_under_their_endpoint() {
        assert_eq!(route("/v1/chain/get_raw_block"), ApiRoute::GetRawBlock);
        assert_eq!(route("/v1/chain/get_block_info"), ApiRoute::GetBlockInfo);
        assert_eq!(
            route("/v1/chain/get_block_header"),
            ApiRoute::GetBlockHeader
        );
        assert_eq!(route("/v1/chain/get_block"), ApiRoute::GetBlock);
        assert_eq!(route(DESCRIBE), ApiRoute::Describe);
        assert_eq!(route(SUPPORTED_APIS), ApiRoute::GetSupportedApis);
        for other in [
            "/v1/chain/get_raw_block?x=1",
            "/v1/chain/get_info",
            "/v1/chain/get_block?",
            "/",
        ] {
            assert_eq!(route(other), ApiRoute::Other, "{other}");
        }
    }

    #[test]
    fn reservations_cover_the_block_and_its_body() {
        use Endpoint::*;
        let len = 1000;
        assert_eq!(first_reservation(GetBlockInfo, false, len), len + SLACK);
        assert_eq!(first_reservation(GetBlockHeader, false, len), len + SLACK);
        assert_eq!(
            first_reservation(GetBlockHeader, true, len),
            4 * len + SLACK
        );
        assert_eq!(first_reservation(GetRawBlock, false, len), 4 * len + SLACK);
        // Nothing overflows.
        assert_eq!(first_reservation(GetRawBlock, false, u64::MAX), u64::MAX);
    }

    #[test]
    fn a_body_keeps_its_reservation_until_its_last_byte_is_dropped() {
        let budget = MemoryBudget::new(1000);
        let reserved = vec![
            budget.try_reserve(250).unwrap(),
            budget.try_reserve(50).unwrap(),
        ];
        let body = into_bytes(vec![7; 300], reserved);
        assert_eq!((body.len(), budget.used()), (300, 300));
        // What a writer still holds of it after writing the first 100 bytes.
        let rest = body.slice(100..);
        drop(body);
        assert_eq!(budget.used(), 300);
        drop(rest);
        assert_eq!(budget.used(), 0);
        // A body that holds no budget is plain bytes.
        assert_eq!(&into_bytes(vec![1, 2], Vec::new())[..], [1, 2]);
    }

    #[test]
    fn request_bodies_reserve_their_room_past_the_unreserved_part() {
        const KIB: usize = 1 << 10;
        let budget = MemoryBudget::new(1 << 20);
        let mut body = Body::default();
        // Room grows to powers of two; the first 16 KiB of it is not reserved.
        for (len, room, used) in [
            (100, 128, 0),
            (128, 128, 0),
            (16 * KIB, 16 * KIB, 0),
            (16 * KIB + 1, 32 * KIB, 16 * KIB),
            (40 * KIB, 64 * KIB, 48 * KIB),
            (600 * KIB, 1024 * KIB, 1008 * KIB),
        ] {
            assert!(body.make_room(len, 2 << 20, &budget).is_ok());
            assert_eq!(body.bytes.capacity(), room, "{len}");
            assert_eq!(budget.used(), used as u64, "{len}");
        }
        drop(body);
        assert_eq!(budget.used(), 0);

        // Never past a declared length.
        let mut body = Body::default();
        assert!(body.make_room(20 * KIB, 50 * KIB, &budget).is_ok());
        assert!(body.make_room(40 * KIB, 50 * KIB, &budget).is_ok());
        assert_eq!(body.bytes.capacity(), 50 * KIB);
        assert_eq!(budget.used(), 34 * KIB as u64);

        // A budget that cannot hold the room refuses it, and nothing more is reserved.
        let held = budget.try_reserve((1 << 20) - 40 * KIB as u64).unwrap();
        let used = budget.used();
        assert!(matches!(
            body.make_room(60 * KIB, 2 << 20, &budget),
            Err(Unread::Refused(n)) if n == used
        ));
        assert_eq!((budget.used(), body.bytes.capacity()), (used, 50 * KIB));
        drop((body, held));
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn a_connection_over_the_cap_is_refused_with_a_whole_response() {
        let refusal = String::from_utf8(refusal(7)).unwrap();
        let (head, body) = refusal.split_once("\r\n\r\n").unwrap();
        let want = ApiError::Busy("Too many connections: 7".into()).body();
        assert_eq!(body.as_bytes(), want);
        let mut lines = head.split("\r\n");
        assert_eq!(lines.next(), Some("HTTP/1.1 503 Service Unavailable"));
        let mut headers: Vec<_> = lines.collect();
        headers.sort();
        assert_eq!(
            headers,
            [
                "Connection: close".to_owned(),
                format!("Content-Length: {}", want.len()),
                "Content-Type: application/json".to_owned(),
                format!("Server: {SERVER_HEADER}"),
            ]
        );
    }

    #[test]
    fn panic_messages_are_read_from_either_payload() {
        let text: Box<dyn Any + Send> = Box::new("boom");
        let owned: Box<dyn Any + Send> = Box::new(String::from("bang"));
        let other: Box<dyn Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(&*text), "boom");
        assert_eq!(panic_message(&*owned), "bang");
        assert_eq!(panic_message(&*other), "a non-text panic");
    }
}
