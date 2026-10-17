//! A history split by range across servers, as a SHiP-aware router or proxy stitches it. Every
//! result a split server sends must be the bytes one server over all the logs sends, but for
//! head and LIB (bytes 1..73), the first block's `prev_block` included: it names the block
//! before the seam, which is how a client checks that two servers continue one chain. That
//! holds whether the servers share one directory (`--range`, with or without block logs, and
//! with every file ending at the seam) or each holds a copy of its own range with its block log.
//! A copy without block logs has no file naming the block before its first: startup warns,
//! `check` and `/health` say so, and that one result lacks its `prev_block`. Where several files
//! name that block, they must agree; with `--verify-ids off` only the one giving its id is
//! read, and one that cannot be read is skipped with a warning. A `have_positions` entry naming
//! that block is one the server knows.

mod common;

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};

use common::*;
use hyperion_ship::protocol::{BlockPos, Request};
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::ship::{Encoding, LogBuilder, synthetic_id};
use hyperion_ship::testkit::{FileName, SyntheticChain};
use serde_json::{Value, json};
use ship_archive::check::{self, CheckConfig, WARN_ENTRY_BYTES};
use ship_archive::store::{BlocksMode, First, Store, StoreConfig, VerifyIds};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The chain is blocks 2..121.
const END: u32 = 121;
/// Finality data starts here, as after a Savanna switch.
const FINALITY: u32 = 60;

fn chain() -> SyntheticChain {
    SyntheticChain::new(2, END - 2)
}

/// One directory tree of the whole chain as a node leaves it: trace in retained files of 40
/// blocks (2-40, 41-80, 81-120) beside an empty head log, chain_state in one head log, finality
/// from block 60 in one head log, and the block log split every 32 blocks. With `aligned`,
/// every log is split every 40 blocks, so that every file holding block 80 ends there and
/// every file holding 81 starts there.
fn shared(aligned: bool) -> Fixture {
    let fx = Fixture::new(chain());
    let (head, retained) = (fx.dir("head"), fx.dir("retained"));
    let partitioned = |kind, blocks, encoding| {
        fx.chain
            .write_ship_partitioned(&head, &retained, kind, blocks, 40, encoding)
            .unwrap();
    };
    partitioned(LogKind::Trace, 2..END, Encoding::SizedStored);
    if aligned {
        partitioned(LogKind::ChainState, 2..END, Encoding::SizedDeflate);
        partitioned(LogKind::Finality, FINALITY..END, Encoding::LegacyZero);
        fx.chain.write_blocks_split(&retained, 2..END, 40).unwrap();
    } else {
        fx.ship(&head, LogKind::ChainState, 2..END, Encoding::SizedDeflate);
        fx.ship(
            &head,
            LogKind::Finality,
            FINALITY..END,
            Encoding::LegacyZero,
        );
        fx.chain.write_blocks_split(&retained, 2..END, 32).unwrap();
    }
    fx
}

/// A copy of blocks `[begin, end)` alone in directory `name` of `fx`: every log cut to the
/// range, and with `blocks` the block log too. The store serving it.
fn trimmed(fx: &Fixture, name: &str, (begin, end): (u32, u32), blocks: bool) -> StoreConfig {
    let dir = fx.dir(name);
    fx.ship(&dir, LogKind::Trace, begin..end, Encoding::SizedStored);
    fx.ship(
        &dir,
        LogKind::ChainState,
        begin..end,
        Encoding::SizedDeflate,
    );
    let finality = begin.max(FINALITY)..end;
    if !finality.is_empty() {
        fx.ship(&dir, LogKind::Finality, finality, Encoding::LegacyZero);
    }
    if blocks {
        fx.chain
            .write_blocks(&dir, begin..end, FileName::Head)
            .unwrap();
    }
    mode(
        StoreConfig {
            dirs: vec![dir],
            ..StoreConfig::default()
        },
        blocks,
        &fx.chain,
    )
}

/// `cfg` with the block logs (`--blocks required`), or without them (`--blocks none`).
fn mode(mut cfg: StoreConfig, blocks: bool, chain: &SyntheticChain) -> StoreConfig {
    if !blocks {
        cfg.blocks = BlocksMode::None;
        cfg.chain_id = Some(chain.chain_id());
    }
    cfg
}

fn u32_at(msg: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(msg[at..at + 4].try_into().unwrap())
}

/// `got` must be `want` but for head and LIB (bytes 1..73).
fn same_but_head(what: &str, got: &[u8], want: &[u8]) {
    assert!(got.len() > 73 && want.len() > 73, "{what}: not a result");
    if got[0] != want[0] || got[73..] != want[73..] {
        let len = got.len().min(want.len());
        let at = (0..len)
            .find(|&i| !(1..73).contains(&i) && got[i] != want[i])
            .unwrap_or(len);
        panic!(
            "{what}: {} bytes, {} expected; they first differ at offset {at}",
            got.len(),
            want.len()
        );
    }
}

/// `msg`, a result with a `prev_block`, without it.
fn without_prev(msg: Vec<u8>) -> Vec<u8> {
    assert_eq!((msg[73], msg[110]), (1, 1), "this_block and prev_block");
    [&msg[..110], &[0], &msg[147..]].concat()
}

/// Ask `split` and `whole` for every block of `range`, v0 then v1 with every field. Each
/// result of `split` must be `want(n, <the result of whole>)` but for head and LIB, which are
/// `split`'s last block.
async fn compare(
    split: &Running,
    whole: &Running,
    (begin, end): (u32, u32),
    want: impl Fn(u32, Vec<u8>) -> Vec<u8>,
) {
    let (mut a, mut b) = (connect(split).await, connect(whole).await);
    for v1 in [false, true] {
        let mut req = fetch_all(blocks(begin, end, end - begin));
        req.v1 = v1;
        req.fetch_finality_data = v1;
        send(&mut a, &Request::Blocks(req.clone())).await;
        send(&mut b, &Request::Blocks(req)).await;
        for n in begin..end {
            let got = recv_binary(&mut a).await;
            let want = want(n, recv_binary(&mut b).await);
            let what = format!("v1 {v1}, block {n} of {begin}..{end}");
            same_but_head(&what, &got, &want);
            assert_eq!(
                (u32_at(&got, 1), &got[1..37]),
                (end - 1, &got[37..73]),
                "{what}: head and LIB are the last served block"
            );
        }
    }
    quiet(&mut a).await;
}

/// The `first` the store over `cfg` reports.
fn first(cfg: &StoreConfig) -> First {
    Store::open(cfg).expect("the store opens").0.first
}

/// `{n, prev_block: {n - 1, id}}` from `chain`.
fn linked(chain: &SyntheticChain, n: u32) -> First {
    First {
        num: n,
        prev: Some(BlockPos {
            num: n - 1,
            id: chain.id(n - 1),
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn servers_split_over_one_directory_send_what_one_server_sends() {
    // In the plain layout the seams fall inside files, 33 on a block-log seam; in the aligned
    // one every file holding the block before 41 and 81 ends there, and the catalogs keep it.
    for (aligned, seams) in [(false, [33, 70]), (true, [41, 81])] {
        let fx = shared(aligned);
        for blocks in [true, false] {
            let whole = start(serve_config(mode(fx.store(), blocks, &fx.chain))).await;
            let bounds = [2, seams[0], seams[1], END];
            for range in bounds.windows(2).map(|w| (w[0], w[1])) {
                let mut cfg = mode(fx.store(), blocks, &fx.chain);
                cfg.range = Some(range);
                // Block 1 is in no file: only the block log's `previous` of 2 names it.
                let want = if blocks || range.0 > 2 {
                    linked(&fx.chain, range.0)
                } else {
                    First { num: 2, prev: None }
                };
                assert_eq!(
                    first(&cfg),
                    want,
                    "aligned {aligned}, blocks {blocks}, {range:?}"
                );
                let split = start(serve_config(cfg)).await;
                compare(&split, &whole, range, |_, r| r).await;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_cut_to_their_range_send_what_one_server_sends_with_their_block_logs() {
    let fx = shared(false);
    let copies = Fixture::new(fx.chain.clone());
    for blocks in [true, false] {
        let whole = start(serve_config(mode(fx.store(), blocks, &fx.chain))).await;
        for range in [(2, 70), (70, END)] {
            let cfg = trimmed(&copies, &format!("{blocks}-{}", range.0), range, blocks);
            let split = start(serve_config(cfg)).await;
            // The block log's `previous` of block 70 names 69. Without block logs no file of
            // the right copy does: its first result alone lacks the prev_block.
            compare(&split, &whole, range, |n, r| {
                if !blocks && n == 70 {
                    without_prev(r)
                } else {
                    r
                }
            })
            .await;
        }
    }
}

/// `GET path` on the HTTP endpoints: the status code and the body.
async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
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

// One thread runs the server and the test, so the startup log lines are this thread's.
#[tokio::test]
async fn startup_check_and_health_say_whether_the_first_block_has_its_prev_block() {
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

    let fx = Fixture::new(chain());
    for blocks in [true, false] {
        let store = trimmed(&fx, &format!("{blocks}"), (70, END), blocks);
        let report = check::check(&CheckConfig {
            store: store.clone(),
            deep: false,
            hash: false,
            warn_entry_bytes: WARN_ENTRY_BYTES,
        });
        assert_eq!(report.error_count(), 0, "{}", report.text());
        let mut cfg = serve_config(store);
        cfg.http = Some(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)));
        let before = logs.0.lock().unwrap().len();
        let server = start(cfg).await;
        let startup = String::from_utf8(logs.0.lock().unwrap()[before..].to_vec()).unwrap();
        let (code, body) = get(server.http.unwrap(), "/health").await;
        assert_eq!(code, 200);
        let health: Value = serde_json::from_str(&body).unwrap();
        let coverage = report.json()["coverage"].clone();
        if blocks {
            let prev = json!({ "num": 69, "id": hex::encode(fx.chain.id(69)) });
            let want = json!({ "num": 70, "prev_block": prev });
            assert_eq!((&health["first"], &coverage["first"]), (&want, &want));
            assert_eq!(report.warning_count(), 0, "{}", report.text());
            assert!(
                report.text().contains(&format!(
                    "first block   70, prev_block 69 {}",
                    hex::encode(fx.chain.id(69))
                )),
                "{}",
                report.text()
            );
            assert!(!startup.contains("prev_block"), "{startup}");
        } else {
            let want = json!({ "num": 70, "prev_block": null });
            assert_eq!((&health["first"], &coverage["first"]), (&want, &want));
            let warning = "block 70, the first served, gets no prev_block: no open log names \
                           block 69";
            assert_eq!(report.warnings.len(), 1, "{}", report.text());
            assert!(report.warnings[0].starts_with(warning), "{}", report.text());
            assert_eq!(report.exit_code(true), 2);
            assert!(
                report.text().contains("first block   70, no prev_block"),
                "{}",
                report.text()
            );
            let lines: Vec<&str> = startup.lines().filter(|l| l.contains(warning)).collect();
            assert_eq!(lines.len(), 1, "{startup}");
            assert!(lines[0].contains("WARN"), "{startup}");
        }
        server.stop().await.unwrap();
    }
}

/// Blocks 2..121 in one directory, the trace log naming another fork's block at `forked`
/// alone, beside canonical chain_state and block logs.
fn forked_below(forked: u32) -> Fixture {
    let fx = Fixture::new(chain());
    let dir = fx.dir("node");
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for n in 2..END {
        let id = if n == forked {
            synthetic_id(n, 3)
        } else {
            fx.chain.id(n)
        };
        trace.push(id, &fx.chain.payload(LogKind::Trace, n));
    }
    trace.write(&dir, "trace_history").unwrap();
    fx.ship(&dir, LogKind::ChainState, 2..END, Encoding::SizedDeflate);
    fx.chain.write_blocks(&dir, 2..END, FileName::Head).unwrap();
    fx
}

#[test]
fn the_files_naming_the_block_before_the_first_must_agree() {
    let fx = forked_below(69);
    let open = |blocks: bool, verify_ids| {
        let mut cfg = mode(fx.store(), blocks, &fx.chain);
        cfg.range = Some((70, END));
        cfg.verify_ids = verify_ids;
        Store::open(&cfg).map(|(store, _)| store.first)
    };
    for blocks in [true, false] {
        let msg = format!("{:#}", open(blocks, VerifyIds::Seams).expect_err("refused"));
        assert!(
            msg.contains("block ids disagree at block 69")
                && msg.contains("the trace log")
                && msg.contains("the chain_state log"),
            "{msg}"
        );
    }
    // Unchecked, the block log's `previous` of block 70 (the block it sends) is the
    // prev_block; without block logs, the first log in Spring's order, the trace log.
    assert_eq!(open(true, VerifyIds::Off).unwrap(), linked(&fx.chain, 70));
    let unchecked = open(false, VerifyIds::Off).unwrap().prev.unwrap();
    assert_eq!(unchecked.id, synthetic_id(69, 3));
    // A range starting past the fork never reads it.
    let mut cfg = fx.store();
    cfg.range = Some((71, END));
    assert_eq!(first(&cfg), linked(&fx.chain, 71));

    // The block log alone disagreeing with the trace log is refused as well.
    let trace_only = forked_below(69);
    let node = trace_only.dir("node");
    std::fs::remove_file(node.join("chain_state_history.log")).unwrap();
    std::fs::remove_file(node.join("chain_state_history.index")).unwrap();
    let mut cfg = trace_only.store();
    cfg.range = Some((70, END));
    let msg = format!("{:#}", Store::open(&cfg).err().expect("refused"));
    assert!(
        msg.contains("block ids disagree at block 69") && msg.contains("(block 70 follows it)"),
        "{msg}"
    );
}

#[test]
fn files_below_the_range_that_do_not_line_up_are_set_aside() {
    // A second, forked copy of blocks 60..70 overlaps the head trace log below --range 70-121:
    // the catalog cannot reach below the range there, and the other files name block 69.
    let fx = Fixture::new(chain());
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..END, Encoding::SizedStored);
    let mut forked = LogBuilder::new(Encoding::SizedStored);
    for n in 60..70 {
        forked.push(synthetic_id(n, 3), &fx.chain.payload(LogKind::Trace, n));
    }
    forked.write(&dir, "trace_history-60-69").unwrap();
    fx.ship(&dir, LogKind::ChainState, 2..END, Encoding::SizedDeflate);
    for blocks in [false, true] {
        if blocks {
            fx.chain.write_blocks(&dir, 2..END, FileName::Head).unwrap();
        }
        let mut cfg = mode(fx.store(), blocks, &fx.chain);
        cfg.range = Some((70, END));
        assert_eq!(first(&cfg), linked(&fx.chain, 70), "blocks {blocks}");
        // Without --range the overlap refuses startup, as always.
        cfg.range = None;
        let msg = format!("{:#}", Store::open(&cfg).err().expect("refused"));
        assert!(msg.contains("overlaps"), "{msg}");
    }
}

/// Point block `n`'s slot in `index`, of a file whose first block is `first`, at the entry of
/// that first block: damage only a read of block `n` finds, as a strict open checks the last
/// slot alone.
fn misdirect(index: &Path, first: u32, n: u32) {
    let mut slots = std::fs::read(index).unwrap();
    let at = ((n - first) * 8) as usize;
    let to: [u8; 8] = slots[..8].try_into().unwrap();
    slots[at..at + 8].copy_from_slice(&to);
    std::fs::write(index, slots).unwrap();
}

#[test]
fn verify_ids_off_reads_below_the_range_only_what_gives_the_prev_block() {
    // Block 69 cannot be read from the trace log nor from the block log; chain_state is whole.
    let fx = Fixture::new(chain());
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..END, Encoding::SizedStored);
    fx.ship(&dir, LogKind::ChainState, 2..END, Encoding::SizedDeflate);
    let block_log = fx.chain.write_blocks(&dir, 2..END, FileName::Head).unwrap();
    misdirect(&dir.join("trace_history.index"), 2, 69);
    misdirect(&block_log.index, 2, 69);
    let config = |blocks: bool, verify_ids| {
        let mut cfg = mode(fx.store(), blocks, &fx.chain);
        cfg.range = Some((70, END));
        cfg.verify_ids = verify_ids;
        cfg
    };
    let open = |blocks, verify_ids| Store::open(&config(blocks, verify_ids)).map(|(s, _)| s);
    // Checked, every file naming block 69 is read, and one that cannot be refuses startup.
    for blocks in [true, false] {
        let msg = format!(
            "{:#}",
            open(blocks, VerifyIds::Seams).err().expect("refused")
        );
        assert!(
            msg.contains("trace_history") && msg.contains("block 69"),
            "{msg}"
        );
    }
    // Unchecked, the served block log names block 69 as block 70's `previous`: nothing below
    // the range is read.
    let store = open(true, VerifyIds::Off).expect("the store opens");
    assert_eq!(store.first, linked(&fx.chain, 70));
    assert_eq!(store.unlinked(), Vec::<String>::new());
    // Without block logs, the trace entry is skipped with a warning and chain_state names it.
    let skipped = "--verify-ids off: a file naming block 69, the prev_block of block 70, cannot \
                   be read and is skipped: ";
    let store = open(false, VerifyIds::Off).expect("the store opens");
    assert_eq!(store.first, linked(&fx.chain, 70));
    let warnings = store.unlinked();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with(skipped) && warnings[0].contains("trace_history"),
        "{warnings:?}"
    );

    // When the damaged file alone names block 69, block 70 gets no prev_block: startup and
    // `check` warn, and nothing is refused.
    std::fs::remove_file(dir.join("chain_state_history.log")).unwrap();
    std::fs::remove_file(dir.join("chain_state_history.index")).unwrap();
    let store = open(false, VerifyIds::Off).expect("the store opens");
    assert_eq!(
        store.first,
        First {
            num: 70,
            prev: None
        }
    );
    let warnings = store.unlinked();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(warnings[0].starts_with(skipped), "{warnings:?}");
    assert!(
        warnings[1].starts_with(
            "block 70, the first served, gets no prev_block: a file naming block 69 cannot be \
             read"
        ),
        "{warnings:?}"
    );
    let report = check::check(&CheckConfig {
        store: config(false, VerifyIds::Off),
        deep: false,
        hash: false,
        warn_entry_bytes: WARN_ENTRY_BYTES,
    });
    assert_eq!(report.error_count(), 0, "{}", report.text());
    assert_eq!(report.warnings, warnings);

    // A served block log whose `previous` of block 70 cannot be read is skipped the same way;
    // only serving block 70 fails then.
    let fx = Fixture::new(chain());
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..END, Encoding::SizedStored);
    let block_log = fx.chain.write_blocks(&dir, 2..END, FileName::Head).unwrap();
    misdirect(&block_log.index, 2, 70);
    let mut cfg = fx.store();
    cfg.range = Some((70, END));
    let msg = format!("{:#}", Store::open(&cfg).err().expect("refused"));
    assert!(msg.contains("block 70"), "{msg}");
    cfg.verify_ids = VerifyIds::Off;
    let store = Store::open(&cfg).expect("the store opens").0;
    assert_eq!(store.first.prev, None);
    let warnings = store.unlinked();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings[0].starts_with(skipped) && warnings[0].contains("blocks.log"),
        "{warnings:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_position_at_the_block_before_the_range_is_one_the_server_knows() {
    // The split server names block 69 as block 70's prev_block, so a client resuming with
    // that position starts at 70, as on one server over all the logs and on a node.
    let fx = Fixture::new(chain());
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..END, Encoding::SizedStored);
    fx.ship(&dir, LogKind::ChainState, 2..END, Encoding::SizedDeflate);
    let whole = start(serve_config(mode(fx.store(), false, &fx.chain))).await;
    let mut cfg = mode(fx.store(), false, &fx.chain);
    cfg.range = Some((70, END));
    let split = start(serve_config(cfg)).await;
    let request = |id| {
        let mut req = fetch_all(blocks(70, 72, 2));
        req.have_positions = vec![BlockPos { num: 69, id }];
        Request::Blocks(req)
    };
    let (mut a, mut b) = (connect(&split).await, connect(&whole).await);
    send(&mut a, &request(fx.chain.id(69))).await;
    send(&mut b, &request(fx.chain.id(69))).await;
    for n in 70..72 {
        let (got, want) = (recv_binary(&mut a).await, recv_binary(&mut b).await);
        assert_eq!(this_block(&got), Some(n));
        same_but_head(&format!("block {n}"), &got, &want);
    }
    quiet(&mut a).await;
    // Another id for block 69 still moves the start back to it.
    send(&mut a, &request(synthetic_id(69, 3))).await;
    let got = recv_binary(&mut a).await;
    assert_eq!(this_block(&got), None, "block 69 is not served");
    assert_eq!(this_block(&recv_binary(&mut a).await), Some(70));
    quiet(&mut a).await;
}

#[test]
fn a_history_from_block_2_needs_no_prev_block_for_it() {
    // Block 1, the genesis block, is in no state-history log, and no server sends it: without
    // block logs, block 2 has no prev_block, and that is no warning.
    let fx = Fixture::new(chain());
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..END, Encoding::SizedStored);
    fx.ship(&dir, LogKind::ChainState, 2..END, Encoding::SizedDeflate);
    let store = mode(fx.store(), false, &fx.chain);
    assert_eq!(first(&store), First { num: 2, prev: None });
    let report = check::check(&CheckConfig {
        store,
        deep: false,
        hash: false,
        warn_entry_bytes: WARN_ENTRY_BYTES,
    });
    assert_eq!(report.warning_count(), 0, "{}", report.text());
    assert_eq!(report.exit_code(true), 0);
    assert!(
        report.text().contains("first block   2, no prev_block"),
        "{}",
        report.text()
    );
}

#[test]
fn a_range_past_the_logs_is_refused_naming_no_file_kept_below_it() {
    // Every log ends at block 120, so the files holding it are kept for --range 121-200: they
    // hold no block of the range, and the refusal says so rather than naming them.
    let fx = shared(false);
    let mut cfg = mode(fx.store(), false, &fx.chain);
    cfg.range = Some((END, 200));
    let msg = format!("{:#}", Store::open(&cfg).err().expect("refused"));
    assert!(
        msg.starts_with(
            "no state-history log (trace_history, chain_state_history or finality_data_history) \
             holds a block of --range in "
        ),
        "{msg}"
    );
}
