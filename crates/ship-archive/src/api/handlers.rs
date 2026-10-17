//! One chain API request, from its head to its answer:
//!
//!  1. The body is read: at most `--api-max-body-bytes` (a larger one, declared or streamed,
//!     gets `413` and the connection is closed), within the request timeout (otherwise the
//!     connection is closed unanswered).
//!  2. The request-target must be origin-form and hold no `..`, Beast's check: anything else
//!     (`OPTIONS *`, an absolute or authority form) is `400 Illegal request-target`.
//!  3. `OPTIONS`, on any path, is `200 {}`.
//!  4. The handler is the one whose path is the whole target, query string included:
//!     a block endpoint, `describe` or `get_supported_apis`; anything else is nodeos's 404.
//!
//! A block request is then parsed as nodeos parses it (`hyperion_ship::chainapi::parse`); a
//! block number outside the served block-log range is `Unknown Block` at once. Otherwise the
//! request takes a slot ([`admit`]) and runs one [`Job`] on an io thread: it reserves what it
//! will hold from the memory budget, reads the block, checks it is the block asked for, walks
//! it and renders the body. Once the block is dropped the reservation shrinks to the body,
//! which keeps it until hyper has written it out: a client that does not read its answer holds
//! budget, never memory beyond it.

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
use hyper::{Method, Request, Response, StatusCode, Uri};
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
    let body = match read_body(body, ctx.cfg.max_body_bytes, ctx.cfg.request_timeout).await {
        Ok(body) => body,
        Err(Unread::TooLarge) => {
            ctx.metrics.api_rejected(ApiRejection::BodyTooLarge);
            let reply = Reply::error(&ApiError::TooLarge(ctx.cfg.max_body_bytes));
            return Ok(respond(&ctx, peer, route, None, reply, started));
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
        Some(target) => answer(&ctx, peer, target, &body).await,
    };
    Ok(respond(&ctx, peer, route, block, reply, started))
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
    /// The connection failed while it was read.
    Failed(hyper::Error),
}

/// The whole body, if it is at most `cap` bytes and arrives within `timeout`.
async fn read_body(body: Incoming, cap: usize, timeout: Duration) -> Result<Vec<u8>, Unread> {
    if body.size_hint().lower() > cap as u64 {
        return Err(Unread::TooLarge);
    }
    let read = async {
        let mut body = body;
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(Unread::Failed)?;
            if let Ok(data) = frame.into_data() {
                if out.len() + data.len() > cap {
                    return Err(Unread::TooLarge);
                }
                out.extend_from_slice(&data);
            }
        }
        Ok(out)
    };
    tokio::time::timeout(timeout, read)
        .await
        .unwrap_or(Err(Unread::Late))
}

/// The answer to a request with a legal target other than `OPTIONS`, and the block it named.
async fn answer(ctx: &Ctx, peer: SocketAddr, target: &str, body: &[u8]) -> (Reply, Option<u32>) {
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
    body: &[u8],
) -> (Reply, Option<u32>) {
    let req = match chainapi::parse(endpoint, body) {
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
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(SERVER, HeaderValue::from_static(SERVER_HEADER));
    if close {
        headers.insert(CONNECTION, HeaderValue::from_static("close"));
    }
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
    fn panic_messages_are_read_from_either_payload() {
        let text: Box<dyn Any + Send> = Box::new("boom");
        let owned: Box<dyn Any + Send> = Box::new(String::from("bang"));
        let other: Box<dyn Any + Send> = Box::new(7u8);
        assert_eq!(panic_message(&*text), "boom");
        assert_eq!(panic_message(&*owned), "bang");
        assert_eq!(panic_message(&*other), "a non-text panic");
    }
}
