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

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::{Duration, Instant};

use common::*;
use hyperion_ship::protocol::{self, Request};
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::SyntheticChain;
use hyperion_ship::testkit::ship::Encoding;
use ship_archive::fetch::{FetchLimits, worst_case_bytes};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};

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
