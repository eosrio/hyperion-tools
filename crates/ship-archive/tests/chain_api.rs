//! The chain API (`--api`) end to end, over TCP on loopback, read byte for byte:
//!
//!   * S1: each endpoint by number and by id, at the edges of the block logs and of `--range`,
//!     for a block log reaching below the SHiP logs, and for a public mainnet block;
//!   * S2: dispatch on the whole target, any method, OPTIONS, illegal targets, the listings;
//!   * S3: the response headers and keep-alive;
//!   * S4: empty, unparsable, oversized and slow bodies, slow heads, and answers left unread;
//!   * S5: full slots and a full memory budget, a body outgrowing its first reservation, large
//!     request bodies within the budget, and connections over `--api-max-connections`;
//!   * S6: a block that does not decode and a block that cannot be read;
//!   * S7: configurations refused at startup;
//!   * S8: a SHiP session streaming while API clients hammer `get_raw_block`;
//!   * S9: shutdown with a request in flight;
//!   * S10: the metrics and the `/health` block.
//!
//! Block bodies are compared with what the core renderer writes for the stored block (its own
//! tests pin those bytes to nodeos's); error bodies with nodeos's exact bytes.

mod common;

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{Fixture, QUIET, WAIT, Ws, blocks, fetch_all, recv, recv_binary, send, serve_config};
use hyperion_ship::chainapi::{ApiError, Endpoint, parse, parse_signed_block, render};
use hyperion_ship::protocol::Request as ShipRequest;
use hyperion_ship::testkit::blocks::{
    self as testblocks, Block, BlockSpec, EOSIO, Genesis, Key, Preamble, Receipt, ReceiptTrx,
    Signature,
};
use hyperion_ship::testkit::ship::{Encoding, LogBuilder};
use serde_json::{Value, json};
use ship_archive::api::ApiConfig;
use ship_archive::fetch::MemoryBudget;
use ship_archive::metrics::{ApiFailure, ApiRejection, ApiRoute, Metrics};
use ship_archive::store::{BlocksMode, StoreConfig};
use ship_archive::{SERVER_HEADER, ServeConfig, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use Endpoint::{GetBlockHeader, GetBlockInfo, GetRawBlock};

// ---- nodeos's bytes ----

const NOT_FOUND: &str = r#"{"code":404,"message":"Not Found","error":{"code":0,"name":"exception","what":"unspecified","details":[{"message":"Unknown Endpoint","file":"beast_http_session.hpp","line_number":186,"method":"handle_request"}]}}"#;
const ILLEGAL_TARGET: &str = r#"{"code":400,"message":"Illegal request-target","error":{"code":0,"name":"","what":"","details":[]}}"#;
const BODY_REQUIRED: &str = r#"{"code":400,"message":"Invalid Request","error":{"code":3200006,"name":"invalid_http_request","what":"invalid http request","details":[{"message":"A Request body is required","file":"http_plugin.hpp","line_number":257,"method":"parse_params"}]}}"#;
const UNPARSABLE: &str = r#"{"code":400,"message":"Invalid Request","error":{"code":3200006,"name":"invalid_http_request","what":"invalid http request","details":[{"message":"Unable to parse valid input from POST body","file":"http_plugin.hpp","line_number":278,"method":"parse_params"}]}}"#;
const SUPPORTED: &str = r#"{"apis":["/v1/chain/get_block_header","/v1/chain/get_block_info","/v1/chain/get_raw_block"]}"#;

/// A `503 Busy` body.
fn busy(what: &str) -> String {
    format!(
        r#"{{"code":503,"message":"Busy","error":{{"code":503,"name":"Busy","what":"{what}","details":[]}}}}"#
    )
}

/// The modes a block can be asked for in: each endpoint, the header in both.
const MODES: [(Endpoint, bool); 4] = [
    (GetBlockInfo, false),
    (GetBlockHeader, false),
    (GetBlockHeader, true),
    (GetRawBlock, false),
];

/// A body asking `endpoint` for block `reference` (JSON text: a number or a string).
fn body_for(endpoint: Endpoint, reference: &str, include_extensions: bool) -> String {
    match endpoint {
        GetBlockInfo => format!("{{\"block_num\":{reference}}}"),
        GetBlockHeader => format!(
            "{{\"block_num_or_id\":{reference},\"include_extensions\":{include_extensions}}}"
        ),
        GetRawBlock => format!("{{\"block_num_or_id\":{reference}}}"),
    }
}

/// What `endpoint` answers for the stored `block`, as the core renders it.
fn rendered(block: &[u8], endpoint: Endpoint, include_extensions: bool) -> Vec<u8> {
    let b = parse_signed_block(block).expect("a block that decodes");
    let mut out = Vec::new();
    render(endpoint, &b, include_extensions, usize::MAX, &mut out).expect("a block that renders");
    out
}

/// What `endpoint` answers when the block `body` names is not served.
fn unknown(endpoint: Endpoint, body: &str) -> Vec<u8> {
    parse(endpoint, body.as_bytes())
        .expect("a body that parses")
        .unknown_block()
        .body()
}

// ---- fixtures ----

/// The last block of a [`Chain`].
const LAST: u32 = 120;

/// Blocks 1 to [`LAST`] of a chain whose blocks hold what the renderer writes, laid out as a
/// node leaves them: a trace log from block 2, so the block log, from block 1, reaches below
/// the SHiP logs, and the block log split every 50 blocks.
struct Chain {
    tmp: tempfile::TempDir,
    blocks: Vec<Block>,
}

impl Chain {
    fn new() -> Self {
        Self::build(rich, |_| {})
    }

    /// Blocks built from `spec`, stored after `damage` changed them (the trace log keeps the
    /// chain's ids).
    fn build(spec: fn(u32) -> BlockSpec, damage: impl FnOnce(&mut [Block])) -> Self {
        let blocks = testblocks::extend(&[0; 32], LAST, spec).unwrap();
        let tmp = tempfile::Builder::new()
            .prefix("ship-archive-api-")
            .tempdir()
            .unwrap();
        let mut trace = LogBuilder::new(Encoding::SizedStored);
        for b in &blocks[1..] {
            trace.push(b.id, &b.num.to_le_bytes());
        }
        trace
            .write(&tmp.path().join("state-history"), "trace_history")
            .unwrap();
        let mut stored = blocks.clone();
        damage(&mut stored);
        let dir = tmp.path().join("blocks");
        std::fs::create_dir_all(&dir).unwrap();
        let preamble = Preamble::V3Genesis {
            genesis: Genesis::eos_mainnet(),
        };
        testblocks::write_split(&dir, &preamble, &stored, 50).unwrap();
        Self { tmp, blocks }
    }

    fn block(&self, n: u32) -> &Block {
        &self.blocks[(n - 1) as usize]
    }

    fn store(&self) -> StoreConfig {
        StoreConfig {
            dirs: vec![self.tmp.path().to_path_buf()],
            recursive: true,
            ..StoreConfig::default()
        }
    }

    fn config(&self) -> ServeConfig {
        with_api(serve_config(self.store()))
    }
}

/// Block `n` of a [`Chain`]: producer schedules with K1, R1 and WebAuthn keys, header and
/// block extensions, receipts of every status, id-only and packed (both compressions), and
/// K1, R1 and WebAuthn producer signatures.
fn rich(n: u32) -> BlockSpec {
    let byte = n as u8;
    let point = |b: u8| {
        let mut p = [b; 33];
        p[0] = 2 + (b & 1);
        p
    };
    let mut spec = BlockSpec::synthetic(n);
    spec.producer = EOSIO + u64::from(n % 3) * 0x10;
    spec.confirmed = (n % 4) as u16;
    spec.transaction_mroot = [byte; 32];
    spec.action_mroot = [!byte; 32];
    spec.schedule_version = n / 10;
    if n.is_multiple_of(10) {
        spec.new_producers = Some((
            n / 10,
            vec![
                (EOSIO, Key::K1(point(byte))),
                (EOSIO + 0x10, Key::R1(point(byte ^ 1))),
                (
                    EOSIO + 0x20,
                    Key::WebAuthn {
                        point: point(byte ^ 2),
                        user_presence: 1,
                        rpid: "keys.example".into(),
                    },
                ),
            ],
        ));
    }
    if n.is_multiple_of(7) {
        spec.header_extensions = vec![(0, vec![byte; 32])];
    }
    spec.producer_signature = match n % 3 {
        0 => Signature::K1([byte; 65]),
        1 => Signature::R1([byte ^ 0x55; 65]),
        _ => Signature::WebAuthn {
            compact: [byte; 65],
            auth_data: vec![byte; 37],
            client_json: format!("{{\"type\":\"webauthn.get\",\"n\":{n}}}"),
        },
    };
    spec.transactions = (0..n % 6)
        .map(|i| Receipt {
            status: ((n + i) % 5) as u8,
            cpu_usage_us: 100 * i + n,
            net_usage_words: 8 * i,
            trx: if i % 2 == 0 {
                ReceiptTrx::Id([byte.wrapping_add(i as u8); 32])
            } else {
                ReceiptTrx::Packed {
                    signatures: vec![Signature::K1([i as u8; 65])],
                    compression: ((n + i) % 2) as u8,
                    packed_context_free_data: vec![byte; (i % 3) as usize],
                    packed_trx: vec![0xa5; 24 + i as usize],
                }
            },
        })
        .collect();
    spec
}

/// `cfg` with the chain API on an ephemeral loopback port, at its defaults.
fn with_api(mut cfg: ServeConfig) -> ServeConfig {
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    cfg.api = Some(ApiConfig::new(listen, cfg.io_threads));
    cfg
}

fn api_cfg(cfg: &mut ServeConfig) -> &mut ApiConfig {
    cfg.api.as_mut().expect("the chain API is configured")
}

// ---- the server ----

/// A server with the chain API, running in the test's runtime. Dropping it shuts it down.
struct Running {
    api: SocketAddr,
    ship: SocketAddr,
    http: Option<SocketAddr>,
    metrics: Arc<Metrics>,
    budget: Arc<MemoryBudget>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    /// Shut the server down and wait for it to finish.
    async fn stop(mut self) -> anyhow::Result<()> {
        drop(self.stop.take());
        let task = self.task.take().expect("running");
        tokio::time::timeout(WAIT, task)
            .await
            .expect("the server stops")
            .expect("server task")
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.stop.take());
    }
}

async fn start(cfg: ServeConfig) -> Running {
    let server = Server::bind(cfg).await.expect("the server starts");
    let (stop, stopped) = oneshot::channel::<()>();
    let (api, ship, http) = (
        server.api_addr().expect("--api is bound"),
        server.local_addr(),
        server.http_addr(),
    );
    let (metrics, budget) = (Arc::clone(server.metrics()), Arc::clone(server.budget()));
    let task = tokio::spawn(server.run(async move {
        let _ = stopped.await;
    }));
    Running {
        api,
        ship,
        http,
        metrics,
        budget,
        stop: Some(stop),
        task: Some(task),
    }
}

/// Wait until `done` holds.
async fn until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---- an HTTP client that sees every byte ----

/// One HTTP/1.1 connection, read byte for byte.
struct Http {
    tcp: TcpStream,
    buf: Vec<u8>,
}

/// A response: its status, its head as sent (the status line, then each header line), its body.
#[derive(Debug)]
struct Answer {
    status: u16,
    head: Vec<String>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.head[1..].iter().find_map(|line| {
            let (k, v) = line.split_once(": ")?;
            k.eq_ignore_ascii_case(name).then_some(v)
        })
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("a text body")
    }

    /// The header lines, sorted.
    fn headers(&self) -> Vec<String> {
        let mut h = self.head[1..].to_vec();
        h.sort();
        h
    }
}

impl Http {
    async fn connect(addr: SocketAddr) -> Self {
        Self {
            tcp: TcpStream::connect(addr).await.expect("connect to --api"),
            buf: Vec::new(),
        }
    }

    /// A connection whose client takes little at a time: with an 8 KiB receive buffer, the
    /// server's writes soon wait on its reads.
    async fn connect_small(addr: SocketAddr) -> Self {
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(8 << 10).unwrap();
        Self {
            tcp: socket.connect(addr).await.expect("connect to --api"),
            buf: Vec::new(),
        }
    }

    async fn send(&mut self, bytes: &[u8]) {
        self.tcp.write_all(bytes).await.expect("send");
    }

    async fn request(&mut self, method: &str, target: &str, body: &[u8]) -> Answer {
        let mut bytes = format!(
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        bytes.extend_from_slice(body);
        self.send(&bytes).await;
        self.answer(method == "HEAD")
            .await
            .unwrap_or_else(|| panic!("no answer to {method} {target}"))
    }

    async fn post(&mut self, target: &str, body: impl AsRef<[u8]>) -> Answer {
        self.request("POST", target, body.as_ref()).await
    }

    /// The next response (without a body for a HEAD request), or `None` if the connection
    /// ends before one starts.
    async fn answer(&mut self, head_only: bool) -> Option<Answer> {
        let end = loop {
            if let Some(at) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            if !self.read_more().await {
                assert!(self.buf.is_empty(), "the connection ended inside a head");
                return None;
            }
        };
        let head = String::from_utf8(self.buf.drain(..end).collect()).expect("a text head");
        let head: Vec<String> = head.trim_end().split("\r\n").map(str::to_owned).collect();
        let status = head[0]
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status line");
        let mut answer = Answer {
            status,
            head,
            body: Vec::new(),
        };
        let len = match head_only {
            true => 0,
            false => answer
                .header("content-length")
                .expect("a Content-Length")
                .parse()
                .unwrap(),
        };
        while self.buf.len() < len {
            assert!(self.read_more().await, "the connection ended inside a body");
        }
        answer.body = self.buf.drain(..len).collect();
        Some(answer)
    }

    /// Whether the server closes the connection within `within`, sending nothing more.
    async fn closes_within(&mut self, within: Duration) -> bool {
        let mut chunk = [0; 4096];
        match tokio::time::timeout(within, self.tcp.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => self.buf.is_empty(),
            Ok(Ok(n)) => panic!(
                "expected a close, got {:?}",
                String::from_utf8_lossy(&chunk[..n])
            ),
            Err(_) => false,
        }
    }

    async fn read_more(&mut self) -> bool {
        let mut chunk = vec![0; 64 << 10];
        match tokio::time::timeout(WAIT, self.tcp.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => false,
            Ok(Ok(n)) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            Err(_) => panic!("nothing from the server within {WAIT:?}"),
        }
    }
}

/// The bytes of a POST with `body`.
fn post_bytes(target: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// One POST on a connection of its own.
async fn post(addr: SocketAddr, target: &str, body: impl AsRef<[u8]>) -> Answer {
    Http::connect(addr).await.post(target, body).await
}

/// One request to the `--http` endpoints: the status and the body.
async fn ops(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut tcp = TcpStream::connect(addr).await.expect("connect to --http");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(WAIT, tcp.read_to_end(&mut response))
        .await
        .expect("an HTTP response")
        .unwrap();
    let response = String::from_utf8(response).expect("a text response");
    let (head, body) = response.split_once("\r\n\r\n").expect("a head and a body");
    let code = head.split(' ').nth(1).and_then(|c| c.parse().ok());
    (code.expect("a status code"), body.to_owned())
}

/// The value of `series` (name and labels) in a metrics text.
fn sample(metrics: &str, series: &str) -> f64 {
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no {series} in\n{metrics}"))
        .parse()
        .unwrap()
}

const P: &str = "antelope_ship_archive_";

// ---- S1 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocks_are_answered_by_number_and_by_id_across_the_block_logs() {
    let chain = Chain::new();
    let server = start(chain.config()).await;
    let mut http = Http::connect(server.api).await;
    // Block 1 is in the block log only, below the trace log; 50 and 51, 100 and 101 sit at
    // the seams of the block-log files; LAST is the last block.
    for n in [1, 2, 3, 10, 50, 51, 70, 100, 101, LAST - 1, LAST] {
        let block = chain.block(n);
        let id = hex::encode(block.id);
        for (ep, ext) in MODES {
            let want = rendered(&block.body, ep, ext);
            let mut references = vec![n.to_string(), format!("\"{n}\"")];
            if ep != GetBlockInfo {
                references.push(format!("\"{id}\""));
                references.push(format!("\"{}\"", id.to_uppercase()));
            }
            for reference in references {
                let a = http.post(ep.path(), body_for(ep, &reference, ext)).await;
                assert_eq!(a.status, 200, "{} {reference}: {}", ep.name(), a.text());
                assert_eq!(a.text(), String::from_utf8_lossy(&want), "{reference}");
            }
        }
        let info: Value =
            serde_json::from_slice(&rendered(&block.body, GetBlockInfo, false)).expect("JSON");
        assert_eq!((&info["block_num"], &info["id"]), (&json!(n), &json!(id)));
    }
    // A number is cut to 32 bits, as nodeos cuts it.
    let a = http
        .post(
            GetRawBlock.path(),
            body_for(GetRawBlock, "\"4294967297\"", false),
        )
        .await;
    assert_eq!(a.body, rendered(&chain.block(1).body, GetRawBlock, false));

    // Outside the block log, and ids naming a served number but another block.
    let raw_123 = r#"{"code":400,"message":"Unknown Block","error":{"code":3100002,"name":"unknown_block_exception","what":"Unknown block","details":[{"message":"Could not find block: 123","file":"chain_plugin.cpp","line_number":1966,"method":"get_raw_block"}]}}"#;
    let a = http
        .post(GetRawBlock.path(), r#"{"block_num_or_id":123}"#)
        .await;
    assert_eq!((a.status, a.text()), (400, raw_123));
    let mut other = chain.block(7).id;
    other[31] ^= 1;
    // The first four bytes of block 10's id, `0000000a`: not a decimal number, so an id,
    // zero-padded into one no block has.
    let short = &hex::encode(chain.block(10).id)[..8];
    assert_eq!(short, "0000000a");
    for (ep, ext) in MODES {
        let mut references = vec!["0".to_owned(), (LAST + 1).to_string(), u32::MAX.to_string()];
        if ep != GetBlockInfo {
            references.push(format!("\"{}\"", hex::encode(other)));
            references.push(format!("\"{short}\""));
        }
        for reference in references {
            let body = body_for(ep, &reference, ext);
            let a = http.post(ep.path(), &body).await;
            assert_eq!(a.status, 400, "{} {reference}", ep.name());
            assert_eq!(a.body, unknown(ep, &body), "{} {reference}", ep.name());
        }
    }
    assert_eq!(server.metrics.api_in_flight(), 0);
    assert_eq!(server.budget.used(), 0, "every reservation is given back");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_range_bounds_the_blocks_answered() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    cfg.store.range = Some((10, 100));
    let server = start(cfg).await;
    for (ep, ext) in MODES {
        for n in [10, 99] {
            let a = post(server.api, ep.path(), body_for(ep, &n.to_string(), ext)).await;
            assert_eq!(a.body, rendered(&chain.block(n).body, ep, ext), "block {n}");
        }
        for n in [9, 100] {
            let body = body_for(ep, &n.to_string(), ext);
            let a = post(server.api, ep.path(), &body).await;
            assert_eq!((a.status, a.body), (400, unknown(ep, &body)), "block {n}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_public_mainnet_block_is_answered() {
    let hex = include_str!("../../core/testdata/blocks/eos-mainnet-480385480.hex");
    let body = hex::decode(hex.trim()).unwrap();
    let header = hyperion_ship::blocklog::block_header(&body).unwrap();
    let n = 480_385_480;
    let id = "1ca219c8aaf55cb9d7b6521f5470c667292a904996c03e190a0cd913d4714bb4";
    assert_eq!((header.number, hex::encode(header.id).as_str()), (n, id));
    let tmp = tempfile::Builder::new()
        .prefix("ship-archive-api-")
        .tempdir()
        .unwrap();
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    trace.push(header.id, b"T");
    trace.write(tmp.path(), "trace_history").unwrap();
    let block = Block {
        num: n,
        id: header.id,
        previous: header.previous,
        body: body.clone(),
    };
    let chain_id = Genesis::eos_mainnet().chain_id();
    let preamble = Preamble::V3ChainId { first: n, chain_id };
    testblocks::write(tmp.path(), "blocks", &preamble, &[block]).unwrap();
    let cfg = with_api(serve_config(StoreConfig {
        dirs: vec![tmp.path().to_path_buf()],
        ..StoreConfig::default()
    }));
    let max_in_flight = cfg.api.as_ref().unwrap().max_in_flight;
    let server = start(cfg).await;
    let mut http = Http::connect(server.api).await;
    for (ep, ext) in MODES {
        let want = rendered(&body, ep, ext);
        let mut references = vec![n.to_string()];
        if ep != GetBlockInfo {
            references.push(format!("\"{id}\""));
        }
        for reference in references {
            let a = http.post(ep.path(), body_for(ep, &reference, ext)).await;
            assert_eq!(
                (a.status, &a.body),
                (200, &want),
                "{} {reference}",
                ep.name()
            );
        }
    }
    let info = http
        .post(GetBlockInfo.path(), format!("{{\"block_num\":{n}}}"))
        .await;
    let info: Value = serde_json::from_slice(&info.body).unwrap();
    assert_eq!(info["id"], id);
    assert_eq!(info["ref_block_prefix"], 525_514_455);
    assert_eq!(info["ref_block_num"], n & 0xffff);
    let raw: Value = serde_json::from_slice(
        &http
            .post(GetRawBlock.path(), format!("{{\"block_num_or_id\":{n}}}"))
            .await
            .body,
    )
    .unwrap();
    assert!(!raw["transactions"].as_array().unwrap().is_empty());

    let describe = http.post("/v1/archive/describe", "").await;
    let want = format!(
        "{{\"server\":\"{SERVER_HEADER}\",\"chain_id\":\"{}\",\"last\":{{\"num\":{n},\"id\":\
         \"{id}\"}},\"blocks\":{{\"begin\":{n},\"end\":{}}},\"apis\":[\"/v1/chain/\
         get_block_header\",\"/v1/chain/get_block_info\",\"/v1/chain/get_raw_block\"],\
         \"get_block\":\"off\",\"max_in_flight\":{max_in_flight},\"in_flight\":0}}",
        hex::encode(chain_id),
        n + 1
    );
    assert_eq!((describe.status, describe.text()), (200, want.as_str()));
}

// ---- S2 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn targets_methods_and_listings_are_dispatched_as_nodeos_does() {
    let chain = Chain::new();
    let cfg = chain.config();
    let max_in_flight = cfg.api.as_ref().unwrap().max_in_flight;
    let server = start(cfg).await;
    let mut http = Http::connect(server.api).await;
    let info = r#"{"block_num":5}"#;
    let want = rendered(&chain.block(5).body, GetBlockInfo, false);

    // Any method reaches the handler, with its body.
    for method in ["POST", "GET", "PUT", "DELETE", "PATCH", "FOO"] {
        let a = http
            .request(method, GetBlockInfo.path(), info.as_bytes())
            .await;
        assert_eq!((a.status, &a.body), (200, &want), "{method}");
    }
    let head = http
        .request("HEAD", GetBlockInfo.path(), info.as_bytes())
        .await;
    assert_eq!(head.status, 200);
    assert_eq!(
        head.header("content-length"),
        Some(want.len().to_string().as_str())
    );

    // OPTIONS is `{}` on any path, whatever the body.
    for target in [GetRawBlock.path(), "/v1/chain/get_info", "/nowhere", "/"] {
        let a = http.request("OPTIONS", target, b"not json").await;
        assert_eq!((a.status, a.text()), (200, "{}"), "{target}");
    }

    // The handler is found by the whole target.
    for target in [
        "/v1/chain/get_raw_block?block_num_or_id=5",
        "/v1/chain/get_raw_block/",
        "/v1/chain/get_info",
        "/v1/chain/get_block",
        "/v1/chain/get_block_header_state",
        "/v1/chain/get_account",
        "/v1/chain/push_transaction",
        "/v1/chain/push_block",
        "/v1/trace_api/get_block",
        "/v1/archive/describe/",
        "/v1/archive/other",
        "/",
    ] {
        let a = http.post(target, r#"{"block_num_or_id":5}"#).await;
        assert_eq!((a.status, a.text()), (404, NOT_FOUND), "{target}");
        let a = http.request("GET", target, b"").await;
        assert_eq!((a.status, a.text()), (404, NOT_FOUND), "{target}");
    }

    // The listings, whatever the method and body.
    for (method, body) in [
        ("POST", ""),
        ("GET", ""),
        ("POST", "{\"x\":1}"),
        ("POST", "]"),
    ] {
        let a = http
            .request(method, "/v1/node/get_supported_apis", body.as_bytes())
            .await;
        assert_eq!((a.status, a.text()), (200, SUPPORTED), "{method} {body}");
        let a = http
            .request(method, "/v1/archive/describe", body.as_bytes())
            .await;
        assert_eq!(a.status, 200);
        let want = format!(
            "{{\"server\":\"{SERVER_HEADER}\",\"chain_id\":\"{}\",\"last\":{{\"num\":{LAST},\
             \"id\":\"{}\"}},\"blocks\":{{\"begin\":1,\"end\":{}}},\"apis\":[\"/v1/chain/\
             get_block_header\",\"/v1/chain/get_block_info\",\"/v1/chain/get_raw_block\"],\
             \"get_block\":\"off\",\"max_in_flight\":{max_in_flight},\"in_flight\":0}}",
            hex::encode(Genesis::eos_mainnet().chain_id()),
            hex::encode(chain.block(LAST).id),
            LAST + 1
        );
        assert_eq!(a.text(), want);
    }

    // A target that is not origin-form, or climbs with `..`, is checked before anything.
    for raw in [
        "OPTIONS * HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
        "GET http://127.0.0.1/v1/chain/get_info HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        "POST http://127.0.0.1/v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Length: 21\r\n\r\n{\"block_num_or_id\":5}",
        "POST /v1/chain/../chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\n\
         Content-Length: 21\r\n\r\n{\"block_num_or_id\":5}",
        "OPTIONS /v1/../x HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 0\r\n\r\n",
    ] {
        http.send(raw.as_bytes()).await;
        let a = http.answer(false).await.expect("an answer");
        assert_eq!((a.status, a.text()), (400, ILLEGAL_TARGET), "{raw}");
    }

    // The documented differences: hyper drops a fragment before the handlers see the target,
    // and answers a target outside its URI grammar itself, empty, closing the connection.
    for target in [
        "/v1/chain/get_block_info#x",
        "/v1/chain/get_block_info#/../x",
    ] {
        let a = http.post(target, info).await;
        assert_eq!((a.status, &a.body), (200, &want), "{target}");
    }
    let a = http.post("/v1/chain/get_block_info<", info).await;
    assert_eq!(
        (a.status, a.body.len(), a.header("content-type")),
        (400, 0, None)
    );
    assert!(http.closes_within(WAIT).await);
}

// ---- S3 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_carry_nodeos_headers_on_kept_alive_connections() {
    let chain = Chain::new();
    let server = start(chain.config()).await;
    let expected = |len: usize| {
        let mut h = vec![
            format!("Content-Length: {len}"),
            "Content-Type: application/json".to_owned(),
            format!("Server: {SERVER_HEADER}"),
        ];
        h.sort();
        h
    };
    // One connection: a block, an error of each status, the listings.
    let mut http = Http::connect(server.api).await;
    let cases: [(&str, &str, &str); 6] = [
        (
            GetRawBlock.path(),
            r#"{"block_num_or_id":9}"#,
            "HTTP/1.1 200 OK",
        ),
        (
            GetRawBlock.path(),
            r#"{"block_num_or_id":900}"#,
            "HTTP/1.1 400 Bad Request",
        ),
        (
            GetRawBlock.path(),
            r#"{"block_num_or_id":"zz"}"#,
            "HTTP/1.1 500 Internal Server Error",
        ),
        ("/v1/chain/get_info", "", "HTTP/1.1 404 Not Found"),
        ("/v1/node/get_supported_apis", "", "HTTP/1.1 200 OK"),
        ("/v1/archive/describe", "", "HTTP/1.1 200 OK"),
    ];
    for (target, body, status_line) in cases {
        let a = http.post(target, body).await;
        assert_eq!(a.head[0], status_line, "{target} {body}");
        assert_eq!(a.headers(), expected(a.body.len()), "{target} {body}");
        assert!(a.header("date").is_none());
    }
    assert!(!http.closes_within(QUIET).await, "kept alive");

    // HTTP/1.0 closes unless asked to keep alive; `Connection: close` is honoured.
    let one = r#"{"block_num_or_id":9}"#;
    for (version, connection) in [("HTTP/1.0", ""), ("HTTP/1.1", "Connection: close\r\n")] {
        let mut http = Http::connect(server.api).await;
        let request = format!(
            "POST {} {version}\r\nHost: 127.0.0.1\r\n{connection}Content-Length: {}\r\n\r\n{one}",
            GetRawBlock.path(),
            one.len()
        );
        http.send(request.as_bytes()).await;
        let a = http.answer(false).await.expect("an answer");
        assert_eq!(a.status, 200, "{version} {connection}");
        assert!(http.closes_within(WAIT).await, "{version} {connection}");
    }
    let mut http = Http::connect(server.api).await;
    let request = format!(
        "POST {} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: keep-alive\r\nContent-Length: \
         {}\r\n\r\n{one}",
        GetRawBlock.path(),
        one.len()
    );
    for _ in 0..2 {
        http.send(request.as_bytes()).await;
        assert_eq!(http.answer(false).await.expect("an answer").status, 200);
    }
}

// ---- S4 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bodies_are_read_within_their_limits_and_parsed_as_nodeos_does() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    api_cfg(&mut cfg).max_body_bytes = 256;
    api_cfg(&mut cfg).request_timeout = Duration::from_millis(400);
    let server = start(cfg).await;
    let mut http = Http::connect(server.api).await;

    // Empty is spaces only, or braces around spaces and TABs.
    for ep in [GetBlockInfo, GetBlockHeader, GetRawBlock] {
        for body in ["", "   ", "{}", "{ \t }", "  {  }  ", "{\t}"] {
            let a = http.post(ep.path(), body).await;
            assert_eq!(
                (a.status, a.text()),
                (400, BODY_REQUIRED),
                "{} {body:?}",
                ep.name()
            );
        }
        // Not empty: the fields take their defaults.
        for body in ["{}\n", "\t{}", "\n{ }"] {
            let a = http.post(ep.path(), body).await;
            let want = match ep {
                GetBlockInfo => unknown(ep, body),
                _ => ApiError::BadLength { endpoint: ep }.body(),
            };
            assert_eq!(
                a.text(),
                String::from_utf8_lossy(&want),
                "{} {body:?}",
                ep.name()
            );
        }
        for body in [
            r#"{"block_num_or_id":1.2.3}"#,
            r#"{"block_num":"x"}"#,
            "[1]",
            "1",
        ] {
            let body = match (ep, body) {
                (GetBlockInfo, b) => b.replace("block_num_or_id", "block_num"),
                (_, b) => b.replace("\"block_num\"", "\"block_num_or_id\":{},\"x\""),
            };
            let a = http.post(ep.path(), &body).await;
            assert_eq!(
                (a.status, a.text()),
                (400, UNPARSABLE),
                "{} {body}",
                ep.name()
            );
        }
    }
    // A request with no Content-Length has an empty body.
    http.send(b"POST /v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
        .await;
    let a = http.answer(false).await.expect("an answer");
    assert_eq!((a.status, a.text()), (400, BODY_REQUIRED));
    // The bad-id and length errors.
    let a = http
        .post(GetRawBlock.path(), r#"{"block_num_or_id":"0x12"}"#)
        .await;
    let want = r#"{"code":500,"message":"Internal Service Error","error":{"code":3010008,"name":"block_id_type_exception","what":"Invalid block ID","details":[{"message":"Invalid block ID: 0x12","file":"chain_plugin.cpp","line_number":1963,"method":"get_raw_block"}]}}"#;
    assert_eq!((a.status, a.text()), (500, want));
    // fc reads `1e5` as the string "1e5", and a fraction where nothing reads it is no error.
    let a = http
        .post(GetRawBlock.path(), r#"{"block_num_or_id":1e5}"#)
        .await;
    assert_eq!(
        (a.status, a.text()),
        (500, want.replace("0x12", "1e5").as_str())
    );
    for ep in [GetBlockInfo, GetBlockHeader, GetRawBlock] {
        let body = match ep {
            GetBlockInfo => r#"{"x":1.5,"block_num":3,"y":-2e-3}"#,
            _ => r#"{"x":1.5,"block_num_or_id":3,"y":-2e-3}"#,
        };
        let a = http.post(ep.path(), body).await;
        assert_eq!(
            a.body,
            rendered(&chain.block(3).body, ep, false),
            "{}",
            ep.name()
        );
    }
    let long = format!("{{\"block_num_or_id\":\"{}\"}}", "a".repeat(66));
    let a = http.post(GetBlockHeader.path(), &long).await;
    let want = ApiError::BadLength {
        endpoint: GetBlockHeader,
    }
    .body();
    assert_eq!((a.status, a.body), (500, want));

    // A body of exactly the cap is read; one byte more, declared or streamed, is 413 and the
    // connection is closed.
    let exact = format!("{:<256}", r#"{"block_num_or_id":"3"}"#);
    assert_eq!(exact.len(), 256);
    let a = http.post(GetRawBlock.path(), &exact).await;
    assert_eq!(a.body, rendered(&chain.block(3).body, GetRawBlock, false));
    let too_large = r#"{"code":413,"message":"Payload Too Large","error":{"code":413,"name":"Payload Too Large","what":"Request body exceeds 256 bytes","details":[]}}"#;
    http.send(
        b"POST /v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 257\r\n\r\n",
    )
    .await;
    let a = http.answer(false).await.expect("an answer");
    assert_eq!((a.status, a.text()), (413, too_large));
    assert_eq!(a.header("connection"), Some("close"));
    assert!(http.closes_within(WAIT).await);
    let mut http = Http::connect(server.api).await;
    let mut chunked = b"POST /v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\n\
                        Transfer-Encoding: chunked\r\n\r\n101\r\n"
        .to_vec();
    chunked.extend_from_slice(format!("{:<257}", "{}").as_bytes());
    chunked.extend_from_slice(b"\r\n0\r\n\r\n");
    http.send(&chunked).await;
    let a = http.answer(false).await.expect("an answer");
    assert_eq!((a.status, a.text()), (413, too_large));
    assert!(http.closes_within(WAIT).await);
    assert_eq!(server.metrics.api_rejections(ApiRejection::BodyTooLarge), 2);

    // A head or a body that does not arrive in time closes the connection, unanswered.
    let started = Instant::now();
    let mut silent = Http::connect(server.api).await;
    let mut partial_head = Http::connect(server.api).await;
    partial_head
        .send(b"POST /v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\n")
        .await;
    let mut partial_body = Http::connect(server.api).await;
    partial_body
        .send(b"POST /v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 50\r\n\r\n{\"block\"")
        .await;
    for (what, http) in [
        ("silent", &mut silent),
        ("partial head", &mut partial_head),
        ("partial body", &mut partial_body),
    ] {
        assert!(http.closes_within(WAIT).await, "{what}");
    }
    assert!(started.elapsed() >= Duration::from_millis(350));
    // A kept-alive connection left idle is closed as well, which rejects nothing.
    let mut idle = Http::connect(server.api).await;
    assert_eq!(
        idle.post(GetRawBlock.path(), r#"{"block_num_or_id":3}"#)
            .await
            .status,
        200
    );
    assert!(idle.closes_within(WAIT).await);
    assert_eq!(server.metrics.api_rejections(ApiRejection::Timeout), 3);
}

/// Block 5 carries 6 MiB of transaction data: its raw body, the data in hex, is over 12 MiB,
/// more than the sockets between the server and a client that does not read can take.
fn bulky(n: u32) -> BlockSpec {
    let mut spec = rich(n);
    if n == 5 {
        spec.transactions = vec![Receipt {
            status: 0,
            cpu_usage_us: 100,
            net_usage_words: 10,
            trx: ReceiptTrx::Packed {
                signatures: vec![Signature::K1([5; 65])],
                compression: 0,
                packed_context_free_data: Vec::new(),
                packed_trx: vec![0xa5; 6 << 20],
            },
        }];
    }
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_left_unread_is_cut_off_and_gives_its_budget_back() {
    let chain = Chain::build(bulky, |_| {});
    let want = rendered(&chain.block(5).body, GetRawBlock, false);
    assert!(want.len() > 12 << 20, "{} bytes", want.len());
    let timeout = Duration::from_secs(1);
    let mut cfg = chain.config();
    api_cfg(&mut cfg).request_timeout = timeout;
    let server = start(cfg).await;
    let request = post_bytes(GetRawBlock.path(), &body_for(GetRawBlock, "5", false));

    // A client that pauses between reads, never for the whole timeout, gets the whole answer
    // however long that takes: the limit is on progress, not on the answer.
    let mut slow = Http::connect_small(server.api).await;
    let started = Instant::now();
    slow.send(&request).await;
    let mut chunk = vec![0; 64 << 10];
    let mut burst = 0;
    loop {
        if let Some(at) = slow.buf.windows(4).position(|w| w == b"\r\n\r\n")
            && slow.buf.len() >= at + 4 + want.len()
        {
            break;
        }
        let n = tokio::time::timeout(WAIT, slow.tcp.read(&mut chunk))
            .await
            .expect("more of the answer")
            .expect("a read");
        assert!(
            n > 0,
            "the connection of a client reading its answer was closed"
        );
        slow.buf.extend_from_slice(&chunk[..n]);
        burst += n;
        if burst >= 2 << 20 {
            burst = 0;
            tokio::time::sleep(timeout / 4).await;
        }
    }
    let a = slow.answer(false).await.expect("an answer");
    assert_eq!(a.status, 200);
    assert!(a.body == want, "{} bytes, not the block's", a.body.len());
    assert!(
        started.elapsed() > timeout,
        "read in {:?}, within the timeout",
        started.elapsed()
    );
    until("the answer read gives its budget back", || {
        server.budget.used() == 0
    })
    .await;

    drop(slow);

    // A client that stops reading is cut off once an answer has waited the whole timeout with
    // none of it taken, and the budget the answer held is given back. It asks several times at
    // once: some platforms take whole answers into the socket, whatever their size, before one
    // has to wait.
    let asks = 8;
    let mut stalled = Http::connect_small(server.api).await;
    let asked = Instant::now();
    stalled.send(&request.repeat(asks)).await;
    tokio::time::sleep(timeout / 2).await;
    assert!(
        server.budget.used() > 0,
        "an answer waiting on its client keeps its reservation"
    );
    until("the unread answer is cut off", || {
        server.metrics.api_rejections(ApiRejection::Timeout) == 1
    })
    .await;
    assert!(
        asked.elapsed() >= timeout,
        "cut off after {:?}",
        asked.elapsed()
    );
    until("the unread answer gives its budget back", || {
        server.budget.used() == 0
    })
    .await;
    // The connection is closed: the client reads what was sent before the cut, then the end.
    let mut rest = std::mem::take(&mut stalled.buf);
    let _ = tokio::time::timeout(WAIT, stalled.tcp.read_to_end(&mut rest))
        .await
        .expect("the connection is closed");
    assert!(rest.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(
        rest.len() < asks * want.len(),
        "{} bytes of {asks} {}-byte answers",
        rest.len(),
        want.len()
    );
}

// ---- S5 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_slots_and_a_full_budget_answer_busy() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    let api = api_cfg(&mut cfg);
    api.max_in_flight = 1;
    api.queue = Duration::from_millis(200);
    api.read_delay = Duration::from_millis(1500);
    let server = start(cfg).await;
    let (addr, slow_body) = (server.api, body_for(GetRawBlock, "5", false));
    let slow = tokio::spawn(async move { post(addr, GetRawBlock.path(), slow_body).await });
    until("the slow request takes the slot", || {
        server.metrics.api_in_flight() == 1
    })
    .await;

    // A block request waits its turn, then is answered 503.
    let started = Instant::now();
    let a = post(server.api, GetBlockInfo.path(), r#"{"block_num":6}"#).await;
    let waited = started.elapsed();
    assert_eq!(
        (a.status, a.text()),
        (503, busy("Too many requests in flight: 1").as_str())
    );
    assert!(
        waited >= Duration::from_millis(190) && waited < Duration::from_millis(1400),
        "answered after {waited:?}"
    );
    // What needs no slot is answered at once: a block outside the logs, the listings.
    let body = body_for(GetRawBlock, "1000", false);
    let a = post(server.api, GetRawBlock.path(), &body).await;
    assert_eq!((a.status, a.body), (400, unknown(GetRawBlock, &body)));
    let a = post(server.api, "/v1/archive/describe", "").await;
    assert!(
        a.text().ends_with(",\"max_in_flight\":1,\"in_flight\":1}"),
        "{}",
        a.text()
    );
    let done = slow.await.unwrap();
    assert_eq!(
        (done.status, done.body),
        (200, rendered(&chain.block(5).body, GetRawBlock, false))
    );
    assert_eq!(server.metrics.api_rejections(ApiRejection::InFlight), 1);
    until("the slot is free", || server.metrics.api_in_flight() == 0).await;
    drop(server);

    // A budget that cannot hold the block and its body answers 503 without reading it.
    let mut cfg = chain.config();
    cfg.memory_budget = 1024;
    let server = start(cfg).await;
    for (ep, ext) in MODES {
        let a = post(server.api, ep.path(), body_for(ep, "6", ext)).await;
        assert_eq!(
            (a.status, a.text()),
            (503, busy("Too many bytes in flight: 0").as_str())
        );
    }
    assert_eq!(server.metrics.api_rejections(ApiRejection::Memory), 4);
    assert_eq!(server.budget.used(), 0);
}

/// Block 5 holds 4000 deferred (id-only) receipts: its raw body is almost four times its size,
/// more than the first reservation holds.
fn heavy(n: u32) -> BlockSpec {
    let mut spec = rich(n);
    if n == 5 {
        spec.transactions = (0..4000u32)
            .map(|i| Receipt {
                status: 2,
                cpu_usage_us: 4_000_000_000 + i,
                net_usage_words: 0,
                trx: ReceiptTrx::Id([i as u8; 32]),
            })
            .collect();
    }
    spec
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_larger_than_its_first_reservation_is_rendered_again() {
    let chain = Chain::build(heavy, |_| {});
    let block = &chain.block(5).body;
    let want = rendered(block, GetRawBlock, false);
    let len = block.len();
    assert!(
        want.len() > 3 * len + (64 << 10),
        "{} bytes rendered from {len}",
        want.len()
    );
    let server = start(chain.config()).await;
    let a = post(server.api, GetRawBlock.path(), r#"{"block_num_or_id":5}"#).await;
    assert_eq!((a.status, &a.body), (200, &want));
    let a = post(
        server.api,
        GetBlockHeader.path(),
        body_for(GetBlockHeader, "5", true),
    )
    .await;
    assert_eq!(a.body, rendered(block, GetBlockHeader, true));
    assert_eq!(server.budget.used(), 0);
    drop(server);

    // A budget holding the first reservation but not the second answers 503.
    let mut cfg = chain.config();
    cfg.memory_budget = 1 << 20;
    let server = start(cfg).await;
    let a = post(server.api, GetRawBlock.path(), r#"{"block_num_or_id":5}"#).await;
    let first = 4 * len as u64 + (64 << 10);
    let want = busy(&format!("Too many bytes in flight: {first}"));
    assert_eq!((a.status, a.text()), (503, want.as_str()));
    assert_eq!(server.budget.used(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_bodies_past_16_kib_are_held_within_the_memory_budget() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    cfg.memory_budget = 1 << 20;
    let server = start(cfg).await;
    let head = format!(
        "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n",
        GetRawBlock.path(),
        2 << 20
    );

    // A body on its way holds room to the next power of two of what has arrived; all of that
    // room but the first 16 KiB is reserved.
    let mut slow = Http::connect(server.api).await;
    slow.send(head.as_bytes()).await;
    slow.send(&vec![b' '; 600 << 10]).await;
    let held = (1 << 20) - (16 << 10);
    until("the body's room is reserved", || {
        server.budget.used() == held
    })
    .await;

    // A request whose body fits in the unreserved part is answered with the budget full.
    let a = post(server.api, "/v1/node/get_supported_apis", "").await;
    assert_eq!((a.status, a.text()), (200, SUPPORTED));
    // A body needing more room than the budget has left is refused once it does, and its
    // connection is closed.
    let mut refused = Http::connect(server.api).await;
    refused.send(head.as_bytes()).await;
    refused.send(&vec![b' '; (32 << 10) + 1]).await;
    let a = refused.answer(false).await.expect("an answer");
    let used: u64 = a
        .text()
        .split("Too many bytes in flight: ")
        .nth(1)
        .and_then(|t| t.split('"').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{}", a.text()));
    let want = busy(&format!("Too many bytes in flight: {used}"));
    assert_eq!((a.status, a.text()), (503, want.as_str()));
    // What the stalled body holds, and what of its own room the refused one held then.
    assert!((held..=1 << 20).contains(&used), "{used}");
    assert_eq!(a.header("connection"), Some("close"));
    assert!(refused.closes_within(WAIT).await);
    assert_eq!(server.metrics.api_rejections(ApiRejection::Memory), 1);
    assert_eq!(server.budget.used(), held);
    drop(slow);
    until("the first body gives its budget back", || {
        server.budget.used() == 0
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_over_the_cap_are_answered_busy_and_closed() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    api_cfg(&mut cfg).max_connections = 2;
    let server = start(cfg).await;
    let body = body_for(GetRawBlock, "3", false);
    let mut first = Http::connect(server.api).await;
    let mut second = Http::connect(server.api).await;
    for http in [&mut first, &mut second] {
        assert_eq!(http.post(GetRawBlock.path(), &body).await.status, 200);
    }

    // One more connection is answered at once, before it sends anything, and closed.
    let mut over = Http::connect(server.api).await;
    let a = over.answer(false).await.expect("a refusal");
    let want = busy("Too many connections: 2");
    assert_eq!((a.status, a.text()), (503, want.as_str()));
    assert_eq!(
        a.headers(),
        [
            "Connection: close".to_owned(),
            format!("Content-Length: {}", want.len()),
            "Content-Type: application/json".to_owned(),
            format!("Server: {SERVER_HEADER}"),
        ]
    );
    assert!(over.closes_within(WAIT).await);
    assert_eq!(server.metrics.api_rejections(ApiRejection::Connections), 1);

    // The open connections are still served, and once one closes its place is free again.
    assert_eq!(first.post(GetRawBlock.path(), &body).await.status, 200);
    drop(second);
    let deadline = Instant::now() + WAIT;
    let mut next = loop {
        let mut http = Http::connect(server.api).await;
        // A refusal comes unasked; an accepted connection waits for its request.
        if tokio::time::timeout(QUIET, http.read_more()).await.is_err() {
            break http;
        }
        assert!(
            Instant::now() < deadline,
            "the closed connection's place stays taken"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(next.post(GetRawBlock.path(), &body).await.status, 200);
    drop(server);

    let mut cfg = chain.config();
    api_cfg(&mut cfg).max_connections = 0;
    let e = Server::bind(cfg).await.err().expect("refused");
    assert_eq!(e.to_string(), "--api-max-connections must be at least 1");
}

// ---- S6 ----

/// The log lines of this thread, at WARN and above.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl io::Write for Logs {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn lines(&self) -> Vec<String> {
        let text = String::from_utf8(self.0.lock().unwrap().clone()).unwrap();
        text.lines().map(str::to_owned).collect()
    }
}

// One thread runs the server and the test, so the log lines of the requests are this thread's.
#[tokio::test]
async fn a_block_that_does_not_decode_or_read_answers_busy() {
    let logs = Logs::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer({
            let logs = logs.clone();
            move || logs.clone()
        })
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);

    // Block 10 is stored with a byte after its block extensions; its header, and so its id,
    // is intact.
    let chain = Chain::build(rich, |blocks| blocks[9].body.push(0));
    let mut cfg = chain.config();
    cfg.http = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let server = start(cfg).await;
    let invalid = busy("Block data failed validation");
    for (ep, ext) in MODES {
        let a = post(server.api, ep.path(), body_for(ep, "10", ext)).await;
        assert_eq!(
            (a.status, a.text()),
            (503, invalid.as_str()),
            "{}",
            ep.name()
        );
    }
    assert_eq!(server.metrics.api_failures(ApiFailure::Integrity), 4);
    let warnings = logs.lines();
    assert_eq!(warnings.len(), 4, "{warnings:#?}");
    assert!(
        warnings
            .iter()
            .all(|l| l.contains("WARN") && l.contains("block=10"))
    );
    // The next request is served.
    let a = post(server.api, GetRawBlock.path(), r#"{"block_num_or_id":11}"#).await;
    assert_eq!(a.body, rendered(&chain.block(11).body, GetRawBlock, false));

    // The block-log file holding 101 to 120 loses its end while being served.
    let file = chain.tmp.path().join("blocks").join("blocks-101-120.log");
    let len = std::fs::metadata(&file).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&file)
        .unwrap()
        .set_len(len - 100)
        .unwrap();
    let a = post(
        server.api,
        GetRawBlock.path(),
        format!("{{\"block_num_or_id\":{LAST}}}"),
    )
    .await;
    assert_eq!(
        (a.status, a.text()),
        (503, busy("Block read failed").as_str())
    );
    assert_eq!(server.metrics.api_failures(ApiFailure::Read), 1);
    assert_eq!(logs.lines().len(), 5);
    let a = post(server.api, GetRawBlock.path(), r#"{"block_num_or_id":100}"#).await;
    assert_eq!(a.body, rendered(&chain.block(100).body, GetRawBlock, false));
    let (_, m) = ops(server.http.unwrap(), "/metrics").await;
    for (reason, count) in [("read", 1.0), ("integrity", 4.0), ("panic", 0.0)] {
        let series = format!("{P}api_failures_total{{reason=\"{reason}\"}}");
        assert_eq!(sample(&m, &series), count, "{reason}");
    }
    assert_eq!(server.budget.used(), 0);
}

// ---- S7 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_api_is_refused_without_block_logs_or_a_listener_of_its_own() {
    let chain = Chain::new();
    let refusal = |cfg: ServeConfig| async move {
        match Server::bind(cfg).await {
            Ok(_) => panic!("the server started"),
            Err(e) => format!("{e:#}"),
        }
    };
    let mut cfg = chain.config();
    cfg.store.blocks = BlocksMode::None;
    cfg.store.chain_id = Some(Genesis::eos_mainnet().chain_id());
    assert!(refusal(cfg).await.contains("--blocks none"));

    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let at = |ip: [u8; 4]| SocketAddr::from((ip, port));
    for (listen, api) in [
        (at([127, 0, 0, 1]), at([127, 0, 0, 1])),
        (at([0, 0, 0, 0]), at([127, 0, 0, 1])),
        (at([127, 0, 0, 1]), at([0, 0, 0, 0])),
    ] {
        let mut cfg = chain.config();
        cfg.listen = listen;
        api_cfg(&mut cfg).listen = api;
        assert!(
            refusal(cfg).await.contains("is the --listen address"),
            "{listen} {api}"
        );
    }
    let mut cfg = chain.config();
    cfg.http = Some(at([127, 0, 0, 1]));
    api_cfg(&mut cfg).listen = at([127, 0, 0, 1]);
    assert!(refusal(cfg).await.contains("is the --http address"));

    let mut cfg = chain.config();
    api_cfg(&mut cfg).max_in_flight = 0;
    assert!(refusal(cfg).await.contains("--api-max-in-flight"));
    let mut cfg = chain.config();
    api_cfg(&mut cfg).max_body_bytes = 63;
    assert!(refusal(cfg).await.contains("--api-max-body-bytes"));
}

// ---- S8 ----

/// A SHiP client that has read the ABI.
async fn ship(addr: SocketAddr) -> Ws {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/"))
        .await
        .expect("connect");
    match recv(&mut ws).await {
        Message::Text(_) => ws,
        other => panic!("the first message is not the ABI text: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ship_sessions_stream_as_before_while_api_clients_hammer() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let mut cfg = with_api(fx.serve());
    cfg.io_threads = 4;
    cfg.memory_budget = 32 << 20;
    api_cfg(&mut cfg).max_in_flight = 2;
    api_cfg(&mut cfg).queue = WAIT;
    let server = start(cfg).await;
    let expected: Arc<Vec<Vec<u8>>> = Arc::new(
        (2..201)
            .map(|n| rendered(&fx.chain.block(n).body, GetRawBlock, false))
            .collect(),
    );
    let mut clients = Vec::new();
    for c in 0..16u32 {
        let (addr, expected) = (server.api, Arc::clone(&expected));
        clients.push(tokio::spawn(async move {
            let mut http = Http::connect(addr).await;
            for i in 0..40u32 {
                let n = 2 + (c * 37 + i * 11) % 199;
                let body = format!("{{\"block_num_or_id\":{n}}}");
                let a = http.post(GetRawBlock.path(), body).await;
                assert_eq!(a.status, 200, "block {n}: {}", a.text());
                assert_eq!(a.body, expected[(n - 2) as usize], "block {n}");
            }
        }));
    }
    for v1 in [false, true] {
        let mut ws = ship(server.ship).await;
        let mut req = fetch_all(blocks(2, 201, 1000));
        req.v1 = v1;
        send(&mut ws, &ShipRequest::Blocks(req.clone())).await;
        for n in 2..201 {
            assert_eq!(
                recv_binary(&mut ws).await,
                model.result(n, &req),
                "block {n}"
            );
        }
    }
    for client in clients {
        client.await.expect("an API client");
    }
    assert_eq!(
        server.metrics.api_requests(ApiRoute::GetRawBlock, 200),
        16 * 40
    );
    assert_eq!(server.metrics.results(), 2 * 199);
    until("every reservation is given back", || {
        server.budget.used() == 0
    })
    .await;
}

// ---- S9 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_lets_requests_in_flight_finish() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    api_cfg(&mut cfg).read_delay = Duration::from_millis(500);
    let server = start(cfg).await;
    let addr = server.api;
    let mut idle = Http::connect(addr).await;
    assert_eq!(
        idle.post("/v1/node/get_supported_apis", "").await.status,
        200
    );
    let mut busy = Http::connect(addr).await;
    busy.send(
        format!(
            "POST {} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 21\r\n\r\n{{\"block_num_or_id\":7}}",
            GetRawBlock.path()
        )
        .as_bytes(),
    )
    .await;
    until("the request is in flight", || {
        server.metrics.api_in_flight() == 1
    })
    .await;
    let stopping = tokio::spawn(server.stop());
    // The idle connection is closed at once; the busy one gets its answer, then is closed.
    assert!(idle.closes_within(WAIT).await);
    let a = busy.answer(false).await.expect("the answer in flight");
    assert_eq!(
        (a.status, a.body),
        (200, rendered(&chain.block(7).body, GetRawBlock, false))
    );
    assert!(busy.closes_within(WAIT).await);
    stopping.await.unwrap().unwrap();
    // Nothing listens any more.
    match tokio::time::timeout(WAIT, TcpStream::connect(addr)).await {
        Ok(Err(_)) => {}
        Ok(Ok(tcp)) => {
            let mut http = Http {
                tcp,
                buf: Vec::new(),
            };
            assert!(
                http.closes_within(WAIT).await,
                "a connection after shutdown"
            );
        }
        Err(_) => panic!("connecting after shutdown hung"),
    }
}

// ---- S10 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metrics_and_health_report_the_chain_api() {
    let chain = Chain::new();
    let mut cfg = chain.config();
    cfg.http = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    api_cfg(&mut cfg).max_body_bytes = 128;
    api_cfg(&mut cfg).max_in_flight = 3;
    let server = start(cfg).await;
    let ops_addr = server.http.expect("--http is bound");
    let (_, m) = ops(ops_addr, "/metrics").await;
    for series in [
        "api_requests_total",
        "api_request_seconds",
        "api_in_flight",
        "api_rejected_total",
        "api_failures_total",
        "api_response_bytes_total",
    ] {
        assert!(m.contains(&format!("# TYPE {P}{series} ")), "{series}");
    }
    assert!(
        !m.contains(&format!("{P}api_requests_total{{")),
        "nothing counted yet"
    );

    let mut http = Http::connect(server.api).await;
    let mut bytes = 0;
    for (target, body, status) in [
        (GetRawBlock.path(), r#"{"block_num_or_id":4}"#, 200),
        (GetRawBlock.path(), r#"{"block_num_or_id":"5"}"#, 200),
        (GetRawBlock.path(), r#"{"block_num_or_id":0}"#, 400),
        (GetBlockInfo.path(), r#"{"block_num":4}"#, 200),
        (GetBlockHeader.path(), r#"{"block_num_or_id":"z"}"#, 500),
        ("/v1/chain/get_block", r#"{"block_num_or_id":4}"#, 404),
        ("/v1/chain/get_info", "", 404),
        ("/v1/node/get_supported_apis", "", 200),
        ("/v1/archive/describe", "", 200),
    ] {
        let a = http.post(target, body).await;
        assert_eq!(a.status, status, "{target} {body}");
        bytes += a.body.len();
    }
    let a = http.request("OPTIONS", GetRawBlock.path(), b"").await;
    bytes += a.body.len();
    let mut big = Http::connect(server.api).await;
    let a = big.post(GetRawBlock.path(), " ".repeat(129)).await;
    assert_eq!(a.status, 413);
    bytes += a.body.len();

    let (_, m) = ops(ops_addr, "/metrics").await;
    let at = |series: &str| sample(&m, &format!("{P}{series}"));
    let requests = [
        ("get_raw_block", "200", 2.0),
        ("get_raw_block", "400", 1.0),
        ("get_raw_block", "413", 1.0),
        ("get_block_info", "200", 1.0),
        ("get_block_header", "500", 1.0),
        ("get_block", "404", 1.0),
        ("get_supported_apis", "200", 1.0),
        ("describe", "200", 1.0),
        ("other", "200", 1.0),
        ("other", "404", 1.0),
    ];
    for (endpoint, code, count) in requests {
        let series = format!("api_requests_total{{endpoint=\"{endpoint}\",code=\"{code}\"}}");
        assert_eq!(at(&series), count, "{series}");
    }
    let counted = m
        .lines()
        .filter(|l| l.starts_with(&format!("{P}api_requests_total{{")))
        .count();
    assert_eq!(counted, requests.len(), "{m}");
    for (endpoint, count) in [
        ("get_raw_block", 4.0),
        ("get_block_info", 1.0),
        ("get_block_header", 1.0),
        ("get_block", 1.0),
        ("get_supported_apis", 1.0),
        ("describe", 1.0),
        ("other", 2.0),
    ] {
        assert_eq!(
            at(&format!(
                "api_request_seconds_count{{endpoint=\"{endpoint}\"}}"
            )),
            count,
            "{endpoint}"
        );
    }
    assert!(at("api_request_seconds_sum{endpoint=\"get_raw_block\"}") > 0.0);
    assert_eq!(at("api_in_flight"), 0.0);
    for (reason, count) in [
        ("in_flight", 0.0),
        ("memory", 0.0),
        ("body_too_large", 1.0),
        ("timeout", 0.0),
    ] {
        assert_eq!(
            at(&format!("api_rejected_total{{reason=\"{reason}\"}}")),
            count
        );
    }
    for reason in ["read", "integrity", "panic"] {
        assert_eq!(
            at(&format!("api_failures_total{{reason=\"{reason}\"}}")),
            0.0
        );
    }
    assert_eq!(at("api_response_bytes_total"), bytes as f64);
    // Every block request read its block on an io thread.
    assert!(at("read_seconds_count") >= 3.0);

    let (code, body) = ops(ops_addr, "/health").await;
    assert_eq!(code, 200);
    let health: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        health["api"],
        json!({
            "listen": server.api.to_string(),
            "get_block": "off",
            "in_flight": 0,
            "max_in_flight": 3,
        })
    );
    drop(server);

    // Without --api, /health has no `api` block.
    let mut cfg = serve_config(chain.store());
    cfg.http = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let server = Server::bind(cfg).await.unwrap();
    assert!(server.api_addr().is_none());
    let ops_addr = server.http_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async move {
        let _ = stopped.await;
    }));
    let (_, body) = ops(ops_addr, "/health").await;
    let health: Value = serde_json::from_str(&body).unwrap();
    assert!(health.get("api").is_none(), "{health}");
    drop(stop);
    task.await.unwrap().unwrap();
}
