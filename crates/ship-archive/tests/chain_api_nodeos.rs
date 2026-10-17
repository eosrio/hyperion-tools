//! The chain API (`--api`) against nodeos's own answers, over TCP on loopback, byte for byte.
//!
//!   * The committed goldens (`crates/core/testdata/chain-api/dev-chain`, raw HTTP exchanges with
//!     a Spring 1.2.2 dev chain): every request of `http/`, and every request of `requests/` that
//!     nodeos refused, is sent to a server over a synthetic chain, and the whole answer must be
//!     nodeos's (status line, headers in their order, body) but for the value of `Server`. These
//!     answers hold nothing of the chain: they come before any handler, or name a block that is
//!     never served, or refuse the request.
//!   * R1, ignored: a server over a copy of a Spring 1.1+ node's logs answers every request that
//!     node answered, and each answer must be the node's but for `Server`, apart from the
//!     documented deviations, which are counted and printed. Nothing of that data is committed.
//!     `CHAIN_API_NODEOS_CAPTURES` names a directory of raw `<case>.req` and `<case>.resp` pairs
//!     (searched recursively), such as `scripts/capture_chain_api_goldens.sh` writes;
//!     `CHAIN_API_NODEOS_DATA` the node's block and state-history logs, copied after the capture
//!     so that they hold every block it asked for (served as `--dir`, recursively). Run it with
//!     `cargo test --release -p antelope-ship-archive --test chain_api_nodeos -- --ignored
//!     --nocapture`; it skips, passing, when the variables are not set.

mod common;

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Fixture, WAIT, serve_config};
use hyperion_ship::chainapi::{ApiError, Endpoint};
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

// ---- the committed goldens ----

fn goldens() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/testdata/chain-api/dev-chain")
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
    for (name, raw, resp) in &cases {
        let nodeos = captured(resp);
        assert_eq!(nodeos.len(), 1, "{name}: one answer");
        let here = replay(api, raw, &asked(raw), &nodeos, WAIT).await;
        if normalized(&here) != normalized(&nodeos) {
            wrong.push(format!(
                "{name}\n  nodeos: {}\n  here:   {}",
                nodeos[0].text(),
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
}

// ---- R1: a node's own answers ----

/// Why an answer may differ from nodeos's (README and CONTRACT list each one).
fn deviation(asked: &Asked, nodeos: &[Answer], here: &[Answer]) -> Option<&'static str> {
    let first = |answers: &[Answer]| answers.iter().find(|a| !a.interim()).cloned();
    let (theirs, ours) = (first(nodeos), first(here));
    let not_found = ApiError::NotFound.body();
    let unparsable = ApiError::Unparsable.body();
    let is_ours = |a: &Option<Answer>, status: u16, body: &[u8]| {
        a.as_ref()
            .is_some_and(|a| a.status() == status && a.body == body)
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
        return Some("X1: an oversized body is answered 413, nodeos closes the connection");
    }
    if !asked.target.starts_with('/')
        && ours
            .as_ref()
            .is_some_and(|a| a.status() == 400 && a.header("server").is_none())
    {
        return Some("hyper refuses a request-target it cannot parse, before any handler");
    }
    if asked.target == ship_archive::api::DESCRIBE {
        return Some("describe: this server's own endpoint, which nodeos does not have");
    }
    if asked.target == ship_archive::api::SUPPORTED_APIS {
        return Some("get_supported_apis: this server lists its own endpoints");
    }
    if Endpoint::from_target(&asked.target).is_none()
        && is_ours(&ours, 404, &not_found)
        && !is_ours(&theirs, 404, &not_found)
    {
        return Some("C-F9: an endpoint nodeos serves and this server does not");
    }
    if is_ours(&ours, 400, &unparsable)
        && !is_ours(&theirs, 400, &unparsable)
        && outside_the_subset(&asked.body)
    {
        return Some("X4: a body outside the subset of fc's JSON this server reads");
    }
    None
}

/// Whether `body` holds what fc reads differently from strict JSON: outside strings, a number
/// with a fraction or an exponent, or a bare word other than `true`, `false` and `null`.
fn outside_the_subset(body: &[u8]) -> bool {
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b'"' => {
                i += 1;
                while i < body.len() && body[i] != b'"' {
                    i += if body[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'.' => return true,
            c if c.is_ascii_digit() => {
                let rest = &body[i..];
                let len = rest.iter().take_while(|c| c.is_ascii_digit()).count();
                if matches!(rest.get(len), Some(b'.' | b'e' | b'E')) {
                    return true;
                }
                i += len;
            }
            c if c.is_ascii_alphabetic() => {
                let rest = &body[i..];
                let len = rest
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .count();
                if !matches!(&rest[..len], b"true" | b"false" | b"null") {
                    return true;
                }
                i += len;
            }
            _ => i += 1,
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs CHAIN_API_NODEOS_CAPTURES and CHAIN_API_NODEOS_DATA"]
async fn r1_a_node_s_own_answers() {
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
        "R1: {} exchanges; {same} byte-equal but for Server ({by_endpoint:?})",
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
}
