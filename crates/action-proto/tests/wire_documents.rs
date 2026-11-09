//! End to end over a synthetic Wire node: a block log of real Wire `signed_block`s and a trace log
//! whose entries name those blocks' ids. Checks the 4.5 documents a run writes, and that every log
//! disagreement or invalid action ancestry fails the run.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const CHAIN_ID: &str = "9999999999999999999999999999999999999999999999999999999999999999";
const PROFILE: &str = "wire-sysio-6508c9e8-native-engineering-v1";

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

/// One action of a transaction trace.
#[derive(Clone)]
struct Action {
    ordinal: u64,
    creator: u64,
    receiver: &'static str,
    account: &'static str,
    data: Vec<u8>,
    sequence: u64,
}

/// A transaction: its packed transaction (whose sha256 is its id), CPU vector and actions.
#[derive(Clone)]
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
fn signed_block(
    n: u32,
    previous: [u8; 32],
    producer: &str,
    transactions: &[Transaction],
) -> (Vec<u8>, [u8; 32]) {
    let mut b = Vec::new();
    b.extend_from_slice(&(1_000_000 + n).to_le_bytes());
    b.extend_from_slice(&name(producer).to_le_bytes());
    b.extend_from_slice(&previous);
    b.extend_from_slice(&[0; 64]); // transaction_mroot, finality_mroot
    b.extend_from_slice(&n.saturating_sub(1).to_le_bytes());
    b.push(1); // is_strong_qc
    b.push(0); // new_finalizer_policy_diff
    b.push(0); // new_proposer_policy_diff
    varuint(&mut b, 0); // header_extensions
    let mut id: [u8; 32] = Sha256::digest(&b).into();
    id[..4].copy_from_slice(&n.to_be_bytes());
    varuint(&mut b, 1); // producer_signatures: one K1
    b.push(0);
    b.extend_from_slice(&[0x1f; 65]);
    varuint(&mut b, transactions.len() as u64);
    for t in transactions {
        varuint(&mut b, t.cpu.len() as u64);
        for &c in &t.cpu {
            varuint(&mut b, c);
        }
        varuint(&mut b, 0); // signatures
        b.push(0); // compression
        blob(&mut b, &[]);
        blob(&mut b, &t.packed);
    }
    b.push(0); // qc
    varuint(&mut b, 0); // block_extensions
    (b, id)
}

/// A `transaction_trace[]` payload, as the trace log stores it (type A, zlib).
fn traces(transactions: &[Transaction]) -> Vec<u8> {
    let mut t = Vec::new();
    varuint(&mut t, transactions.len() as u64);
    for tx in transactions {
        varuint(&mut t, 0); // transaction_trace_v0
        t.extend_from_slice(&tx.id());
        t.push(0); // status
        t.extend_from_slice(&(tx.cpu.iter().sum::<u64>() as u32).to_le_bytes());
        varuint(&mut t, 0); // net_usage_words
        t.extend_from_slice(&0i64.to_le_bytes()); // elapsed
        t.extend_from_slice(&0u64.to_le_bytes()); // net_usage
        t.push(0); // scheduled
        varuint(&mut t, tx.actions.len() as u64);
        for a in &tx.actions {
            varuint(&mut t, 0); // action_trace_v0
            varuint(&mut t, a.ordinal);
            varuint(&mut t, a.creator);
            t.push(1); // receipt
            varuint(&mut t, 0);
            t.extend_from_slice(&name(a.receiver).to_le_bytes());
            let digest: [u8; 32] = Sha256::digest([a.account.as_bytes(), &a.data].concat()).into();
            t.extend_from_slice(&digest);
            t.extend_from_slice(&a.sequence.to_le_bytes());
            t.extend_from_slice(&1u64.to_le_bytes()); // recv_sequence
            varuint(&mut t, 1);
            t.extend_from_slice(&name("alice").to_le_bytes());
            t.extend_from_slice(&7u64.to_le_bytes());
            varuint(&mut t, 1); // code_sequence
            varuint(&mut t, 2); // abi_sequence
            t.extend_from_slice(&name(a.receiver).to_le_bytes());
            t.extend_from_slice(&name(a.account).to_le_bytes());
            t.extend_from_slice(&name("transfer").to_le_bytes());
            varuint(&mut t, 1);
            t.extend_from_slice(&name("alice").to_le_bytes());
            t.extend_from_slice(&name("active").to_le_bytes());
            blob(&mut t, &a.data);
            t.push(0); // context_free
            t.extend_from_slice(&0i64.to_le_bytes()); // elapsed
            blob(&mut t, &[]); // console
            varuint(&mut t, 0); // account_ram_deltas
            t.push(0); // except
            t.push(0); // error_code
        }
        t.extend_from_slice(&[0; 5]); // account_ram_delta, except, error_code, failed_dtrx_trace, partial
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&t).unwrap();
    let mut payload = 1u32.to_le_bytes().to_vec();
    payload.extend_from_slice(&(t.len() as u64).to_le_bytes());
    payload.extend_from_slice(&z.finish().unwrap());
    payload
}

/// A node directory: blocks 1..=N in the block log, and the trace log from block 2.
struct Node {
    dir: PathBuf,
}

impl Node {
    fn new(
        test: &str,
        blocks: &[(Vec<u8>, [u8; 32])],
        traced: &[(u32, [u8; 32], Vec<u8>)],
    ) -> Self {
        let dir = std::env::temp_dir().join(format!("action-proto-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("blocks")).unwrap();
        std::fs::create_dir_all(dir.join("state-history")).unwrap();
        let (mut log, mut index) = (Vec::new(), Vec::new());
        log.extend_from_slice(&3u32.to_le_bytes());
        log.extend_from_slice(&1u32.to_le_bytes());
        log.extend_from_slice(&[0x42; 32]); // preamble
        for (bytes, _) in blocks {
            let pos = log.len() as u64;
            index.extend_from_slice(&pos.to_le_bytes());
            log.extend_from_slice(bytes);
            log.extend_from_slice(&pos.to_le_bytes());
        }
        std::fs::write(dir.join("blocks/blocks.log"), log).unwrap();
        std::fs::write(dir.join("blocks/blocks.index"), index).unwrap();
        let (mut log, mut index) = (Vec::new(), Vec::new());
        for (_, id, payload) in traced {
            let pos = log.len() as u64;
            index.extend_from_slice(&pos.to_le_bytes());
            log.extend_from_slice(&0xc35d_5000_0000_0000u64.to_le_bytes());
            log.extend_from_slice(id);
            log.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            log.extend_from_slice(payload);
            log.extend_from_slice(&pos.to_le_bytes());
        }
        std::fs::write(dir.join("state-history/trace_history.log"), log).unwrap();
        std::fs::write(dir.join("state-history/trace_history.index"), index).unwrap();
        std::fs::write(dir.join("abi.ndjson"), "").unwrap();
        Self { dir }
    }

    fn run(&self, extra: &[&str]) -> Output {
        let path = |p: &str| self.dir.join(p).to_str().unwrap().to_owned();
        Command::new(env!("CARGO_BIN_EXE_action-proto"))
            .args([
                "--protocol",
                "wire-sysio-6508c9e8-engineering-v1",
                "--start",
                "1",
                "--end",
                "100",
            ])
            .args([
                "--from-disk",
                &path("state-history"),
                "--blocks-dir",
                &path("blocks"),
            ])
            .args([
                "--abi-index",
                &path("abi.ndjson"),
                "--out",
                &path("actions.ndjson"),
            ])
            .args(["--blocks-out", &path("blocks.ndjson"), "--threads", "2"])
            .args([
                "--chain-id",
                CHAIN_ID,
                "--blocks",
                "--doc-profile-id",
                PROFILE,
            ])
            .args(extra)
            .output()
            .unwrap()
    }

    fn documents(&self, file: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(self.dir.join(file)).unwrap();
        let mut docs: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        docs.sort_by_key(|d| {
            (
                d["block_num"].as_u64(),
                d["global_sequence"].as_str().map(str::to_owned),
            )
        });
        docs
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn action(
    ordinal: u64,
    creator: u64,
    receiver: &'static str,
    account: &'static str,
    data: &[u8],
    sequence: u64,
) -> Action {
    Action {
        ordinal,
        creator,
        receiver,
        account,
        data: data.to_vec(),
        sequence,
    }
}

/// Block 2: a transfer notifying alice and bob, which sends an inline transfer. Block 4: an inline
/// action identical to its creator, sent to the same contract, so the same digest but its own input.
fn chain() -> Vec<(u32, Vec<Transaction>)> {
    let transfer = Transaction {
        packed: b"transfer transaction".to_vec(),
        cpu: vec![11, 22],
        actions: vec![
            action(1, 0, "token", "token", &[1, 2], 100),
            action(2, 1, "alice", "token", &[1, 2], 101),
            action(3, 1, "bob", "token", &[1, 2], 102),
            action(4, 1, "token", "token", &[3], 103),
        ],
    };
    let repeat = Transaction {
        packed: b"repeat transaction".to_vec(),
        cpu: vec![5],
        actions: vec![
            action(1, 0, "token", "token", &[9], 200),
            action(2, 1, "token", "token", &[9], 201),
        ],
    };
    vec![
        (1, vec![]),
        (2, vec![transfer]),
        (3, vec![]),
        (4, vec![repeat]),
    ]
}

/// Encode the chain, letting `edit` change a block's transactions in the trace log only.
fn write(
    test: &str,
    chain: &[(u32, Vec<Transaction>)],
    traced: impl Fn(u32, &[Transaction]) -> Vec<Transaction>,
) -> (Node, Vec<[u8; 32]>) {
    let mut previous = [0u8; 32];
    let mut blocks = Vec::new();
    let mut ids = Vec::new();
    let mut entries = Vec::new();
    for (n, txs) in chain {
        let (bytes, id) = signed_block(*n, previous, "prod.a", txs);
        if *n >= 2 {
            entries.push((*n, id, traces(&traced(*n, txs))));
        }
        blocks.push((bytes, id));
        ids.push(id);
        previous = id;
    }
    (Node::new(test, &blocks, &entries), ids)
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn writes_4_5_block_and_action_documents() {
    let (node, ids) = write("documents", &chain(), |_, txs| txs.to_vec());
    let out = node.run(&[]);
    assert!(out.status.success(), "{}", stderr(&out));
    let blocks = node.documents("blocks.ndjson");
    assert_eq!(blocks.len(), 4, "the untraced block 1 included");
    assert_eq!(blocks[0]["prev_id"], "0".repeat(64));
    assert_eq!(blocks[1]["block_id"], hex::encode(ids[1]));
    assert_eq!(blocks[1]["prev_id"], hex::encode(ids[0]));
    assert_eq!(blocks[1]["producer"], "prod.a");
    assert_eq!(blocks[1]["cpu_usage"], 33);
    assert_eq!(blocks[1]["trx_count"], 1);
    assert_eq!(
        blocks[1]["protocol"],
        json!({"id": "@eosrio/hyperion-wire-plugin", "profileId": PROFILE, "receipts": [{"cpu_usage_us": [11, 22]}]})
    );
    assert!(blocks[1].get("net_usage").is_none() && blocks[1].get("schedule_version").is_none());

    let actions = node.documents("actions.ndjson");
    let summary: Vec<(u64, u64, Vec<&str>)> = actions
        .iter()
        .map(|d| {
            let receivers = d["receipts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["receiver"].as_str().unwrap())
                .collect();
            (
                d["block_num"].as_u64().unwrap(),
                d["action_ordinal"].as_u64().unwrap(),
                receivers,
            )
        })
        .collect();
    assert_eq!(
        summary,
        vec![
            (2, 1, vec!["token", "alice", "bob"]),
            (2, 4, vec!["token"]),
            (4, 1, vec!["token"]),
            (4, 2, vec!["token"]),
        ]
    );
    let first = &actions[0]["protocol"];
    assert_eq!(first["schemaVersion"], 3);
    assert_eq!(
        first["source"],
        json!({"chainId": CHAIN_ID, "blockNum": 2, "blockId": hex::encode(ids[1])})
    );
    let position = format!(
        "{{\"blockId\":\"{}\",\"blockNum\":2,\"chainId\":\"{CHAIN_ID}\",\"previousBlockId\":\"{}\"}}",
        hex::encode(ids[1]),
        hex::encode(ids[0])
    );
    assert_eq!(
        first["selectionId"],
        format!(
            "h45abidiskselection1:{}",
            hex::encode(Sha256::digest(position))
        )
    );
    assert_eq!(
        first["input"],
        json!({"transactionId": hex::encode(Sha256::digest(b"transfer transaction")), "actionOrdinal": 1,
            "receiver": "token", "account": "token", "name": "transfer", "rawHex": "0102",
            "globalSequence": "100", "receiptAbiSequence": "2", "inputActionOrdinal": 1})
    );
    assert_eq!(actions[3]["protocol"]["input"]["inputActionOrdinal"], 2);
}

#[test]
fn a_block_log_that_disagrees_with_the_trace_log_fails_the_run() {
    let (node, _) = write("disagree", &chain(), |_, txs| txs.to_vec());
    // Block 3's header changes: it no longer hashes to the id the trace log names.
    let log = node.dir.join("blocks/blocks.log");
    let index = std::fs::read(node.dir.join("blocks/blocks.index")).unwrap();
    let at = u64::from_le_bytes(index[16..24].try_into().unwrap()) as usize;
    let mut bytes = std::fs::read(&log).unwrap();
    bytes[at + 4] ^= 1; // producer
    std::fs::write(&log, bytes).unwrap();
    let out = node.run(&[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("block 3: the block log's header hashes to"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_transaction_without_a_trace_fails_the_run() {
    let (node, _) = write("untraced", &chain(), |n, txs| {
        if n == 4 { vec![] } else { txs.to_vec() }
    });
    let out = node.run(&[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("block 4 includes transaction"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn an_invalid_notification_ancestry_fails_the_run() {
    let (node, _) = write("ancestry", &chain(), |n, txs| {
        let mut txs = txs.to_vec();
        if n == 2 {
            txs[0].actions[1].data = vec![7]; // alice's notification no longer replays the transfer
        }
        txs
    });
    let out = node.run(&[]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("invalid notification ancestry at action 2"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn documents_need_a_wire_protocol_and_the_block_log() {
    let (node, _) = write("arguments", &chain(), |_, txs| txs.to_vec());
    let path = |p: &str| node.dir.join(p).to_str().unwrap().to_owned();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_action-proto"))
            .args([
                "--from-disk",
                &path("state-history"),
                "--abi-index",
                &path("abi.ndjson"),
                "--start",
                "1",
                "--end",
                "9",
            ])
            .args(args)
            .output()
            .unwrap()
    };
    let antelope = run(&["--chain-id", CHAIN_ID]);
    assert!(
        stderr(&antelope).contains("select a Wire --protocol"),
        "{}",
        stderr(&antelope)
    );
    let unlogged = run(&[
        "--protocol",
        "wire-sysio-6508c9e8-engineering-v1",
        "--blocks",
    ]);
    assert!(
        stderr(&unlogged).contains("need --blocks-dir"),
        "{}",
        stderr(&unlogged)
    );
    let short = run(&[
        "--protocol",
        "wire-sysio-6508c9e8-engineering-v1",
        "--blocks-dir",
        &path("blocks"),
        "--chain-id",
        "abc",
    ]);
    assert!(
        stderr(&short).contains("64 lowercase hex"),
        "{}",
        stderr(&short)
    );
    assert!(!antelope.status.success() && !unlogged.status.success() && !short.status.success());
}
