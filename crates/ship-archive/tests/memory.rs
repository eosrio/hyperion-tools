//! The memory bound under load: 64 sessions ask for large results and never read them, so each
//! one holds the result it is writing (mid-stream for some) while the results after it are
//! built ahead until the shared budget refuses more. Everything the process allocates from
//! before the server starts is counted by this test binary's global allocator, and its peak
//! must stay within `memory_budget + sessions × (3 × stream_threshold + max block + session
//! buffers)` ([`worst_case_bytes`]).
//!
//! The sessions build up to 16 results ahead each, with io threads enough for most of them to
//! start at once, so the budget is what stops them: the same load with a budget too large to
//! refuse anything holds more than the bound.
//!
//! The chain API (`--api`) holds its blocks and bodies within the same budget, so the bound
//! holds with API clients hammering it too, some of them never reading their answers.
//!
//! Each test measures the whole process, so they run one at a time.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use common::*;
use hyperion_ship::protocol::{self, Request};
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::SyntheticChain;
use hyperion_ship::testkit::blocks::{
    self as testblocks, BlockSpec, Genesis, Preamble, Receipt, ReceiptTrx, Signature,
};
use hyperion_ship::testkit::ship::{Encoding, LogBuilder};
use ship_archive::Server;
use ship_archive::api::ApiConfig;
use ship_archive::fetch::{FetchLimits, worst_case_bytes};
use ship_archive::metrics::ApiRoute;
use ship_archive::store::StoreConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::oneshot;

/// Held by each test while it runs.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The system allocator, counting the bytes allocated now and at most.
struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = NOW.fetch_add(by, Relaxed) + by;
    PEAK.fetch_max(now, Relaxed);
}

fn shrank(by: usize) {
    NOW.fetch_sub(by, Relaxed);
}

// SAFETY: every call goes to the system allocator unchanged; only the sizes are counted.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, forwarded.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, forwarded.
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, forwarded.
        unsafe { System.dealloc(ptr, layout) };
        shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, forwarded.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                shrank(layout.size() - new_size);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

const SESSIONS: usize = 64;
const THRESHOLD: u64 = 512 << 10;
/// Every payload of every block is just under the stream threshold, so a result holds three
/// of them in memory.
const INLINE: usize = 500 << 10;
/// Every fifth trace entry, from block 2, is streamed instead.
const STREAMED: usize = 3 << 20;
const BUDGET: u64 = 16 << 20;
const PREFETCH: usize = 16;
const FIRST: u32 = 2;
const END: u32 = 42;

/// Upgrade, read the response and the ABI message with a small fixed buffer, send `request`,
/// and never read again. The receive buffer is small, so the server's writes block early.
async fn stalled(addr: SocketAddr, request: &[u8]) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(8 << 10).unwrap();
    let mut tcp = socket.connect(addr).await.expect("connect");
    let upgrade = format!(
        "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    tcp.write_all(upgrade.as_bytes()).await.unwrap();
    let mut buf = [0u8; 4096];
    let mut have = 0;
    // The response head.
    let head_end = loop {
        let n = tcp.read(&mut buf[have..]).await.unwrap();
        assert!(n > 0, "the connection ended during the upgrade");
        have += n;
        if let Some(at) = buf[..have].windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        assert!(have < buf.len(), "an upgrade response over 4 KiB");
    };
    assert!(buf.starts_with(b"HTTP/1.1 101"), "the upgrade was refused");
    buf.copy_within(head_end..have, 0);
    have -= head_end;
    // The ABI frame: its header, then its payload, read and dropped.
    while have < 4 {
        have += tcp.read(&mut buf[have..]).await.unwrap();
    }
    assert_eq!(buf[0], 0x81, "the ABI is one final text frame");
    let (len, header) = match buf[1] {
        126 => (u64::from(u16::from_be_bytes([buf[2], buf[3]])), 4),
        127 => {
            while have < 10 {
                have += tcp.read(&mut buf[have..]).await.unwrap();
            }
            (u64::from_be_bytes(buf[2..10].try_into().unwrap()), 10)
        }
        n => (u64::from(n), 2),
    };
    let mut left = len - (have as u64 - header as u64).min(len);
    while left > 0 {
        let n = tcp.read(&mut buf).await.unwrap();
        assert!(n > 0, "the connection ended inside the ABI");
        left = left.saturating_sub(n as u64);
    }
    tcp.write_all(request).await.unwrap();
    tcp
}

#[test]
fn sixty_four_sessions_of_large_results_stay_within_the_memory_bound() {
    let _serial = serial();
    let mut chain = SyntheticChain::new(FIRST, END - FIRST);
    for n in FIRST..END {
        for kind in LogKind::ALL {
            chain.set_payload_len(kind, n, INLINE);
        }
        if n % 5 == 2 {
            chain.set_payload_len(LogKind::Trace, n, STREAMED);
        }
    }
    let fx = Fixture::new(chain);
    let dir = fx.dir("node");
    fx.chain
        .write_node(&dir, &LogKind::ALL, Encoding::SizedStored)
        .unwrap();
    let max_block = (FIRST..END)
        .map(|n| fx.chain.block(n).body.len() as u64)
        .max()
        .unwrap();
    let mut cfg = fx.serve();
    cfg.max_sessions = SESSIONS;
    cfg.stream_threshold = THRESHOLD;
    cfg.memory_budget = BUDGET;
    cfg.prefetch = PREFETCH;
    cfg.io_threads = 64;
    let limits = FetchLimits {
        stream_threshold: THRESHOLD,
    };
    let bound = worst_case_bytes(BUDGET, SESSIONS, &limits, max_block);
    // What building ahead without a budget would hold: `--prefetch` results per session.
    let unbudgeted = SESSIONS as u64 * PREFETCH as u64 * 3 * INLINE as u64;
    assert!(unbudgeted > bound);

    // Counted from here: the runtime, the server, the clients.
    let base = NOW.load(Relaxed);
    PEAK.store(base, Relaxed);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let (results, refused, budget_peak) = runtime.block_on(async {
        let server = start(cfg).await;
        let mut clients = Vec::new();
        for i in 0..SESSIONS as u32 {
            // Starts at every block from 2 to 11: some on a streamed entry, most not.
            let mut req = fetch_all(blocks(FIRST + i % 10, END, 1000));
            req.v1 = true;
            let request = masked(&protocol::encode_request(&Request::Blocks(req)));
            clients.push(stalled(server.addr, &request).await);
        }
        // Every session is open, and its reads (builds and streamed chunks) have stopped: each
        // writer waits on a client that does not read, and the budget refuses more.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut reads = server.metrics.reads();
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = server.metrics.reads();
            if server.metrics.sessions() == SESSIONS as u64
                && server.budget.refused() > 0
                && now == reads
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no steady state: {} sessions, {} refusals, reads {reads} then {now}",
                server.metrics.sessions(),
                server.budget.refused()
            );
            reads = now;
        }
        let seen = (
            server.metrics.results(),
            server.budget.refused(),
            server.budget.peak(),
        );
        drop(clients);
        let deadline = Instant::now() + WAIT;
        while server.metrics.sessions() > 0 {
            assert!(Instant::now() < deadline, "the sessions never closed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let budget = std::sync::Arc::clone(&server.budget);
        server.stop().await.unwrap();
        assert_eq!(budget.used(), 0, "every reservation is given back");
        seen
    });
    drop(runtime);
    let peak = (PEAK.load(Relaxed) - base) as u64;
    let mib = |b: u64| b as f64 / f64::from(1 << 20);
    eprintln!(
        "peak {:.1} MiB of a {:.1} MiB bound ({:.1} MiB unbudgeted); {results} results sent, \
         {refused} builds ahead refused, at most {:.1} MiB of the budget used",
        mib(peak),
        mib(bound),
        mib(unbudgeted),
        mib(budget_peak)
    );
    assert!(budget_peak > 0 && budget_peak <= BUDGET);
    assert!(
        peak <= bound,
        "the server held {peak} bytes, over the {bound}-byte bound"
    );
}

/// Blocks carrying 1 MiB of transaction data each: their `get_raw_block` bodies, the data in
/// hex, are over 2 MiB.
const API_BLOCK_DATA: usize = 1 << 20;
const API_BUDGET: u64 = 32 << 20;
const SHIP_SESSIONS: usize = 16;
/// API clients that send one `get_raw_block` and never read the answer.
const STALLED_API: usize = 6;
/// API clients asking in a loop, reading every answer.
const READING_API: usize = 8;
const API_ROUNDS: u32 = 5;
/// What one API connection holds outside the budget, at most: hyper's 16 KiB buffer, the
/// response head, the request, the connection task, and the client buffer of this test.
const API_CONNECTION: u64 = 128 << 10;

/// One HTTP request for raw block `n`.
fn raw_block_request(n: u32) -> Vec<u8> {
    let body = format!("{{\"block_num_or_id\":{n}}}");
    format!(
        "POST /v1/chain/get_raw_block HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\n\r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

/// Send `request` with a small receive buffer, and never read the answer.
async fn api_stalled(addr: SocketAddr, request: &[u8]) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(8 << 10).unwrap();
    let mut tcp = socket.connect(addr).await.expect("connect to --api");
    tcp.write_all(request).await.unwrap();
    tcp
}

/// Ask for each of `blocks` in turn on one connection, reading every answer through a 16 KiB
/// buffer and dropping it: the statuses.
async fn api_reader(addr: SocketAddr, blocks: Vec<u32>) -> Vec<u16> {
    let mut tcp = TcpStream::connect(addr).await.expect("connect to --api");
    let mut buf = vec![0u8; 16 << 10];
    let mut statuses = Vec::new();
    for n in blocks {
        tcp.write_all(&raw_block_request(n)).await.unwrap();
        let mut have = 0;
        let head_end = loop {
            let got = tcp.read(&mut buf[have..]).await.unwrap();
            assert!(got > 0, "the connection ended inside a head");
            have += got;
            if let Some(at) = buf[..have].windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
        };
        let head = std::str::from_utf8(&buf[..head_end]).expect("a text head");
        let status = head[9..12].parse().expect("a status");
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .expect("a Content-Length")
            .parse()
            .unwrap();
        let mut left = len - (have - head_end);
        while left > 0 {
            let got = tcp.read(&mut buf).await.unwrap();
            assert!(got > 0, "the connection ended inside a body");
            left -= got;
        }
        statuses.push(status);
    }
    statuses
}

#[test]
fn chain_api_requests_stay_within_the_memory_bound() {
    let _serial = serial();
    let tmp = tempfile::Builder::new()
        .prefix("ship-archive-memory-")
        .tempdir()
        .unwrap();
    let dir = tmp.path().to_path_buf();
    let chain = testblocks::extend(&testblocks::synthetic_id(FIRST - 1), END - FIRST, |n| {
        let mut spec = BlockSpec::synthetic(n);
        spec.transactions = vec![Receipt {
            status: 0,
            cpu_usage_us: 100,
            net_usage_words: 10,
            trx: ReceiptTrx::Packed {
                signatures: vec![Signature::default()],
                compression: 0,
                packed_context_free_data: Vec::new(),
                packed_trx: vec![n as u8; API_BLOCK_DATA],
            },
        }];
        spec
    })
    .unwrap();
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for b in &chain {
        trace.push(b.id, &vec![b.num as u8; INLINE]);
    }
    trace.write(&dir, "trace_history").unwrap();
    let preamble = Preamble::V3ChainId {
        first: FIRST,
        chain_id: Genesis::eos_mainnet().chain_id(),
    };
    testblocks::write(&dir, "blocks", &preamble, &chain).unwrap();
    let max_block = chain.iter().map(|b| b.body.len() as u64).max().unwrap();
    drop((chain, trace));

    let mut cfg = serve_config(StoreConfig {
        dirs: vec![dir],
        ..StoreConfig::default()
    });
    cfg.max_sessions = SHIP_SESSIONS;
    cfg.stream_threshold = THRESHOLD;
    cfg.memory_budget = API_BUDGET;
    cfg.prefetch = PREFETCH;
    cfg.io_threads = 32;
    let mut api = ApiConfig::new(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)), cfg.io_threads);
    api.max_in_flight = 8;
    api.queue = Duration::from_secs(5);
    cfg.api = Some(api);
    let limits = FetchLimits {
        stream_threshold: THRESHOLD,
    };
    let connections = (STALLED_API + READING_API) as u64;
    let bound = worst_case_bytes(API_BUDGET, SHIP_SESSIONS, &limits, max_block)
        + connections * API_CONNECTION;

    // Counted from here: the runtime, the server, the clients.
    let base = NOW.load(Relaxed);
    PEAK.store(base, Relaxed);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let (held, statuses, refused, budget_peak) = runtime.block_on(async {
        let server = Server::bind(cfg).await.expect("the server starts");
        let ship = server.local_addr();
        let api = server.api_addr().expect("--api is bound");
        let (metrics, budget) = (Arc::clone(server.metrics()), Arc::clone(server.budget()));
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(server.run(async move {
            let _ = stopped.await;
        }));

        // Answers nobody reads: what of them the socket does not take keeps its reservation
        // (how much it takes depends on the platform).
        let mut unread = Vec::new();
        for i in 0..STALLED_API as u32 {
            unread.push(api_stalled(api, &raw_block_request(FIRST + i)).await);
        }
        let deadline = Instant::now() + WAIT;
        while metrics.api_requests(ApiRoute::GetRawBlock, 200) < STALLED_API as u64 {
            assert!(
                Instant::now() < deadline,
                "the unread requests were never answered"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let held = budget.used();

        // SHiP sessions that never read build ahead into what is left of the budget, while
        // API clients read answers or are refused for want of it.
        let mut sessions = Vec::new();
        for i in 0..SHIP_SESSIONS as u32 {
            let mut req = fetch_all(blocks(FIRST + i % 10, END, 1000));
            req.v1 = true;
            let request = masked(&protocol::encode_request(&Request::Blocks(req)));
            sessions.push(stalled(ship, &request).await);
        }
        let readers: Vec<_> = (0..READING_API as u32)
            .map(|c| {
                let asks = (0..API_ROUNDS)
                    .map(|i| FIRST + (c * 7 + i * 3) % (END - FIRST))
                    .collect();
                tokio::spawn(api_reader(api, asks))
            })
            .collect();
        let mut statuses = Vec::new();
        for reader in readers {
            statuses.extend(reader.await.expect("an API reader"));
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut reads = metrics.reads();
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let now = metrics.reads();
            if metrics.sessions() == SHIP_SESSIONS as u64 && budget.refused() > 0 && now == reads {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "no steady state: {} sessions, {} refusals, reads {reads} then {now}",
                metrics.sessions(),
                budget.refused()
            );
            reads = now;
        }
        let seen = (held, statuses, budget.refused(), budget.peak());
        drop((sessions, unread));
        let deadline = Instant::now() + WAIT;
        while metrics.sessions() > 0 || budget.used() > 0 {
            assert!(
                Instant::now() < deadline,
                "{} sessions and {} bytes of the budget left",
                metrics.sessions(),
                budget.used()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(stop);
        task.await.unwrap().unwrap();
        seen
    });
    drop(runtime);
    let peak = (PEAK.load(Relaxed) - base) as u64;
    let mib = |b: u64| b as f64 / f64::from(1 << 20);
    let answered = statuses.iter().filter(|&&s| s == 200).count();
    eprintln!(
        "peak {:.1} MiB of a {:.1} MiB bound; unread answers held {:.1} MiB of the budget; \
         {answered} of {} reading requests answered, the rest refused; {refused} reservations \
         refused, at most {:.1} MiB of the budget used",
        mib(peak),
        mib(bound),
        mib(held),
        statuses.len(),
        mib(budget_peak)
    );
    assert!(
        statuses.iter().all(|&s| s == 200 || s == 503),
        "{statuses:?}"
    );
    assert!(budget_peak > 0 && budget_peak <= API_BUDGET);
    assert!(
        peak <= bound,
        "the server held {peak} bytes, over the {bound}-byte bound"
    );
}
