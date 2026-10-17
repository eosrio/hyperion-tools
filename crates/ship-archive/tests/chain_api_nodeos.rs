//! The chain API (`--api`) against nodeos's own answers, over TCP on loopback, byte for byte.
//!
//!   * The committed goldens (`crates/core/testdata/chain-api/dev-chain`, raw HTTP exchanges with
//!     a Spring 1.2.2 dev chain): every request of `http/`, and every request of `requests/` that
//!     nodeos refused, is sent to a server over a synthetic chain, and the whole answer must be
//!     nodeos's (status line, headers in their order, body) but for the value of `Server`. These
//!     answers hold nothing of the chain: they come before any handler, or name a block that is
//!     never served, or refuse the request. The one exception is a request without a `Host`,
//!     which nodeos refuses and this server, checking no `Host`, answers as it answers the same
//!     request with one.
//!   * A node's own answers, ignored: a server over a copy of a Spring 1.1+ node's logs answers
//!     every request that node answered, and each answer must be the node's but for `Server`,
//!     apart from the deviations [`deviation`] names, which are counted and printed; and at least
//!     one block's answer of each endpoint (and `get_block_header` mode) must be byte-equal, so a
//!     capture of no block fails. Nothing of that data is committed. `CHAIN_API_NODEOS_CAPTURES`
//!     names a directory of raw `<case>.req` and `<case>.resp` pairs (searched recursively), such
//!     as `scripts/capture_chain_api_goldens.sh` writes, given a `--block` for each block to
//!     compare (a sample across the logs, and the blocks with extensions);
//!     `CHAIN_API_NODEOS_DATA` the node's block and state-history logs, copied after the capture
//!     so that they hold every block it asked for (served as `--dir`, recursively). Run it with
//!     `cargo test --release -p antelope-ship-archive --test chain_api_nodeos -- --ignored
//!     --nocapture`; like the other tests of real data, it skips, passing, when the variables
//!     are not set.

mod common;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Fixture, WAIT, serve_config};
use hyperion_ship::chainapi::{self, ApiError, Endpoint};
use ship_archive::api::ApiConfig;
use ship_archive::store::StoreConfig;
use ship_archive::{SERVER_HEADER, ServeConfig, Server};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

// ---- HTTP messages ----

/// One response: its head lines (the status line, then each header line, as sent) and body.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Answer {
    head: Vec<String>,
    body: Vec<u8>,
}

impl Answer {
    fn status(&self) -> u16 {
        self.head[0]
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("a status line")
    }

    fn header(&self, name: &str) -> Option<&str> {
        header(&self.head, name)
    }

    fn interim(&self) -> bool {
        (100..200).contains(&self.status())
    }

    /// The answer with the value of `Server` left out, the one difference nodeos's and this
    /// server's answers may have.
    fn normalized(&self) -> Self {
        let head = self
            .head
            .iter()
            .map(|line| match line.split_once(':') {
                Some((k, _)) if k.eq_ignore_ascii_case("server") => format!("{k}: *"),
                _ => line.clone(),
            })
            .collect();
        Self {
            head,
            body: self.body.clone(),
        }
    }

    fn text(&self) -> String {
        let head = self.head.join("\\r\\n");
        let body = String::from_utf8_lossy(&self.body);
        let body: String = body.chars().take(400).collect();
        format!("{head} | {body}")
    }
}

fn header<'a>(head: &'a [String], name: &str) -> Option<&'a str> {
    head[1..].iter().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.eq_ignore_ascii_case(name).then_some(v.trim())
    })
}

/// Where a head ends in `bytes`, after its blank line.
fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|at| at + 4)
}

fn head_lines(head: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(&head[..head.len() - 4])
        .split("\r\n")
        .map(str::to_owned)
        .collect()
}

/// The first request of a capture: method, target, head lines and body.
struct Asked {
    method: String,
    target: String,
    head: Vec<String>,
    body: Vec<u8>,
}

fn asked(raw: &[u8]) -> Asked {
    let end = head_end(raw).expect("a request head");
    let head = head_lines(&raw[..end]);
    let mut words = head[0].split(' ');
    let method = words.next().unwrap_or_default().to_owned();
    let target = words.next().unwrap_or_default().to_owned();
    let rest = &raw[end..];
    let chunked = header(&head, "transfer-encoding").is_some_and(|v| v.contains("chunked"));
    let body = match header(&head, "content-length") {
        Some(len) => rest[..len.parse::<usize>().unwrap().min(rest.len())].to_vec(),
        None if chunked => unchunk(rest),
        None => Vec::new(),
    };
    Asked {
        method,
        target,
        head,
        body,
    }
}

/// A chunked body's data.
fn unchunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(at) = rest.windows(2).position(|w| w == b"\r\n") {
        let size = std::str::from_utf8(&rest[..at]).unwrap();
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
        if size == 0 {
            break;
        }
        out.extend_from_slice(&rest[at + 2..at + 2 + size]);
        rest = &rest[at + 2 + size + 2..];
    }
    out
}

/// The answers a capture holds, each framed by its `Content-Length`; interim (1xx) answers
/// have no body. Bytes after the last whole answer are left out.
fn captured(mut raw: &[u8]) -> Vec<Answer> {
    let mut out = Vec::new();
    while let Some(end) = head_end(raw) {
        let head = head_lines(&raw[..end]);
        let mut answer = Answer {
            head,
            body: Vec::new(),
        };
        let len = match answer.interim() {
            true => 0,
            false => answer
                .header("content-length")
                .map_or(0, |v| v.parse().unwrap()),
        };
        let len = len.min(raw.len() - end);
        answer.body = raw[end..end + len].to_vec();
        raw = &raw[end + len..];
        out.push(answer);
    }
    out
}

/// Send `request` on a connection of its own and read `count` final answers (and any interim
/// ones before them); without a body for the first when `head_only`. Fewer when the server
/// closes the connection, or sends nothing more within `quiet`.
async fn exchange(
    addr: SocketAddr,
    request: &[u8],
    count: usize,
    head_only: bool,
    quiet: Duration,
) -> Vec<Answer> {
    let mut tcp = TcpStream::connect(addr).await.expect("connect to --api");
    // A server may answer and close before it has read a large body.
    let _ = tcp.write_all(request).await;
    let mut buf = Vec::new();
    let mut out = Vec::new();
    let mut finals = 0;
    while finals < count {
        let end = loop {
            if let Some(end) = head_end(&buf) {
                break end;
            }
            if !read_more(&mut tcp, &mut buf, quiet).await {
                return out;
            }
        };
        let mut answer = Answer {
            head: head_lines(&buf[..end]),
            body: Vec::new(),
        };
        buf.drain(..end);
        if !answer.interim() {
            let len = match head_only && finals == 0 {
                true => 0,
                false => answer
                    .header("content-length")
                    .map_or(0, |v| v.parse().unwrap()),
            };
            while buf.len() < len {
                if !read_more(&mut tcp, &mut buf, quiet).await {
                    return out;
                }
            }
            answer.body = buf.drain(..len).collect();
            finals += 1;
        }
        out.push(answer);
    }
    out
}

async fn read_more(tcp: &mut TcpStream, buf: &mut Vec<u8>, quiet: Duration) -> bool {
    let mut chunk = vec![0; 256 << 10];
    match tokio::time::timeout(quiet, tcp.read(&mut chunk)).await {
        Ok(Ok(n)) if n > 0 => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
        _ => false,
    }
}

/// The `<case>.req` and `<case>.resp` pairs under `dir`, by path.
fn pairs(dir: &Path) -> Vec<(String, Vec<u8>, Vec<u8>)> {
    let mut out = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("a capture directory") {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "req") {
                let resp = path.with_extension("resp");
                if let Ok(answer) = fs::read(&resp) {
                    let name = path.strip_prefix(dir.as_path()).unwrap_or(&path);
                    let name = format!(
                        "{}/{}",
                        dir.file_name().unwrap().to_string_lossy(),
                        name.with_extension("").display()
                    );
                    out.push((name, fs::read(&path).unwrap(), answer));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// ---- the server ----

/// A server with the chain API on an ephemeral loopback port, and the sender that stops it.
async fn start(mut cfg: ServeConfig) -> (SocketAddr, oneshot::Sender<()>) {
    let listen = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    cfg.api = Some(ApiConfig::new(listen, cfg.io_threads));
    let server = Server::bind(cfg).await.expect("the server starts");
    let api = server.api_addr().expect("--api is bound");
    let (stop, stopped) = oneshot::channel::<()>();
    tokio::spawn(server.run(async move {
        let _ = stopped.await;
    }));
    (api, stop)
}

/// What this server answers for a capture whose first request asked `asked`.
async fn replay(
    api: SocketAddr,
    raw: &[u8],
    asked: &Asked,
    nodeos: &[Answer],
    quiet: Duration,
) -> Vec<Answer> {
    let finals = nodeos.iter().filter(|a| !a.interim()).count();
    exchange(api, raw, finals.max(1), asked.method == "HEAD", quiet).await
}

fn normalized(answers: &[Answer]) -> Vec<Answer> {
    answers.iter().map(Answer::normalized).collect()
}

/// The start of nodeos's refusal of a request without a `Host`, or with an empty one, which it
/// sends even with `http-validate-host = false`.
const NO_HOST: &[u8] = br#"{"code":400,"message":"Disallowed HTTP HOST header in the request""#;

/// Whether nodeos refused a request for naming no host.
fn refused_for_no_host(nodeos: &[Answer]) -> bool {
    nodeos
        .first()
        .is_some_and(|a| a.status() == 400 && a.body.starts_with(NO_HOST))
}

/// `raw` with `Host: 127.0.0.1` after its request line, in place of any `Host` line it has.
fn with_host(raw: &[u8]) -> Vec<u8> {
    let end = head_end(raw).expect("a request head");
    let mut lines: Vec<String> = head_lines(&raw[..end])
        .into_iter()
        .filter(|line| {
            !line
                .split_once(':')
                .is_some_and(|(k, _)| k.eq_ignore_ascii_case("host"))
        })
        .collect();
    lines.insert(1, "Host: 127.0.0.1".to_owned());
    let mut out = lines.join("\r\n").into_bytes();
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(&raw[end..]);
    out
}

/// Send `request` on a connection of its own and read until the server closes it: what it
/// sent, or `None` when it kept the connection open for [`WAIT`].
async fn until_closed(addr: SocketAddr, request: &[u8]) -> Option<Vec<u8>> {
    let mut tcp = TcpStream::connect(addr).await.expect("connect to --api");
    tcp.write_all(request).await.expect("send the request");
    let mut out = Vec::new();
    loop {
        let mut chunk = [0; 4096];
        match tokio::time::timeout(WAIT, tcp.read(&mut chunk)).await {
            Ok(Ok(0) | Err(_)) => return Some(out),
            Ok(Ok(n)) => out.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
}

// ---- the committed goldens ----

fn goldens() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/testdata/chain-api/dev-chain")
}

/// A golden exchange of `dev-chain/http`: the request and nodeos's answers.
fn golden(case: &str) -> (Vec<u8>, Vec<Answer>) {
    let dir = goldens().join("http");
    let raw = fs::read(dir.join(format!("{case}.req"))).unwrap();
    let resp = fs::read(dir.join(format!("{case}.resp"))).unwrap();
    (raw, captured(&resp))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn answers_that_hold_no_chain_data_are_nodeos_s_byte_for_byte() {
    let fx = Fixture::standard();
    let (api, _stop) = start(fx.serve()).await;
    let mut cases = pairs(&goldens().join("http"));
    let refused = pairs(&goldens().join("requests"))
        .into_iter()
        .filter(|(_, _, resp)| captured(resp)[0].status() != 200);
    cases.extend(refused);
    assert!(cases.len() > 100, "{} cases", cases.len());

    let mut wrong = Vec::new();
    let mut statuses = BTreeMap::<u16, usize>::new();
    let mut no_host = Vec::new();
    for (name, raw, resp) in &cases {
        let nodeos = captured(resp);
        assert_eq!(nodeos.len(), 1, "{name}: one answer");
        let here = replay(api, raw, &asked(raw), &nodeos, WAIT).await;
        let want = if refused_for_no_host(&nodeos) {
            // This server checks no Host: the answer is the one to the request with a Host,
            // which nodeos's answer to that request pins where the goldens hold it.
            no_host.push(name.as_str());
            let hosted = with_host(raw);
            let want = replay(api, &hosted, &asked(&hosted), &nodeos, WAIT).await;
            assert!(
                want.first().is_some_and(|a| !a.body.starts_with(NO_HOST)),
                "{name}"
            );
            want
        } else {
            nodeos.clone()
        };
        if normalized(&here) != normalized(&want) {
            wrong.push(format!(
                "{name}\n  want: {}\n  here: {}",
                want.first().map_or("nothing".into(), Answer::text),
                here.first().map_or("nothing".into(), Answer::text)
            ));
        }
        assert!(
            here.iter()
                .all(|a| a.header("server") == Some(SERVER_HEADER)),
            "{name}"
        );
        *statuses.entry(nodeos[0].status()).or_default() += 1;
    }
    assert!(
        wrong.is_empty(),
        "{} differ:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
    assert_eq!(
        statuses.keys().copied().collect::<Vec<_>>(),
        [200, 400, 404, 500]
    );
    assert_eq!(
        no_host,
        [
            "http/empty-host",
            "http/no-host",
            "http/no-host-http10",
            "http/no-host-options"
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_without_a_host_is_answered_as_one_with_a_host() {
    let fx = Fixture::standard();
    let (api, _stop) = start(fx.serve()).await;
    // A load balancer's HTTP/1.0 health check, which nodeos refuses for naming no host.
    let here = exchange(api, b"OPTIONS / HTTP/1.0\r\n\r\n", 1, false, WAIT).await;
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].head[0], "HTTP/1.0 200 OK");
    assert_eq!(here[0].header("server"), Some(SERVER_HEADER));
    assert_eq!(here[0].body, b"{}");

    let ub = br#"{"block_num_or_id":4294967295}"#;
    let unknown = chainapi::parse(Endpoint::GetRawBlock, ub)
        .unwrap()
        .unknown_block()
        .body();
    for (version, host) in [
        ("HTTP/1.1", ""),
        ("HTTP/1.1", "Host:\r\n"),
        ("HTTP/1.1", "Host: \r\n"),
        ("HTTP/1.0", ""),
    ] {
        let mut raw = format!(
            "POST /v1/chain/get_raw_block {version}\r\n{host}Content-Length: {}\r\n\r\n",
            ub.len()
        )
        .into_bytes();
        raw.extend_from_slice(ub);
        let here = exchange(api, &raw, 1, false, WAIT).await;
        assert_eq!(here.len(), 1, "{version} {host:?}");
        assert_eq!(here[0].status(), 400, "{version} {host:?}");
        assert_eq!(here[0].body, unknown, "{version} {host:?}");
        let hosted = with_host(&raw);
        let want = exchange(api, &hosted, 1, false, WAIT).await;
        assert_eq!(here, want, "{version} {host:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connection_lines_are_read_as_nodeos_reads_them_where_hyper_lets_them_be() {
    let fx = Fixture::standard();
    let (api, _stop) = start(fx.serve()).await;
    let ub = r#"{"block_num_or_id":4294967295}"#;
    let request = |version: &str, lines: &[&str]| {
        let lines: String = lines
            .iter()
            .map(|l| format!("Connection: {l}\r\n"))
            .collect();
        format!(
            "POST /v1/chain/get_raw_block {version}\r\nHost: 127.0.0.1\r\n{lines}\
             Content-Length: {}\r\n\r\n{ub}",
            ub.len()
        )
    };
    // HTTP/1.0: Beast reads the first line alone, which here does not keep the connection
    // alive: nodeos's answer to a request with no Connection line, and the connection closed.
    let (_, nodeos) = golden("ub-http10");
    let sent = until_closed(
        api,
        request("HTTP/1.0", &["close", "keep-alive"]).as_bytes(),
    )
    .await
    .expect("the connection is closed");
    assert_eq!(normalized(&captured(&sent)), normalized(&nodeos));

    // HTTP/1.1: a later `close` closes the connection in hyper; nodeos, reading the first line
    // alone, would keep it and send no Connection header. The answer says `close` where nodeos
    // puts it.
    let (_, nodeos) = golden("ub-close");
    let sent = until_closed(
        api,
        request("HTTP/1.1", &["keep-alive", "close"]).as_bytes(),
    )
    .await
    .expect("the connection is closed");
    assert_eq!(normalized(&captured(&sent)), normalized(&nodeos));

    // One line that keeps an HTTP/1.0 connection alive: nodeos's answer, and the connection
    // kept for the next request.
    let (raw, nodeos) = golden("ub-http10-keepalive");
    assert_eq!(nodeos[0].header("connection"), Some("keep-alive"));
    let here = exchange(api, &[raw.clone(), raw].concat(), 2, false, WAIT).await;
    let twice = [nodeos.clone(), nodeos].concat();
    assert_eq!(normalized(&here), normalized(&twice));
}

// ---- a node's own answers ----

/// Why an answer may differ from the node's: what this server does otherwise by design, or what
/// hyper does before the server sees a request. Each is narrow: it names what may differ, and
/// the answer must still be the one this server gives on purpose.
fn deviation(asked: &Asked, nodeos: &[Answer], here: &[Answer]) -> Option<&'static str> {
    let first = |answers: &[Answer]| answers.iter().find(|a| !a.interim()).cloned();
    let (theirs, ours) = (first(nodeos), first(here));
    let not_found = ApiError::NotFound.body();
    let is_ours = |a: &Option<Answer>, status: u16, body: &[u8]| {
        a.as_ref()
            .is_some_and(|a| a.status() == status && a.body == body)
    };
    let same_but_head = match (&theirs, &ours) {
        (Some(t), Some(o)) => t.status() == o.status() && t.body == o.body,
        _ => false,
    };
    if asked.method == "HEAD"
        && let (Some(t), Some(o)) = (&theirs, &ours)
        && t.normalized().head == o.normalized().head
        && o.body.is_empty()
    {
        return Some("HEAD: nodeos writes the body too, hyper writes the head alone");
    }
    if header(&asked.head, "expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
        && theirs.as_ref().map(Answer::normalized) == ours.as_ref().map(Answer::normalized)
    {
        return Some("Expect: 100-continue: hyper's interim answer has no Server header");
    }
    let declared = header(&asked.head, "content-length").and_then(|v| v.parse::<usize>().ok());
    let too_large = ApiError::TooLarge(2 << 20).body();
    if theirs.is_none() && declared.is_some_and(|n| n > 2 << 20) && is_ours(&ours, 413, &too_large)
    {
        return Some("an oversized body is answered 413; nodeos closes the connection");
    }
    if ours
        .as_ref()
        .is_some_and(|a| a.status() == 400 && a.header("server").is_none())
    {
        return Some(
            "hyper refuses a request it cannot parse (such as a target byte that is not a URI \
             character) before any handler",
        );
    }
    if asked.target.contains('#') {
        return Some("hyper drops a target's #fragment, which nodeos takes as part of the path");
    }
    if refused_for_no_host(nodeos)
        && header(&asked.head, "host").is_none_or(str::is_empty)
        && ours.as_ref().is_some_and(|a| !a.body.starts_with(NO_HOST))
    {
        return Some("no Host: nodeos refuses the request, this server checks no Host");
    }
    let connection_lines = asked.head[1..]
        .iter()
        .filter(|line| {
            line.split_once(':')
                .is_some_and(|(k, _)| k.eq_ignore_ascii_case("connection"))
        })
        .count();
    if connection_lines > 1 && same_but_head {
        return Some(
            "several Connection lines: nodeos reads the first, hyper closes on any `close`",
        );
    }
    if asked.target == ship_archive::api::DESCRIBE && is_ours(&theirs, 404, &not_found) {
        return Some("describe: this server's own endpoint, which nodeos does not have");
    }
    if asked.target == ship_archive::api::SUPPORTED_APIS
        && let (Some(t), Some(o)) = (&theirs, &ours)
        && t.status() == 200
        && o.status() == 200
        && unframed(&t.normalized().head) == unframed(&o.normalized().head)
    {
        return Some("get_supported_apis: this server lists its own endpoints");
    }
    if Endpoint::from_target(&asked.target).is_none()
        && is_ours(&ours, 404, &not_found)
        && !is_ours(&theirs, 404, &not_found)
    {
        return Some("an endpoint nodeos serves and this server does not");
    }
    None
}

/// A head without its `Content-Length` line.
fn unframed(head: &[String]) -> Vec<&String> {
    head.iter()
        .filter(|line| {
            !line
                .split_once(':')
                .is_some_and(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        })
        .collect()
}

/// The block answers that must be compared at least once each.
const BLOCK_ANSWERS: [&str; 4] = [
    "get_block_info",
    "get_block_header",
    "get_block_header with extensions",
    "get_raw_block",
];

/// Which of [`BLOCK_ANSWERS`] a request is, when the node answered it with a block.
fn block_answer(asked: &Asked, nodeos: &[Answer]) -> Option<&'static str> {
    let endpoint = Endpoint::from_target(&asked.target)?;
    // `OPTIONS` on a block endpoint is `200 {}`, no block.
    if asked.method == "OPTIONS" || nodeos.iter().find(|a| !a.interim())?.status() != 200 {
        return None;
    }
    let req = chainapi::parse(endpoint, &asked.body).ok()?;
    Some(match (endpoint, req.include_extensions) {
        (Endpoint::GetBlockInfo, _) => BLOCK_ANSWERS[0],
        (Endpoint::GetBlockHeader, false) => BLOCK_ANSWERS[1],
        (Endpoint::GetBlockHeader, true) => BLOCK_ANSWERS[2],
        (Endpoint::GetRawBlock, _) => BLOCK_ANSWERS[3],
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs CHAIN_API_NODEOS_CAPTURES and CHAIN_API_NODEOS_DATA"]
async fn a_node_s_own_answers_but_for_the_deviations() {
    let (Ok(captures), Ok(data)) = (
        env::var("CHAIN_API_NODEOS_CAPTURES"),
        env::var("CHAIN_API_NODEOS_DATA"),
    ) else {
        println!("skipped: set CHAIN_API_NODEOS_CAPTURES and CHAIN_API_NODEOS_DATA");
        return;
    };
    let cases = pairs(Path::new(&captures));
    assert!(!cases.is_empty(), "no <case>.req and <case>.resp pairs");
    let store = StoreConfig {
        dirs: vec![PathBuf::from(&data)],
        recursive: true,
        ..StoreConfig::default()
    };
    let (api, _stop) = start(serve_config(store)).await;

    let (mut same, mut by_endpoint) = (0, BTreeMap::<String, usize>::new());
    let mut blocks = BTreeMap::<&str, usize>::new();
    let mut deviations = BTreeMap::<&str, Vec<String>>::new();
    let mut wrong = Vec::new();
    for (name, raw, resp) in &cases {
        let req = asked(raw);
        let nodeos = captured(resp);
        // Answers nodeos never sent are waited for briefly.
        let quiet = if nodeos.is_empty() {
            Duration::from_secs(2)
        } else {
            WAIT
        };
        let here = replay(api, raw, &req, &nodeos, quiet).await;
        if normalized(&here) == normalized(&nodeos) {
            same += 1;
            let endpoint = Endpoint::from_target(&req.target).map_or("other", Endpoint::name);
            *by_endpoint.entry(endpoint.to_owned()).or_default() += 1;
            if let Some(kind) = block_answer(&req, &nodeos) {
                *blocks.entry(kind).or_default() += 1;
            }
        } else if let Some(why) = deviation(&req, &nodeos, &here) {
            deviations.entry(why).or_default().push(name.clone());
        } else {
            wrong.push(format!(
                "{name}: {} {}\n  nodeos: {}\n  here:   {}",
                req.method,
                req.target,
                nodeos.first().map_or("nothing".into(), Answer::text),
                here.first().map_or("nothing".into(), Answer::text)
            ));
        }
    }
    println!(
        "a node's own answers: {} exchanges; {same} byte-equal but for Server \
         ({by_endpoint:?}); block answers {blocks:?}",
        cases.len()
    );
    for (why, names) in &deviations {
        let some: Vec<_> = names.iter().take(4).map(String::as_str).collect();
        println!(
            "  deviation, {} answers: {why} (e.g. {})",
            names.len(),
            some.join(", ")
        );
    }
    for w in &wrong {
        println!("  DIFFERS {w}");
    }
    assert!(
        wrong.is_empty(),
        "{} answers differ from the node's",
        wrong.len()
    );
    let missing: Vec<_> = BLOCK_ANSWERS
        .iter()
        .filter(|kind| !blocks.contains_key(*kind))
        .collect();
    assert!(
        missing.is_empty(),
        "no byte-equal block answer of {missing:?}: capture with a --block for each block to \
         compare"
    );
}
