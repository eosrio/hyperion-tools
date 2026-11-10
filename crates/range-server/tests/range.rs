//! The range service over a synthetic Wire node: a block log of real Wire `signed_block`s, a
//! trace log and a chain-state log naming those blocks' ids, and sealed catalogs covering them.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CHAIN_ID: &str = "9999999999999999999999999999999999999999999999999999999999999999";
const PROTOCOL: &str = "wire-sysio-6508c9e8-engineering-v1";
const DOC_PROFILE: &str = "wire-sysio-6508c9e8-native-engineering-v1";
const NODE_REVISION: &str = "6508c9e8f4044df8d5acb082a7af3be6a80cd9a0";

/// The node's SHiP ABI, as a Wire node's handshake sends it.
fn ship_abi() -> String {
    let capture: Value =
        serde_json::from_str(include_str!("../../core/testdata/wire/ship-capture.json")).unwrap();
    capture["handshakeUtf8"].as_str().unwrap().to_owned()
}

fn name(s: &str) -> u64 {
    const CHARMAP: &[u8; 32] = b".12345abcdefghijklmnopqrstuvwxyz";
    let mut value = 0u64;
    for (i, c) in s.bytes().enumerate() {
        let v = CHARMAP.iter().position(|&x| x == c).unwrap() as u64;
        value |= (v & 0x1f) << (64 - 5 * (i + 1));
    }
    value
}

fn varuint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn blob(out: &mut Vec<u8>, bytes: &[u8]) {
    varuint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct Action {
    ordinal: u64,
    creator: u64,
    receiver: &'static str,
    data: Vec<u8>,
    sequence: u64,
}

struct Transaction {
    packed: Vec<u8>,
    cpu: Vec<u64>,
    actions: Vec<Action>,
}

impl Transaction {
    fn id(&self) -> [u8; 32] {
        Sha256::digest(&self.packed).into()
    }
}

/// A Wire block header (no policy diffs or extensions) and its signed block.
fn signed_block(n: u32, previous: [u8; 32], transactions: &[Transaction]) -> (Vec<u8>, [u8; 32]) {
    let mut b = Vec::new();
    b.extend_from_slice(&(1_000_000 + n).to_le_bytes());
    b.extend_from_slice(&name("prod.a").to_le_bytes());
    b.extend_from_slice(&previous);
    b.extend_from_slice(&[0; 64]);
    b.extend_from_slice(&n.saturating_sub(1).to_le_bytes());
    b.push(1);
    b.push(0);
    b.push(0);
    varuint(&mut b, 0);
    let mut id: [u8; 32] = Sha256::digest(&b).into();
    id[..4].copy_from_slice(&n.to_be_bytes());
    varuint(&mut b, 1);
    b.push(0);
    b.extend_from_slice(&[0x1f; 65]);
    varuint(&mut b, transactions.len() as u64);
    for t in transactions {
        varuint(&mut b, t.cpu.len() as u64);
        for &c in &t.cpu {
            varuint(&mut b, c);
        }
        varuint(&mut b, 0);
        b.push(0);
        blob(&mut b, &[]);
        blob(&mut b, &t.packed);
    }
    b.push(0);
    varuint(&mut b, 0);
    (b, id)
}

fn zlib_payload(data: &[u8]) -> Vec<u8> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(data).unwrap();
    let mut payload = 1u32.to_le_bytes().to_vec();
    payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
    payload.extend_from_slice(&z.finish().unwrap());
    payload
}

/// A `transaction_trace[]` payload, as the trace log stores it.
fn traces(transactions: &[Transaction]) -> Vec<u8> {
    let mut t = Vec::new();
    varuint(&mut t, transactions.len() as u64);
    for tx in transactions {
        varuint(&mut t, 0);
        t.extend_from_slice(&tx.id());
        t.push(0);
        t.extend_from_slice(&(tx.cpu.iter().sum::<u64>() as u32).to_le_bytes());
        varuint(&mut t, 0);
        t.extend_from_slice(&0i64.to_le_bytes());
        t.extend_from_slice(&0u64.to_le_bytes());
        t.push(0);
        varuint(&mut t, tx.actions.len() as u64);
        for a in &tx.actions {
            varuint(&mut t, 0);
            varuint(&mut t, a.ordinal);
            varuint(&mut t, a.creator);
            t.push(1);
            varuint(&mut t, 0);
            t.extend_from_slice(&name(a.receiver).to_le_bytes());
            let digest: [u8; 32] = Sha256::digest([b"token".as_slice(), &a.data].concat()).into();
            t.extend_from_slice(&digest);
            t.extend_from_slice(&a.sequence.to_le_bytes());
            t.extend_from_slice(&1u64.to_le_bytes());
            varuint(&mut t, 1);
            t.extend_from_slice(&name("alice").to_le_bytes());
            t.extend_from_slice(&7u64.to_le_bytes());
            varuint(&mut t, 1);
            varuint(&mut t, 0); // abi_sequence: no ABI was ever deployed
            t.extend_from_slice(&name(a.receiver).to_le_bytes());
            t.extend_from_slice(&name("token").to_le_bytes());
            t.extend_from_slice(&name("transfer").to_le_bytes());
            varuint(&mut t, 1);
            t.extend_from_slice(&name("alice").to_le_bytes());
            t.extend_from_slice(&name("active").to_le_bytes());
            blob(&mut t, &a.data);
            t.push(0);
            t.extend_from_slice(&0i64.to_le_bytes());
            blob(&mut t, &[]);
            varuint(&mut t, 0);
            t.push(0);
            t.push(0);
        }
        t.extend_from_slice(&[0; 5]);
    }
    zlib_payload(&t)
}

fn action(
    ordinal: u64,
    creator: u64,
    receiver: &'static str,
    data: &[u8],
    sequence: u64,
) -> Action {
    Action {
        ordinal,
        creator,
        receiver,
        data: data.to_vec(),
        sequence,
    }
}

/// Blocks 1..=6: a transfer with two notifications and an inline at block 2, one action at 4.
fn chain() -> Vec<Vec<Transaction>> {
    vec![
        vec![],
        vec![Transaction {
            packed: b"transfer transaction".to_vec(),
            cpu: vec![11, 22],
            actions: vec![
                action(1, 0, "token", &[1, 2], 100),
                action(2, 1, "alice", &[1, 2], 101),
                action(3, 1, "bob", &[1, 2], 102),
                action(4, 1, "token", &[3], 103),
            ],
        }],
        vec![],
        vec![Transaction {
            packed: b"another transaction".to_vec(),
            cpu: vec![5],
            actions: vec![action(1, 0, "token", &[9], 104)],
        }],
        vec![],
        vec![],
    ]
}

struct Node {
    dir: PathBuf,
    ids: Vec<[u8; 32]>,
}

fn ship_log(path: &Path, stem: &str, entries: &[([u8; 32], Vec<u8>)]) {
    let (mut log, mut index) = (Vec::new(), Vec::new());
    for (id, payload) in entries {
        let pos = log.len() as u64;
        index.extend_from_slice(&pos.to_le_bytes());
        log.extend_from_slice(&0xc35d_5000_0000_0000u64.to_le_bytes());
        log.extend_from_slice(id);
        log.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        log.extend_from_slice(payload);
        log.extend_from_slice(&pos.to_le_bytes());
    }
    std::fs::write(path.join(format!("{stem}.log")), log).unwrap();
    std::fs::write(path.join(format!("{stem}.index")), index).unwrap();
}

impl Node {
    fn new(test: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("range-server-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["blocks", "state-history", "catalogs"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let mut previous = [0u8; 32];
        let (mut log, mut index) = (Vec::new(), Vec::new());
        log.extend_from_slice(&3u32.to_le_bytes());
        log.extend_from_slice(&1u32.to_le_bytes());
        log.extend_from_slice(&[0x42; 32]);
        let (mut traced, mut states, mut ids) = (Vec::new(), Vec::new(), Vec::new());
        for (i, txs) in chain().iter().enumerate() {
            let n = i as u32 + 1;
            let (bytes, id) = signed_block(n, previous, txs);
            let pos = log.len() as u64;
            index.extend_from_slice(&pos.to_le_bytes());
            log.extend_from_slice(&bytes);
            log.extend_from_slice(&pos.to_le_bytes());
            if n >= 2 {
                traced.push((id, traces(txs)));
            }
            states.push((id, zlib_payload(&[0])));
            ids.push(id);
            previous = id;
        }
        std::fs::write(dir.join("blocks/blocks.log"), log).unwrap();
        std::fs::write(dir.join("blocks/blocks.index"), index).unwrap();
        ship_log(&dir.join("state-history"), "trace_history", &traced);
        ship_log(&dir.join("state-history"), "chain_state_history", &states);
        Self { dir, ids }
    }

    /// Seal a catalog over blocks 1..=end (with `ids` as its membership), extending `base`.
    fn seal(&self, sub: &str, end: u32, ids: &[[u8; 32]], base: Option<(&str, u32)>) -> String {
        let d = self.dir.join("catalogs").join(sub);
        std::fs::create_dir_all(&d).unwrap();
        let members: Vec<u8> = ids[..end as usize].iter().flatten().copied().collect();
        std::fs::write(d.join("abi.ndjson"), b"").unwrap();
        std::fs::write(d.join("abi.ndjson.blocks.bin"), &members).unwrap();
        let id = |n: u32| hex::encode(ids[n as usize - 1]);
        let mut m = json!({"format":"hyperion-wire-abi-history-v1","chainId":CHAIN_ID,"nodeRevision":NODE_REVISION,
            "producerProfileId":PROTOCOL,"shipAbiSha256":sha(ship_abi().as_bytes()),"startBlock":1,"endBlock":end,
            "firstBlockId":id(1),"lastBlockId":id(end),"irreversibleBoundary":{"blockNum":end,"blockId":id(end)},
            "complete":true,"lastGlobalSequence":null,
            "deployments":{"name":"abi.ndjson","bytes":0,"sha256":sha(b""),"records":0},
            "blockIds":{"name":"abi.ndjson.blocks.bin","bytes":members.len(),"sha256":sha(&members),"count":end}});
        if let Some((sha, base_end)) = base {
            m["base"] =
                json!({"manifestSha256":sha,"endBlock":base_end,"lastBlockId":id(base_end)});
        }
        let text = format!("{m}\n");
        std::fs::write(d.join("abi.ndjson.manifest.json"), &text).unwrap();
        sha(text.as_bytes())
    }

    fn start(&self, genesis: &str, extra: &[&str]) -> Server {
        let p = |s: &str| self.dir.join(s).to_str().unwrap().to_owned();
        let mut child = Command::new(env!("CARGO_BIN_EXE_range-server"))
            .args([
                "--protocol",
                PROTOCOL,
                "--chain-id",
                CHAIN_ID,
                "--doc-profile-id",
                DOC_PROFILE,
            ])
            .args([
                "--state-history",
                &p("state-history"),
                "--blocks-dir",
                &p("blocks"),
            ])
            .args([
                "--catalog-dir",
                &p("catalogs"),
                "--genesis-catalog-sha256",
                genesis,
            ])
            .args([
                "--listen",
                "127.0.0.1:0",
                "--threads",
                "2",
                "--credit-timeout",
                "5",
            ])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| {
                let mut err = String::new();
                child.stderr.take().unwrap().read_to_string(&mut err).ok();
                panic!("the service did not start: {line} {err}")
            })
            .to_owned();
        Server { child, addr }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Server {
    child: Child,
    addr: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A response: status, headers and a reader over the de-chunked body.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Box<dyn BufRead>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn text(mut self) -> String {
        let mut s = String::new();
        self.body.read_to_string(&mut s).unwrap();
        s
    }

    fn json(self) -> Value {
        serde_json::from_str(&self.text()).unwrap()
    }
}

/// De-chunks an HTTP/1.1 chunked body.
struct Chunked<R> {
    inner: R,
    left: usize,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.inner.read_line(&mut line)?;
            self.left = usize::from_str_radix(line.trim(), 16).unwrap();
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left);
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n;
        if self.left == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
        }
        Ok(n)
    }
}

fn http(addr: &str, method: &str, path: &str, body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut r = BufReader::new(s);
    let mut status = String::new();
    r.read_line(&mut status).unwrap();
    let status = status.split(' ').nth(1).unwrap().parse().unwrap();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (k, v) = line.split_once(':').unwrap();
        headers.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    let chunked = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"));
    let length = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.parse::<u64>().unwrap());
    let body: Box<dyn BufRead> = if chunked {
        Box::new(BufReader::new(Chunked {
            inner: r,
            left: 0,
            done: false,
        }))
    } else if let Some(n) = length {
        Box::new(BufReader::new(r.take(n)))
    } else {
        Box::new(r)
    };
    Reply {
        status,
        headers,
        body,
    }
}

fn range(addr: &str, body: Value) -> Reply {
    http(addr, "POST", "/v1/ranges", &body.to_string())
}

/// The frames of a complete range response: (header, payload) pairs.
fn frames(reply: Reply) -> Vec<(Value, Vec<u8>)> {
    let mut body = reply.body;
    let mut out = Vec::new();
    loop {
        let mut line = String::new();
        if body.read_line(&mut line).unwrap() == 0 {
            return out;
        }
        let h: Value = serde_json::from_str(&line).unwrap();
        let mut payload = vec![0u8; h["bytes"].as_u64().unwrap_or(0) as usize];
        body.read_exact(&mut payload).unwrap();
        out.push((h, payload));
    }
}

fn all(from: u32, to: u32) -> Value {
    json!({"from":from,"to":to,"profile":PROTOCOL})
}

#[test]
fn a_range_streams_framed_bulk_lines_and_a_verifiable_manifest() {
    let node = Node::new("frames");
    let genesis = node.seal("a", 6, &node.ids, None);
    let server = node.start(&genesis, &[]);
    let reply = range(&server.addr, all(1, 6));
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("X-Range-Catalog"), Some(genesis.as_str()));
    let f = frames(reply);
    assert_eq!(f.len(), 8, "start, six blocks, manifest");
    assert_eq!(f[0].0["frame"], "start");
    assert_eq!(f[0].0["previousBlockId"], Value::Null);
    let mut digests = [Sha256::new(), Sha256::new(), Sha256::new()];
    let mut docs: [Vec<Value>; 3] = Default::default();
    for (i, (h, payload)) in f[1..7].iter().enumerate() {
        assert_eq!(h["frame"], "block");
        assert_eq!(h["block"], i + 1);
        assert_eq!(h["id"], hex::encode(node.ids[i]));
        let mut at = 0;
        for (k, kind) in ["blocks", "actions", "deltas"].iter().enumerate() {
            let size = h["docs"][kind][1].as_u64().unwrap() as usize;
            let part = &payload[at..at + size];
            at += size;
            digests[k].update(part);
            let lines: Vec<&[u8]> = part.split(|b| *b == b'\n').collect();
            assert_eq!(
                lines.len(),
                2 * h["docs"][kind][0].as_u64().unwrap() as usize + 1
            );
            for pair in lines.chunks(2).filter(|p| p.len() == 2) {
                let meta: Value = serde_json::from_slice(pair[0]).unwrap();
                let doc: Value = serde_json::from_slice(pair[1]).unwrap();
                match *kind {
                    "blocks" => assert_eq!(meta["index"]["_id"], doc["block_num"].to_string()),
                    "actions" => {
                        assert_eq!(meta["index"]["_index"], "wire-action-v1-000001");
                        assert_eq!(meta["index"]["_id"], doc["global_sequence"]);
                    }
                    _ => {}
                }
                docs[k].push(doc);
            }
        }
        assert_eq!(at, payload.len());
    }
    assert_eq!(
        docs[0].len(),
        6,
        "a block document for every block, block 1 included"
    );
    assert_eq!(docs[0][1]["prev_id"], hex::encode(node.ids[0]));
    assert_eq!(docs[0][1]["protocol"]["profileId"], DOC_PROFILE);
    let receivers: Vec<(u64, usize)> = docs[1]
        .iter()
        .map(|d| {
            (
                d["block_num"].as_u64().unwrap(),
                d["receipts"].as_array().unwrap().len(),
            )
        })
        .collect();
    assert_eq!(
        receivers,
        vec![(2, 3), (2, 1), (4, 1)],
        "notifications grouped by input"
    );
    assert_eq!(docs[1][0]["protocol"]["schemaVersion"], 3);
    let m = &f[7].0;
    assert_eq!(m["frame"], "manifest");
    assert_eq!(
        (m["from"].as_u64(), m["to"].as_u64(), m["blocks"].as_u64()),
        (Some(1), Some(6), Some(6))
    );
    assert_eq!(m["catalogSha"], genesis);
    assert_eq!(m["firstBlockId"], hex::encode(node.ids[0]));
    assert_eq!(m["lastBlockId"], hex::encode(node.ids[5]));
    assert_eq!(
        (
            m["firstGlobalSequence"].as_str(),
            m["lastGlobalSequence"].as_str()
        ),
        (Some("100"), Some("104"))
    );
    for (k, kind) in ["blocks", "actions", "deltas"].iter().enumerate() {
        assert_eq!(
            m["kinds"][kind]["sha256"],
            hex::encode(digests[k].clone().finalize())
        );
        assert_eq!(m["kinds"][kind]["docs"], docs[k].len());
    }

    // The same request returns the same bytes; a later range names the block before it.
    let again = |r: Value| range(&server.addr, r).text();
    assert_eq!(again(all(1, 6)), again(all(1, 6)));
    let tail = frames(range(
        &server.addr,
        json!({"from":4,"to":6,"profile":PROTOCOL,"kinds":["actions"]}),
    ));
    assert_eq!(tail[0].0["previousBlockId"], hex::encode(node.ids[2]));
    assert_eq!(tail[1].0["docs"], json!({"actions":[1, tail[1].1.len()]}));
    assert!(tail.last().unwrap().0["kinds"].get("blocks").is_none());
}

#[test]
fn ranges_past_the_sealed_lib_or_for_another_profile_are_refused() {
    let node = Node::new("refusals");
    let genesis = node.seal("a", 4, &node.ids, None);
    let server = node.start(&genesis, &[]);
    let info = http(&server.addr, "GET", "/v1/info", "").json();
    assert_eq!(
        info["sealedLib"], 4,
        "the catalog ends at 4; the logs hold 6"
    );
    assert_eq!(info["logs"]["blocks"], json!([1, 6]));
    let r = range(&server.addr, all(3, 5));
    assert_eq!(r.status, 409);
    let v = r.json();
    assert_eq!(
        (v["error"].as_str(), v["sealedLib"].as_u64()),
        (Some("beyond_sealed_lib"), Some(4))
    );
    let r = range(
        &server.addr,
        json!({"from":1,"to":2,"profile":"wire-other"}),
    );
    assert_eq!(r.status, 409);
    let r = range(
        &server.addr,
        json!({"from":1,"to":2,"profile":PROTOCOL,"kinds":["abis"]}),
    );
    assert_eq!(
        (r.status, r.json()["error"].as_str().map(str::to_owned)),
        (400, Some("unsupported_kind".into()))
    );
    assert_eq!(range(&server.addr, all(3, 2)).status, 400);
    assert_eq!(
        http(&server.addr, "GET", "/v1/catalog?after=00", "").status,
        404
    );
    let cat = http(&server.addr, "GET", "/v1/catalog", "").json();
    assert_eq!(cat["segments"].as_array().unwrap().len(), 1);
    let manifest = http(
        &server.addr,
        "GET",
        &format!("/v1/catalog/{genesis}/abi.ndjson.manifest.json"),
        "",
    )
    .text();
    assert_eq!(
        sha(manifest.as_bytes()),
        genesis,
        "members are served verbatim"
    );
}

#[test]
fn credits_hold_the_stream_until_granted() {
    let node = Node::new("credits");
    let genesis = node.seal("a", 6, &node.ids, None);
    let server = node.start(&genesis, &[]);
    let reply = range(
        &server.addr,
        json!({"from":1,"to":6,"profile":PROTOCOL,"credits":2}),
    );
    let stream = reply.header("X-Range-Stream").unwrap().to_owned();
    let mut body = reply.body;
    let mut frame = || -> Value {
        let mut line = String::new();
        body.read_line(&mut line).unwrap();
        let h: Value = serde_json::from_str(&line).unwrap();
        let mut payload = vec![0u8; h["bytes"].as_u64().unwrap_or(0) as usize];
        body.read_exact(&mut payload).unwrap();
        h
    };
    assert_eq!(frame()["frame"], "start");
    assert_eq!(frame()["block"], 1);
    assert_eq!(frame()["block"], 2);
    // No third frame arrives until a grant: grant one at a time and see each block follow.
    let started = Instant::now();
    let granter = {
        let addr = server.addr.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            let r = http(
                &addr,
                "POST",
                &format!("/v1/ranges/{stream}/credits"),
                r#"{"frames":10}"#,
            );
            assert_eq!(r.status, 200);
        })
    };
    assert_eq!(frame()["block"], 3);
    assert!(
        started.elapsed() >= Duration::from_millis(350),
        "block 3 waited for its credit"
    );
    for n in 4..=6 {
        assert_eq!(frame()["block"], n);
    }
    assert_eq!(frame()["frame"], "manifest");
    granter.join().unwrap();
    assert_eq!(
        http(
            &server.addr,
            "POST",
            "/v1/ranges/999/credits",
            r#"{"frames":1}"#
        )
        .status,
        404
    );
}

#[test]
fn follow_mode_adopts_verified_extensions() {
    let node = Node::new("follow");
    let genesis = node.seal("a", 3, &node.ids, None);
    let server = node.start(&genesis, &["--follow", "--poll-ms", "100"]);
    assert_eq!(
        http(&server.addr, "GET", "/v1/info", "").json()["sealedLib"],
        3
    );
    // An extension that rewrites a block below its base is ignored.
    let mut forked = node.ids.clone();
    forked[1][31] ^= 1;
    node.seal("b", 5, &forked, Some((&genesis, 3)));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        http(&server.addr, "GET", "/v1/info", "").json()["sealedLib"],
        3
    );
    let ext = node.seal("c", 6, &node.ids, Some((&genesis, 3)));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let info = http(&server.addr, "GET", "/v1/info", "").json();
        if info["sealedLib"] == 6 {
            assert_eq!(info["catalog"]["manifestSha256"], ext);
            assert_eq!(info["catalog"]["segments"], 2);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the extension was not adopted: {info}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let f = frames(range(&server.addr, all(2, 6)));
    let m = &f.last().unwrap().0;
    assert_eq!(m["frame"], "manifest");
    assert_eq!(m["catalogSha"], ext, "the segment that sealed block 6");
    let f = frames(range(&server.addr, all(1, 3)));
    assert_eq!(f.last().unwrap().0["catalogSha"], genesis);
    let cat = http(
        &server.addr,
        "GET",
        &format!("/v1/catalog?after={genesis}"),
        "",
    )
    .json();
    assert_eq!(cat["segments"][0]["manifestSha256"], ext);
}

#[test]
fn a_block_the_catalog_does_not_name_fails_the_range() {
    let node = Node::new("membership");
    let mut ids = node.ids.clone();
    ids[2][20] ^= 1; // block 3 differs; the boundary still matches the logs
    let genesis = node.seal("a", 6, &ids, None);
    let server = node.start(&genesis, &[]);
    let f = frames(range(&server.addr, all(1, 6)));
    let last = &f.last().unwrap().0;
    assert_eq!(last["frame"], "error", "{last}");
    assert!(
        last["message"].as_str().unwrap().contains("block 3"),
        "{last}"
    );
    assert!(f.iter().all(|(h, _)| h["frame"] != "manifest"));
}

#[test]
fn seal_mode_extends_the_chain_with_verified_segments() {
    let node = Node::new("seal");
    let genesis = node.seal("a", 3, &node.ids, None);
    std::fs::write(node.dir.join("ship.json"), ship_abi()).unwrap();
    let ship = node.dir.join("ship.json").to_str().unwrap().to_owned();
    let server = node.start(
        &genesis,
        &[
            "--seal",
            "--ship-abi",
            &ship,
            "--seal-blocks",
            "2",
            "--seal-poll-ms",
            "50",
        ],
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let info = loop {
        let info = http(&server.addr, "GET", "/v1/info", "").json();
        if info["sealedLib"] == 6 {
            break info;
        }
        assert!(
            Instant::now() < deadline,
            "the chain was not sealed: {info}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(info["catalog"]["segments"], 2);
    assert_eq!(info["seal"]["segmentsSealed"], 1);
    let segment = info["catalog"]["manifestSha256"]
        .as_str()
        .unwrap()
        .to_owned();
    let path = node
        .dir
        .join("catalogs/segments/abi-history.0000000004-0000000006.ndjson.manifest.json");
    let text = std::fs::read(&path).unwrap();
    assert_eq!(sha(&text), segment);
    let m: Value = serde_json::from_slice(&text).unwrap();
    assert_eq!(m["format"], "hyperion-abi-history-segment-v1");
    assert_eq!(
        (m["startBlock"].as_u64(), m["endBlock"].as_u64()),
        (Some(4), Some(6))
    );
    assert_eq!(m["base"]["manifestSha256"], genesis);
    let ids = std::fs::read(
        node.dir
            .join("catalogs/segments/abi-history.0000000004-0000000006.ndjson.blocks.bin"),
    )
    .unwrap();
    assert_eq!(ids, node.ids[3..6].concat());
    // The new blocks are served, sealed by the segment; the service picks the chain up again
    // after a restart.
    let f = frames(range(&server.addr, all(1, 6)));
    assert_eq!(f.last().unwrap().0["catalogSha"], segment);
    drop(server);
    let again = node.start(&genesis, &[]);
    assert_eq!(
        http(&again.addr, "GET", "/v1/info", "").json()["sealedLib"],
        6
    );
}
