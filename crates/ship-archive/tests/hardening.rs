//! What a long-running server adds to the Spring session: the `--http` endpoints and their
//! metrics, and results built ahead of the writers within `--prefetch`, the memory budget and
//! the io threads, which must never change what is sent.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use common::*;
use hyperion_ship::protocol::Request;
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::ship::Encoding;
use hyperion_ship::testkit::{FileName, SyntheticChain};
use serde_json::{Value, json};
use ship_archive::ServeConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Error as WsError;

/// One request to the HTTP endpoints: the status code and the body.
async fn http(addr: SocketAddr, method: &str, path: &str) -> (u16, String) {
    let mut tcp = TcpStream::connect(addr).await.expect("connect to --http");
    let request = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
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

/// The value of `series` (name and labels) in a `/metrics` body.
fn sample(metrics: &str, series: &str) -> f64 {
    metrics
        .lines()
        .find_map(|l| l.strip_prefix(series)?.strip_prefix(' '))
        .unwrap_or_else(|| panic!("no {series} in\n{metrics}"))
        .parse()
        .unwrap()
}

/// Poll `/metrics` until `done` holds; its last body.
async fn metrics_until(addr: SocketAddr, done: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + WAIT;
    loop {
        let (code, body) = http(addr, "GET", "/metrics").await;
        assert_eq!(code, 200);
        if done(&body) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "the metrics never got there:\n{body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

const P: &str = "antelope_ship_archive_";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_endpoints_report_health_readiness_and_metrics() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let mut cfg = fx.serve();
    cfg.http = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    cfg.max_sessions = 1;
    cfg.memory_budget = 64 << 20;
    let server = start(cfg).await;
    let addr = server.http.expect("--http is bound");

    assert_eq!(http(addr, "GET", "/ready").await, (200, "ready\n".into()));
    assert_eq!(http(addr, "GET", "/ready?probe=1").await.0, 200);
    assert_eq!(http(addr, "HEAD", "/ready").await, (200, String::new()));
    assert_eq!(http(addr, "GET", "/nothing").await.0, 404);
    assert_eq!(http(addr, "POST", "/health").await.0, 405);

    let (code, body) = http(addr, "GET", "/health").await;
    assert_eq!(code, 200);
    let health: Value = serde_json::from_str(&body).expect("JSON");
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(health["chain_id"], hex::encode(fx.chain.chain_id()));
    assert_eq!(
        health["served"],
        json!({
            "trace": { "begin": 2, "end": 201 },
            "chain_state": { "begin": 2, "end": 201 },
            "finality": { "begin": 100, "end": 201 },
            "blocks": { "begin": 2, "end": 201 },
        })
    );
    assert_eq!(
        health["last"],
        json!({ "num": 200, "id": hex::encode(fx.chain.id(200)) })
    );
    assert_eq!(health["sessions"], 0);
    let findings = health["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1);
    assert_eq!(
        (&findings[0]["kind"], &findings[0]["warning"]),
        (&json!("empty"), &json!(false))
    );

    // One session: two status requests and three results; a second session is refused.
    let mut ws = connect(&server).await;
    let mut bytes = 0;
    for v1 in [false, true] {
        bytes += status(&mut ws, v1).await.len();
    }
    let mut req = fetch_all(blocks(98, 101, 10));
    req.v1 = true;
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 98..101 {
        let msg = recv_binary(&mut ws).await;
        assert_eq!(msg, model.result(n, &req));
        bytes += msg.len();
    }
    match connect_async(server.url()).await {
        Err(WsError::Http(r)) => assert_eq!(r.status(), 503),
        other => panic!("expected HTTP 503, got {other:?}"),
    }
    let m = metrics_until(addr, |m| {
        m.contains(&format!("{P}results_sent_total 3\n"))
            && m.contains(&format!("{P}sessions_rejected_total 1\n"))
    })
    .await;
    assert!(
        m.contains(&format!("# TYPE {P}read_seconds summary\n")),
        "{m}"
    );
    let at = |series: &str| sample(&m, &format!("{P}{series}"));
    assert_eq!(at("sessions"), 1.0);
    assert_eq!(at("status_requests_total{version=\"v0\"}"), 1.0);
    assert_eq!(at("status_requests_total{version=\"v1\"}"), 1.0);
    assert_eq!(at("requests_total{version=\"v0\"}"), 0.0);
    assert_eq!(at("requests_total{version=\"v1\"}"), 1.0);
    assert_eq!(at("bytes_sent_total"), bytes as f64);
    for log in ["trace", "chain_state", "finality", "blocks"] {
        assert_eq!(at(&format!("read_errors_total{{log=\"{log}\"}}")), 0.0);
    }
    assert!(at("read_seconds_count") >= 3.0);
    assert!(at("read_seconds_sum") > 0.0);
    let (used, free) = (
        at("memory_budget_bytes{state=\"used\"}"),
        at("memory_budget_bytes{state=\"free\"}"),
    );
    assert_eq!(used + free, (64u64 << 20) as f64);
    let served: Vec<(f64, f64)> = ["trace", "chain_state", "finality", "blocks"]
        .iter()
        .map(|log| {
            (
                at(&format!("served_begin_block{{log=\"{log}\"}}")),
                at(&format!("served_end_block{{log=\"{log}\"}}")),
            )
        })
        .collect();
    assert_eq!(
        served,
        [(2.0, 201.0), (2.0, 201.0), (100.0, 201.0), (2.0, 201.0)]
    );
    let (_, body) = http(addr, "GET", "/health").await;
    assert_eq!(serde_json::from_str::<Value>(&body).unwrap()["sessions"], 1);

    // While the sessions close at shutdown the server is no longer ready. The client never
    // answers the Close, so the session lingers for its close wait.
    let stopping = tokio::spawn(server.stop());
    let deadline = Instant::now() + WAIT;
    loop {
        match http(addr, "GET", "/ready").await {
            (503, body) => {
                assert_eq!(body, "shutting down\n");
                break;
            }
            (200, _) => {
                assert!(Instant::now() < deadline, "/ready never turned 503");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    stopping.await.unwrap().unwrap();
    drop(ws);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_errors_count_against_the_log_they_come_from() {
    let fx = Fixture::new(SyntheticChain::new(2, 20));
    let dir = fx.dir("node");
    let mut deltas = fx
        .chain
        .ship_log(LogKind::ChainState, 2..22, Encoding::SizedDeflate);
    deltas.corrupt_checksum(10);
    deltas.write(&dir, "chain_state_history").unwrap();
    fx.chain.write_blocks(&dir, 2..22, FileName::Head).unwrap();
    let mut cfg = fx.serve();
    cfg.http = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
    let server = start(cfg).await;
    let addr = server.http.unwrap();

    let mut raw = Raw::connect(server.addr).await;
    let mut req = blocks(8, 12, 10);
    req.fetch_deltas = true;
    raw.send_request(&Request::Blocks(req)).await;
    for _ in 8..10 {
        raw.message().await.expect("an intact result");
    }
    assert_eq!(raw.frame().await, None, "dropped before block 10");
    let m = metrics_until(addr, |m| m.contains(&format!("{P}sessions 0\n"))).await;
    let at = |log: &str| sample(&m, &format!("{P}read_errors_total{{log=\"{log}\"}}"));
    assert_eq!(
        ["trace", "chain_state", "finality", "blocks"].map(at),
        [0.0, 1.0, 0.0, 0.0]
    );
    assert_eq!(sample(&m, &format!("{P}results_sent_total")), 2.0);
}

/// Blocks 2..101 with payloads large enough to matter: 24 KiB traces (Spring stored), 20 KiB
/// deltas (legacy framing, no declared size), 8 KiB finality data (Leap deflate), and three
/// entries above a 64 KiB stream threshold.
fn ahead_fixture() -> Fixture {
    let mut chain = SyntheticChain::new(2, 99);
    for n in 2..101 {
        chain
            .set_payload_len(LogKind::Trace, n, 24 << 10)
            .set_payload_len(LogKind::ChainState, n, 20 << 10)
            .set_payload_len(LogKind::Finality, n, 8 << 10);
    }
    chain
        .set_payload_len(LogKind::Trace, 10, 100 << 10)
        .set_payload_len(LogKind::Trace, 50, 200 << 10)
        .set_payload_len(LogKind::ChainState, 30, 90 << 10);
    let fx = Fixture::new(chain);
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..101, Encoding::SizedStored);
    fx.ship(&dir, LogKind::ChainState, 2..101, Encoding::LegacyZero);
    fx.ship(&dir, LogKind::Finality, 2..101, Encoding::SizedDeflate);
    fx.chain.write_blocks(&dir, 2..101, FileName::Head).unwrap();
    fx
}

/// Wait until the server has no session left.
async fn idle(server: &Running) {
    let deadline = Instant::now() + WAIT;
    while server.metrics.sessions() > 0 {
        assert!(Instant::now() < deadline, "a session never closed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn results_are_the_same_however_much_is_built_ahead() {
    let fx = ahead_fixture();
    let model = Model {
        chain: &fx.chain,
        trace: 2..101,
        chain_state: 2..101,
        finality: 2..101,
        blocks: 2..101,
    };
    // (prefetch, memory budget, io threads)
    let setups: [(usize, u64, usize); 5] = [
        (0, 1 << 30, 4),
        (8, 1 << 30, 4),
        // Nothing fits: every result is built when it is sent.
        (8, 1, 4),
        // Some fit.
        (8, 2 << 20, 2),
        // One io thread for everything, streams included.
        (8, 1 << 30, 1),
    ];
    for (prefetch, budget, io_threads) in setups {
        let what = format!("prefetch {prefetch}, budget {budget}, io threads {io_threads}");
        let cfg = ServeConfig {
            prefetch,
            memory_budget: budget,
            io_threads,
            stream_threshold: 64 << 10,
            ..fx.serve()
        };
        let server = start(cfg).await;
        let mut ws = connect(&server).await;
        let mut req = fetch_all(blocks(2, 101, 1000));
        req.v1 = true;
        send(&mut ws, &Request::Blocks(req.clone())).await;
        for n in 2..101 {
            assert_eq!(
                recv_binary(&mut ws).await,
                model.result(n, &req),
                "{what}: block {n}"
            );
        }
        drop(ws);
        idle(&server).await;
        let b = &server.budget;
        assert_eq!(b.used(), 0, "{what}: every reservation is given back");
        assert!(b.peak() <= budget, "{what}");
        match (prefetch, budget) {
            (0, _) => assert_eq!((b.peak(), b.refused()), (0, 0), "{what}"),
            (_, 1) => assert!(b.peak() == 0 && b.refused() > 0, "{what}"),
            (_, b2) if b2 == 1 << 30 => {
                assert!(b.peak() > 0 && b.refused() == 0, "{what}")
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nothing_is_built_beyond_the_credit_window() {
    let fx = ahead_fixture();
    let model = Model {
        chain: &fx.chain,
        trace: 2..101,
        chain_state: 2..101,
        finality: 2..101,
        blocks: 2..101,
    };
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;
    let mut req = blocks(60, 101, 5);
    req.fetch_traces = true;
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 60..65 {
        assert_eq!(recv_binary(&mut ws).await, model.result(n, &req));
    }
    quiet(&mut ws).await;
    // One read per result: the first built when it is sent, the next four ahead of it.
    assert_eq!(server.metrics.reads(), 5);
    assert_eq!(server.budget.used(), 0);

    send(&mut ws, &Request::Ack(3)).await;
    for n in 65..68 {
        assert_eq!(recv_binary(&mut ws).await, model.result(n, &req));
    }
    quiet(&mut ws).await;
    assert_eq!(server.metrics.reads(), 8);
    assert_eq!(server.budget.used(), 0);
}
