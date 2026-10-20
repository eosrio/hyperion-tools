//! Conformance of the server with the wire behaviour of a Spring SHiP node, over synthetic
//! fixtures: an in-process server on a loopback port, driven by a tokio-tungstenite client or
//! by a raw client that sees every frame, and compared byte for byte with a model of what a
//! Spring node holding the same logs sends.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use futures::StreamExt;
use hyperion_ship::protocol::{self, BlockPos, Request, SPRING_SHIP_ABI};
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::ship::{Encoding, LogBuilder, synthetic_id};
use hyperion_ship::testkit::{FileName, SyntheticChain};
use ship_archive::SERVER_HEADER;
use ship_archive::check::{self, CheckConfig, WARN_ENTRY_BYTES};
use ship_archive::fetch::{FetchLimits, Flags, Job};
use ship_archive::store::{BlocksMode, Store, StoreConfig, VerifyIds};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

// --- T1: the ABI --------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t01_the_abi_is_the_first_message_one_text_frame() {
    let fx = Fixture::standard();
    let server = start(fx.serve()).await;

    let (mut ws, response) = connect_async(server.url()).await.unwrap();
    assert_eq!(response.headers()["server"], SERVER_HEADER);
    match recv(&mut ws).await {
        Message::Text(abi) => assert_eq!(abi.as_str(), SPRING_SHIP_ABI),
        other => panic!("expected the ABI text, got {other:?}"),
    }
    // Every later message is binary.
    assert_eq!(status(&mut ws, false).await.len(), protocol::STATUS_V0_LEN);

    // On the wire the ABI is one final text frame, whatever the path.
    let (mut raw, head) = Raw::upgrade(server.addr).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let abi = raw.frame().await.unwrap();
    assert_eq!((abi.fin, abi.opcode), (true, OP_TEXT));
    assert_eq!(abi.payload, SPRING_SHIP_ABI.as_bytes());

    // --abi sends another text, which must be JSON.
    let path = fx.dir("node.abi.json");
    let text = r#"{"version":"eosio::abi/1.1","structs":[],"variants":[]}"#;
    std::fs::write(&path, text).unwrap();
    let mut cfg = fx.serve();
    cfg.store.abi = Some(path.clone());
    let other = start(cfg).await;
    let (mut ws, _) = connect_async(other.url()).await.unwrap();
    match recv(&mut ws).await {
        Message::Text(abi) => assert_eq!(abi.as_str(), text),
        other => panic!("expected the ABI text, got {other:?}"),
    }
    std::fs::write(&path, b"\x00\x01 not json").unwrap();
    let mut cfg = fx.store();
    cfg.abi = Some(path);
    let err = Store::open(&cfg).err().expect("a non-JSON ABI is refused");
    assert!(format!("{err:#}").contains("not a JSON ABI"), "{err:#}");

    let path = fx.dir("wire.abi.json");
    std::fs::write(
        &path,
        include_str!("../../core/testdata/wire/ship.abi.json"),
    )
    .unwrap();
    let mut cfg = fx.store();
    cfg.abi = Some(path);
    let err = Store::open(&cfg)
        .err()
        .expect("a Wire ABI override cannot change archive binary layouts");
    assert!(
        format!("{err:#}").contains("unsupported_feature"),
        "{err:#}"
    );
}

// --- T2: status ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t02_status_v0_and_v1_report_last_block_ranges_and_chain_id() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;

    let v0 = status(&mut ws, false).await;
    assert_eq!((v0.len(), v0[0]), (121, 0));
    assert_eq!(v0, model.status(false));
    let v1 = status(&mut ws, true).await;
    assert_eq!((v1.len(), v1[0]), (129, 3));
    assert_eq!(v1, model.status(true));

    // Field by field, at Spring's offsets: head = LIB = {L*, id(L*)}, exclusive range ends.
    let u32_at = |b: &[u8], at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let last = fx.chain.id(200);
    assert_eq!((u32_at(&v1, 1), &v1[5..37]), (200, &last[..]));
    assert_eq!((u32_at(&v1, 37), &v1[41..73]), (200, &last[..]));
    assert_eq!((u32_at(&v1, 73), u32_at(&v1, 77)), (2, 201));
    assert_eq!((u32_at(&v1, 81), u32_at(&v1, 85)), (2, 201));
    assert_eq!(&v1[89..121], &fx.chain.chain_id()[..]);
    assert_eq!((u32_at(&v1, 121), u32_at(&v1, 125)), (100, 201));
    assert_eq!(v0[1..], v1[1..121], "v0 is v1 without the finality range");

    // A log not served reports 0,0.
    let mut cfg = fx.serve();
    cfg.store.range = Some((10, 90));
    let other = start(cfg).await;
    let mut ws = connect(&other).await;
    let v1 = status(&mut ws, true).await;
    assert_eq!((u32_at(&v1, 1), u32_at(&v1, 37)), (89, 89));
    assert_eq!((u32_at(&v1, 121), u32_at(&v1, 125)), (0, 0));
}

// --- T3: result variants ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t03_result_variants_follow_the_request_version() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;

    let mut v0 = blocks(150, 151, 1);
    v0.fetch_traces = true;
    send(&mut ws, &Request::Blocks(v0.clone())).await;
    let msg = recv_binary(&mut ws).await;
    assert_eq!(msg[0], 1);
    assert_eq!(msg, model.result(150, &v0));

    for fetch_finality_data in [false, true] {
        let mut v1 = blocks(150, 151, 1);
        v1.v1 = true;
        v1.fetch_traces = true;
        v1.fetch_finality_data = fetch_finality_data;
        send(&mut ws, &Request::Blocks(v1.clone())).await;
        let msg = recv_binary(&mut ws).await;
        assert_eq!(msg[0], 2, "v1 answers with get_blocks_result_v1");
        assert_eq!(msg, model.result(150, &v1));
    }
    quiet(&mut ws).await;
}

// --- T4: credit ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t04_a_request_replaces_the_credit_and_acks_add_to_it() {
    let fx = Fixture::standard();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;
    async fn numbers(ws: &mut Ws, count: usize) -> Vec<u32> {
        let mut got = Vec::new();
        for _ in 0..count {
            got.push(this_block(&recv_binary(ws).await).unwrap());
        }
        got
    }

    send(&mut ws, &Request::Blocks(blocks(10, 100, 3))).await;
    assert_eq!(numbers(&mut ws, 3).await, [10, 11, 12]);
    quiet(&mut ws).await;
    send(&mut ws, &Request::Ack(2)).await;
    assert_eq!(numbers(&mut ws, 2).await, [13, 14]);
    quiet(&mut ws).await;

    // A new request replaces credit and cursor: the 1 here is all there is.
    send(&mut ws, &Request::Blocks(blocks(50, 100, 1))).await;
    assert_eq!(numbers(&mut ws, 1).await, [50]);
    quiet(&mut ws).await;

    // Credit 0 sends nothing until an ack; acks sum up.
    send(&mut ws, &Request::Blocks(blocks(60, 100, 0))).await;
    quiet(&mut ws).await;
    send(&mut ws, &Request::Ack(1)).await;
    send(&mut ws, &Request::Ack(2)).await;
    assert_eq!(numbers(&mut ws, 3).await, [60, 61, 62]);
    quiet(&mut ws).await;

    // Status replies cost no credit.
    send(&mut ws, &Request::Blocks(blocks(70, 100, 1))).await;
    assert_eq!(numbers(&mut ws, 1).await, [70]);
    assert_eq!(status(&mut ws, false).await.len(), 121);
    quiet(&mut ws).await;
    // (That an ack wraps at u32 is a unit test of the session state: a frozen server cannot
    // hold its cursor still between two acks to show it on the wire.)
}

// --- T5: have_positions -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t05_have_positions_rewind_to_unknown_or_different_ids_only() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;
    let pos = |num: u32, id: [u8; 32]| BlockPos { num, id };
    let known = |n: u32| pos(n, fx.chain.id(n));
    let wrong = |n: u32| pos(n, synthetic_id(n, 9));

    let cases = [
        // Matching ids below start, and anything at or above it, change nothing.
        (vec![known(90), known(95), wrong(100), wrong(150)], 100),
        // A different id moves the start back to it.
        (vec![known(90), wrong(95)], 95),
        // The lowest mismatch wins, in any order.
        (vec![wrong(80), known(70), wrong(60)], 60),
        (vec![wrong(60), wrong(80)], 60),
        // An id this server does not know (block 0) counts as different.
        (vec![known(50), pos(0, [7; 32])], 0),
        // Block 1 is in no state-history log, but the block log knows its id.
        (vec![known(1)], 100),
        (vec![wrong(1)], 1),
    ];
    for (positions, first) in cases {
        let mut req = blocks(100, 201, 1);
        req.fetch_traces = true;
        req.have_positions = positions.clone();
        send(&mut ws, &Request::Blocks(req.clone())).await;
        let msg = recv_binary(&mut ws).await;
        req.have_positions.clear();
        assert_eq!(msg, model.result(first, &req), "positions {positions:?}");
        quiet(&mut ws).await;
    }
}

// --- T6: the end of a request -------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t06_at_the_end_the_session_idles_open_and_takes_a_new_request() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;

    // An end past L* stops at L*.
    let req = fetch_all(blocks(195, 1000, 100));
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 195..=200 {
        assert_eq!(recv_binary(&mut ws).await, model.result(n, &req));
    }
    if let Ok(msg) = tokio::time::timeout(Duration::from_millis(500), ws.next()).await {
        panic!("a frame after the last block: {msg:?}");
    }
    assert_eq!(status(&mut ws, true).await, model.status(true));

    let req = fetch_all(blocks(10, 12, 100));
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 10..12 {
        assert_eq!(recv_binary(&mut ws).await, model.result(n, &req));
    }
    quiet(&mut ws).await;
}

// --- T7: outside the served range ---------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t07_past_the_last_block_idles_and_below_the_first_sends_idless_results() {
    let fx = Fixture::standard();
    let mut cfg = fx.serve();
    cfg.store.range = Some((50, 150));
    let model = Model {
        chain: &fx.chain,
        trace: 50..150,
        chain_state: 50..150,
        finality: 100..150,
        blocks: 50..150,
    };
    let server = start(cfg).await;
    let mut ws = connect(&server).await;

    send(&mut ws, &Request::Blocks(fetch_all(blocks(150, 300, 10)))).await;
    quiet(&mut ws).await;
    assert_eq!(status(&mut ws, false).await, model.status(false));

    // Below the first block, a result per block, id-less, each costing a credit.
    let req = fetch_all(blocks(45, 53, 3));
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 45..48 {
        let msg = recv_binary(&mut ws).await;
        assert_eq!(msg.len(), 78, "an id-less v0 result is 78 bytes");
        assert_eq!(msg, model.result(n, &req));
    }
    quiet(&mut ws).await;
    send(&mut ws, &Request::Ack(5)).await;
    // 48 is id-less; 49's id is the block log's `previous` of 50, with nothing else known;
    // 50 is the first served block, its `prev_block` from the block log.
    for n in 48..53 {
        assert_eq!(
            recv_binary(&mut ws).await,
            model.result(n, &req),
            "block {n}"
        );
    }
    quiet(&mut ws).await;

    let mut v1 = blocks(40, 41, 1);
    v1.v1 = true;
    send(&mut ws, &Request::Blocks(v1.clone())).await;
    let msg = recv_binary(&mut ws).await;
    assert_eq!(
        (msg.len(), msg[0]),
        (79, 2),
        "an id-less v1 result is 79 bytes"
    );
    assert_eq!(msg, model.result(40, &v1));
}

// --- T8: fetch flags ----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t08_every_fetch_flag_combination_matches_spring() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;
    for v1 in [false, true] {
        for flags in 0..16u8 {
            // 150 has every log; 60 has no finality data.
            for n in [150, 60] {
                let mut req = blocks(n, n + 1, 1);
                req.v1 = v1;
                req.fetch_block = flags & 1 != 0;
                req.fetch_traces = flags & 2 != 0;
                req.fetch_deltas = flags & 4 != 0;
                req.fetch_finality_data = v1 && flags & 8 != 0;
                send(&mut ws, &Request::Blocks(req.clone())).await;
                let msg = recv_binary(&mut ws).await;
                assert_eq!(
                    msg,
                    model.result(n, &req),
                    "v1 {v1}, flags {flags:04b}, block {n}"
                );
            }
        }
    }
    quiet(&mut ws).await;
}

// --- T9: seams ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t09_ids_are_continuous_across_file_seams_and_at_the_first_block() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;

    let mut req = fetch_all(blocks(1, 201, 1000));
    req.v1 = true;
    req.fetch_finality_data = true;
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 1..201 {
        let msg = recv_binary(&mut ws).await;
        assert_eq!(msg, model.result(n, &req), "block {n}");
        if n >= 2 {
            // prev_block is present and is the id of the block before, also at block 2 (from
            // the block log) and at the trace seams 51, 101, 151 and the block seams 65, 129, 193.
            assert_eq!(msg[110], 1, "block {n} has prev_block");
            assert_eq!(u32::from_le_bytes(msg[111..115].try_into().unwrap()), n - 1);
            assert_eq!(&msg[115..147], &fx.chain.id(n - 1)[..]);
        }
    }
    quiet(&mut ws).await;
}

// --- T10: --blocks none -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t10_without_block_logs_ids_come_from_the_state_history_logs() {
    let fx = Fixture::standard();
    let mut cfg = fx.serve();
    cfg.store.blocks = BlocksMode::None;
    cfg.store.chain_id = Some(fx.chain.chain_id());
    let model = Model {
        blocks: 0..0,
        ..fx.standard_model()
    };
    let server = start(cfg).await;
    let mut ws = connect(&server).await;
    assert_eq!(status(&mut ws, true).await, model.status(true));

    let req = fetch_all(blocks(1, 6, 100));
    send(&mut ws, &Request::Blocks(req.clone())).await;
    let mut msgs = Vec::new();
    for n in 1..6 {
        let msg = recv_binary(&mut ws).await;
        assert_eq!(msg, model.result(n, &req), "block {n}");
        msgs.push(msg);
    }
    // Nothing knows block 1: it is id-less, and block 2 has no prev_block.
    assert_eq!(msgs[0].len(), 78);
    assert_eq!((msgs[1][73], msgs[1][110]), (1, 0));
    assert_eq!(msgs[2][110], 1);

    // --blocks none needs the chain id; with block logs it must match theirs.
    let mut cfg = fx.store();
    cfg.blocks = BlocksMode::None;
    let err = Store::open(&cfg).err().unwrap();
    assert!(
        format!("{err:#}").contains("--chain-id is required"),
        "{err:#}"
    );
    let mut cfg = fx.store();
    cfg.chain_id = Some([1; 32]);
    let err = Store::open(&cfg).err().unwrap();
    assert!(format!("{err:#}").contains("not to --chain-id"), "{err:#}");
    cfg.chain_id = Some(fx.chain.chain_id());
    Store::open(&cfg).unwrap();
}

// --- T11: streamed payloads ---------------------------------------------------------------

const BIG: usize = (5 << 20) / 2;

/// Blocks 2..12 with a 2.5 MiB trace entry at 5 (Spring stored, Type A) and a 2.5 MiB delta
/// entry at 6 (EOSIO 2.0 framing, Type B, no declared size), an empty delta entry at 8, and
/// optionally both large entries' checksums broken.
fn large_fixture(corrupt: bool) -> Fixture {
    let mut chain = SyntheticChain::new(2, 10);
    chain
        .set_payload_len(LogKind::Trace, 5, BIG)
        .set_payload_len(LogKind::ChainState, 6, BIG)
        .set_payload_len(LogKind::ChainState, 8, 0);
    let fx = Fixture::new(chain);
    let dir = fx.dir("node");
    let mut trace = fx
        .chain
        .ship_log(LogKind::Trace, 2..12, Encoding::SizedStored);
    let mut deltas = fx
        .chain
        .ship_log(LogKind::ChainState, 2..12, Encoding::LegacyLength);
    if corrupt {
        trace.corrupt_checksum(5);
        deltas.corrupt_checksum(6);
    }
    trace.write(&dir, "trace_history").unwrap();
    deltas.write(&dir, "chain_state_history").unwrap();
    fx.chain.write_blocks(&dir, 2..12, FileName::Head).unwrap();
    fx
}

fn large_model(fx: &Fixture) -> Model<'_> {
    Model {
        chain: &fx.chain,
        trace: 2..12,
        chain_state: 2..12,
        finality: 0..0,
        blocks: 2..12,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t11_payloads_over_the_threshold_stream_in_fragments_and_reassemble() {
    let fx = large_fixture(false);
    let model = large_model(&fx);
    let mut cfg = fx.serve();
    cfg.stream_threshold = 256 << 10;
    let server = start(cfg).await;
    let mut raw = Raw::connect(server.addr).await;

    let mut req = fetch_all(blocks(4, 10, 100));
    req.v1 = true;
    raw.send_request(&Request::Blocks(req.clone())).await;
    for n in 4..10 {
        let frames = raw.message().await.expect("a result");
        assert_eq!(joined(&frames), model.result(n, &req), "block {n}");
        let (variant, _base, fields) = pieces(&frames);
        assert_eq!((variant, fields.len()), (2, 3));
        for (head, data) in &fields {
            if let Some((_, full)) = data.split_last() {
                assert!(full.iter().all(|c| c.len() == 1 << 20), "chunks fill 1 MiB");
            }
            assert!(data.iter().all(|c| !c.is_empty() && c.len() <= 1 << 20));
            if head == &[0] {
                assert!(data.is_empty());
            }
        }
        let chunks: Vec<usize> = fields.iter().map(|(_, data)| data.len()).collect();
        match n {
            5 => assert_eq!(chunks, [3, 1, 0], "2.5 MiB of traces in 3 frames"),
            6 => assert_eq!(chunks, [1, 3, 0], "2.5 MiB of deltas in 3 frames"),
            8 => {
                // An empty payload: present, length 0, no data frame.
                assert_eq!(fields[1], (vec![1, 0], vec![]));
                assert_eq!(chunks, [1, 0, 0]);
            }
            _ => assert_eq!(chunks, [1, 1, 0]),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t11_large_blocks_go_out_in_1_mib_frames_and_over_20_mib_drop_the_connection() {
    use hyperion_ship::testkit::blocks::{self, BlockSpec, Preamble};
    // Blocks 2..12; block 5 carries 1.5 MiB of extensions, block 7 more than 20 MiB.
    let chain = blocks::extend(&blocks::synthetic_id(1), 10, |n| {
        let mut spec = BlockSpec::synthetic(n);
        match n {
            5 => spec.block_extensions = vec![(0, vec![5; 3 << 19])],
            7 => spec.block_extensions = vec![(0, vec![7; protocol::MAX_BLOCK_BYTES])],
            _ => {}
        }
        spec
    })
    .unwrap();
    let fx = Fixture::new(SyntheticChain::new(2, 1));
    let dir = fx.dir("node");
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for b in &chain {
        trace.push(b.id, b"trace");
    }
    trace.write(&dir, "trace_history").unwrap();
    let preamble = Preamble::V3ChainId {
        first: 2,
        chain_id: fx.chain.chain_id(),
    };
    std::fs::create_dir_all(&dir).unwrap();
    blocks::write(&dir, "blocks", &preamble, &chain).unwrap();
    let server = start(fx.serve()).await;
    let head = BlockPos {
        num: 11,
        id: chain[9].id,
    };
    let pos = |n: u32| BlockPos {
        num: n,
        id: chain[n as usize - 2].id,
    };

    let mut raw = Raw::connect(server.addr).await;
    let mut req = blocks(4, 8, 10);
    req.fetch_block = true;
    raw.send_request(&Request::Blocks(req)).await;
    for n in [4, 5, 6] {
        let frames = raw.message().await.expect("a result");
        let body = &chain[n as usize - 2].body;
        let mut want = vec![1];
        want.extend(protocol::encode_result_base(
            head,
            head,
            Some(pos(n)),
            Some(pos(n - 1)),
            Some(body),
        ));
        want.extend([0, 0]);
        assert_eq!(joined(&frames), want, "block {n}");
        assert!(frames.iter().all(|f| f.payload.len() <= 1 << 20));
        // variant, base (in 1 MiB frames), two absent fields, the final frame.
        let base_frames = frames.len() - 4;
        assert_eq!(base_frames, if n == 5 { 2 } else { 1 }, "block {n}");
    }
    assert_eq!(
        raw.frame().await,
        None,
        "block 7 is too large for a SHiP result"
    );
}

// --- T12, T13: integrity ------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t12_a_corrupt_inline_entry_drops_the_connection_before_its_first_frame() {
    let fx = Fixture::new(SyntheticChain::new(2, 20));
    let dir = fx.dir("node");
    let mut trace = fx
        .chain
        .ship_log(LogKind::Trace, 2..22, Encoding::LegacyLength);
    trace.corrupt_checksum(15);
    trace.write(&dir, "trace_history").unwrap();
    let mut deltas = fx
        .chain
        .ship_log(LogKind::ChainState, 2..22, Encoding::SizedDeflate);
    deltas.corrupt_checksum(10);
    deltas.write(&dir, "chain_state_history").unwrap();
    fx.chain.write_blocks(&dir, 2..22, FileName::Head).unwrap();
    let model = Model {
        chain: &fx.chain,
        trace: 2..22,
        chain_state: 2..22,
        finality: 0..0,
        blocks: 2..22,
    };
    let server = start(fx.serve()).await;

    for (field, damaged) in [("deltas", 10), ("traces", 15)] {
        let mut raw = Raw::connect(server.addr).await;
        let mut req = blocks(damaged - 2, damaged + 2, 10);
        req.fetch_deltas = field == "deltas";
        req.fetch_traces = field == "traces";
        raw.send_request(&Request::Blocks(req.clone())).await;
        for n in damaged - 2..damaged {
            let frames = raw.message().await.expect("an intact result");
            assert_eq!(joined(&frames), model.result(n, &req), "block {n}");
        }
        assert_eq!(
            raw.frame().await,
            None,
            "no frame of block {damaged}'s result, and no Close"
        );
    }

    // What does not read the damaged payload is served.
    let mut raw = Raw::connect(server.addr).await;
    let mut req = blocks(9, 12, 10);
    req.fetch_traces = true;
    req.fetch_block = true;
    raw.send_request(&Request::Blocks(req.clone())).await;
    for n in 9..12 {
        let frames = raw.message().await.expect("a result");
        assert_eq!(joined(&frames), model.result(n, &req), "block {n}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t13_a_corrupt_streamed_entry_is_caught_before_its_first_frame() {
    let fx = large_fixture(true);
    let model = large_model(&fx);
    let mut cfg = fx.serve();
    cfg.stream_threshold = 256 << 10;
    let server = start(cfg).await;

    // Type A, its size declared: the counting pass catches the checksum.
    let mut raw = Raw::connect(server.addr).await;
    let mut req = blocks(4, 8, 10);
    req.fetch_traces = true;
    raw.send_request(&Request::Blocks(req.clone())).await;
    let frames = raw.message().await.expect("block 4");
    assert_eq!(joined(&frames), model.result(4, &req));
    assert_eq!(raw.frame().await, None, "no frame of block 5's result");

    // Type B: the inline read stops at the threshold, the counting pass finds the damage.
    let mut raw = Raw::connect(server.addr).await;
    let mut req = blocks(6, 8, 10);
    req.fetch_deltas = true;
    raw.send_request(&Request::Blocks(req)).await;
    assert_eq!(raw.frame().await, None, "no frame of block 6's result");

    let mut raw = Raw::connect(server.addr).await;
    let req = fetch_all(blocks(7, 10, 10));
    raw.send_request(&Request::Blocks(req.clone())).await;
    for n in 7..10 {
        let frames = raw.message().await.expect("a result");
        assert_eq!(joined(&frames), model.result(n, &req), "block {n}");
    }
}

/// A trace log of blocks `blocks` whose entries for `forked` name another fork's blocks, with
/// the chain's payloads.
fn forked_trace(fx: &Fixture, blocks: std::ops::Range<u32>, forked: std::ops::Range<u32>) {
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for n in blocks {
        let id = if forked.contains(&n) {
            synthetic_id(n, 3)
        } else {
            fx.chain.id(n)
        };
        trace.push(id, &fx.chain.payload(LogKind::Trace, n));
    }
    trace.write(&fx.dir("node"), "trace_history").unwrap();
}

/// Block `n`'s v0 result with traces and, if `block`, the block.
fn job(n: u32, block: bool) -> Job {
    Job {
        generation: 1,
        n,
        v1: false,
        flags: Flags {
            block,
            traces: true,
            deltas: false,
            finality: false,
        },
    }
}

/// `f` on a thread of its own, which must return within [`WAIT`]: a regression that spins
/// fails the test instead of hanging the run.
fn within<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(WAIT).expect("returned in time")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t12_a_block_that_does_not_hash_to_this_block_drops_the_connection() {
    // Trace entries naming another fork's blocks 15 to 20, away from every seam, over the
    // canonical block log: the ids agree at the edges, so the store opens.
    let fx = Fixture::new(SyntheticChain::new(2, 39));
    forked_trace(&fx, 2..41, 15..21);
    fx.chain
        .write_blocks(&fx.dir("node"), 2..41, FileName::Head)
        .unwrap();
    let model = Model {
        chain: &fx.chain,
        trace: 2..41,
        chain_state: 0..0,
        finality: 0..0,
        blocks: 2..41,
    };
    let server = start(fx.serve()).await;

    // Block 15 follows block 14, but it is not the block 15 the trace log names: Spring never
    // attaches such a block, and coverage promises one, so the result is never started.
    let mut raw = Raw::connect(server.addr).await;
    let mut req = blocks(13, 18, 10);
    req.fetch_block = true;
    req.fetch_traces = true;
    raw.send_request(&Request::Blocks(req.clone())).await;
    for n in 13..15 {
        let frames = raw.message().await.expect("an intact result");
        assert_eq!(joined(&frames), model.result(n, &req), "block {n}");
    }
    assert_eq!(
        raw.frame().await,
        None,
        "no frame of block 15's result, and no Close"
    );
}

#[test]
fn a_block_goes_out_only_under_its_own_id() {
    // Only `L*`, 40, from another fork: no block after it names its id.
    let fx = Fixture::new(SyntheticChain::new(2, 39));
    forked_trace(&fx, 2..41, 40..41);
    fx.chain
        .write_blocks(&fx.dir("node"), 2..41, FileName::Head)
        .unwrap();
    // The startup check sees it through block 40's header hash.
    let msg = format!("{:#}", Store::open(&fx.store()).err().expect("refused"));
    assert!(
        msg.contains("block ids disagree at block 40")
            && msg.contains("the hash of block 40's header"),
        "{msg}"
    );
    // Past it, the read of the block does.
    let mut cfg = fx.store();
    cfg.verify_ids = VerifyIds::Off;
    let (store, _) = Store::open(&cfg).unwrap();
    assert_eq!(store.last.id, synthetic_id(40, 3));
    let limits = FetchLimits {
        stream_threshold: 1 << 20,
    };
    store.build(&job(39, true), &limits).expect("block 39");
    let msg = format!(
        "{:#}",
        store.build(&job(40, true), &limits).expect_err("refused")
    );
    assert!(msg.contains("block 40's header hashes to"), "{msg}");
    store
        .build(&job(40, false), &limits)
        .expect("without its block, nothing contradicts the trace log");
}

#[test]
fn t13_a_size_field_of_u64_max_fails_the_read_instead_of_spinning() {
    // Block 5's trace entry declares u64::MAX bytes over an intact stream.
    let fx = Fixture::new(SyntheticChain::new(2, 10));
    let dir = fx.dir("node");
    let mut trace = fx
        .chain
        .ship_log(LogKind::Trace, 2..12, Encoding::SizedStored);
    let at = trace.position(5) as usize + 52;
    trace.log_mut()[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
    trace.write(&dir, "trace_history").unwrap();
    fx.chain.write_blocks(&dir, 2..12, FileName::Head).unwrap();
    let declared = format!("its header declares {}", u64::MAX);

    let store = Arc::new(Store::open(&fx.store()).unwrap().0);
    let limits = FetchLimits {
        stream_threshold: 4 << 20,
    };
    for (n, block) in [(4, true), (5, true), (5, false)] {
        let (s, l) = (Arc::clone(&store), limits);
        let built = within(move || s.build(&job(n, block), &l).map(|_| ()));
        if n == 4 {
            built.expect("block 4");
        } else {
            let msg = format!("{:#}", built.expect_err("refused"));
            assert!(msg.contains(&declared) && msg.contains("block 5"), "{msg}");
        }
    }
    let cfg = CheckConfig {
        store: fx.store(),
        deep: true,
        hash: false,
        warn_entry_bytes: WARN_ENTRY_BYTES,
    };
    let report = within(move || check::check(&cfg));
    assert_eq!(report.error_count(), 1, "{}", report.text());
    assert!(report.errors[0].contains(&declared), "{}", report.text());
}

// --- T14: capacity ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t14_upgrades_beyond_max_sessions_get_503_with_retry_after() {
    let fx = Fixture::standard();
    let mut cfg = fx.serve();
    cfg.max_sessions = 1;
    let server = start(cfg).await;
    let first = connect(&server).await;

    match connect_async(server.url()).await {
        Err(WsError::Http(response)) => {
            assert_eq!(response.status(), 503);
            assert_eq!(response.headers()["retry-after"], "1");
            assert_eq!(response.headers()["server"], SERVER_HEADER);
        }
        other => panic!("expected HTTP 503, got {other:?}"),
    }
    let (_, head) = Raw::upgrade(server.addr).await;
    assert!(head.starts_with("HTTP/1.1 503"), "{head}");

    // The slot frees once the server sees the first session go.
    drop(first);
    let deadline = Instant::now() + WAIT;
    loop {
        match connect_async(server.url()).await {
            Ok(_) => break,
            Err(WsError::Http(r)) if r.status() == 503 && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
}

// --- T15: shutdown ------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t15_shutdown_sends_close_1001_between_messages() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut idle = connect(&server).await;
    let mut streaming = connect(&server).await;
    let req = fetch_all(blocks(2, 201, 1000));
    send(&mut streaming, &Request::Blocks(req.clone())).await;
    assert_eq!(recv_binary(&mut streaming).await, model.result(2, &req));

    let stopping = tokio::spawn(server.stop());
    for ws in [&mut idle, &mut streaming] {
        let mut n = 3;
        loop {
            match recv(ws).await {
                // Whole results, in order, until the Close.
                Message::Binary(msg) => {
                    assert_eq!(msg.to_vec(), model.result(n, &req));
                    n += 1;
                }
                Message::Close(Some(frame)) => {
                    assert_eq!(frame.code, CloseCode::Away);
                    break;
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        // Reading on answers the Close and ends the stream.
        assert!(matches!(ws.next().await, None | Some(Err(_))));
    }
    stopping.await.unwrap().unwrap();
}

// --- T16, T17: protocol errors ------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t16_malformed_requests_drop_the_connection_without_close() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let mut cfg = fx.serve();
    cfg.max_request_bytes = 1024;
    let server = start(cfg).await;

    let mut v1 = blocks(2, 5, 3);
    v1.v1 = true;
    let mut bool_of_two = protocol::encode_request(&Request::Blocks(v1));
    let at = bool_of_two.len() - 2;
    bool_of_two[at] = 2;
    let mut too_long = blocks(2, 5, 3);
    too_long.have_positions = vec![BlockPos::default(); 40];
    let too_long = protocol::encode_request(&Request::Blocks(too_long));
    assert!(too_long.len() > 1024);
    let cases = [
        ("a bool of 2", bool_of_two),
        ("unknown variant 9", vec![9]),
        ("variant 5, padded", vec![0x85, 0x80, 0x80, 0x80, 0x00]),
        ("a cut request", vec![1, 2, 0, 0]),
        ("an ack without its count", vec![2, 1]),
        ("a request over --max-request-bytes", too_long),
    ];
    for (what, bytes) in cases {
        let mut raw = Raw::connect(server.addr).await;
        raw.send(&bytes).await;
        assert_eq!(raw.frame().await, None, "{what}: dropped without a frame");
    }

    // A malformed request after results leaves them whole; trailing bytes are ignored.
    let mut raw = Raw::connect(server.addr).await;
    let req = fetch_all(blocks(2, 4, 10));
    let mut padded = protocol::encode_request(&Request::Blocks(req.clone()));
    padded.extend_from_slice(b"ignored");
    raw.send(&padded).await;
    for n in 2..4 {
        assert_eq!(joined(&raw.message().await.unwrap()), model.result(n, &req));
    }
    raw.send(&[9]).await;
    assert_eq!(raw.frame().await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t17_more_than_1024_queued_status_requests_drop_the_connection() {
    let fx = Fixture::standard();
    let model = fx.standard_model();
    let server = start(fx.serve()).await;
    let mut raw = Raw::connect(server.addr).await;

    // Never read: once the socket buffers are full the writer blocks, the queue grows past
    // 1024, and the server drops the connection, which makes the sends fail.
    let batch: Vec<u8> = (0..1000).flat_map(|_| masked(&[0])).collect();
    let mut sent = 0;
    while sent < 4_000_000 && raw.send_bytes(&batch).await {
        sent += 1000;
    }
    assert!(
        sent < 4_000_000,
        "the server kept reading {sent} status requests"
    );

    // What was sent before the drop is whole status replies, and no Close follows them.
    let rest = raw.rest().await;
    let reply = {
        let mut frame = vec![0x82, 121];
        frame.extend(model.status(false));
        frame
    };
    let whole = rest.len() / reply.len();
    assert!(
        rest.chunks(reply.len()).take(whole).all(|f| f == reply),
        "only status replies"
    );
    let tail = &rest[whole * reply.len()..];
    assert!(
        tail.len() < reply.len() && reply.starts_with(tail),
        "no Close frame"
    );
}

// --- T20: a new request mid-stream --------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t20_a_new_request_mid_stream_drops_what_was_built_ahead() {
    // 400 blocks of 64 KiB traces from block 400, far more than socket buffers hold.
    let mut chain = SyntheticChain::new(2, 799);
    for n in 400..800 {
        chain.set_payload_len(LogKind::Trace, n, 64 << 10);
    }
    let fx = Fixture::new(chain);
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..801, Encoding::SizedStored);
    fx.chain.write_blocks(&dir, 2..801, FileName::Head).unwrap();
    let server = start(fx.serve()).await;
    let mut ws = connect(&server).await;

    let mut old = blocks(400, 800, 1000);
    old.fetch_traces = true;
    send(&mut ws, &Request::Blocks(old)).await;
    assert_eq!(this_block(&recv_binary(&mut ws).await), Some(400));
    assert_eq!(this_block(&recv_binary(&mut ws).await), Some(401));
    // Let the writer fill the socket and block in the middle of the old request.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut new = blocks(20, 23, 3);
    new.fetch_traces = true;
    send(&mut ws, &Request::Blocks(new)).await;

    let mut old_after = 0;
    loop {
        let n = this_block(&recv_binary(&mut ws).await).unwrap();
        if n == 20 {
            break;
        }
        assert!((402..800).contains(&n), "block {n} from the old request");
        old_after += 1;
    }
    assert!(
        old_after < 398,
        "the new request arrived after the old one was all sent"
    );
    for want in [21, 22] {
        assert_eq!(this_block(&recv_binary(&mut ws).await), Some(want));
    }
    quiet(&mut ws).await;
}

// --- Startup ------------------------------------------------------------------------------

#[test]
fn startup_refuses_ids_that_disagree_at_a_seam() {
    // A trace file from another fork between 51 and 100.
    let fx = Fixture::new(SyntheticChain::new(2, 199));
    let dir = fx.dir("node");
    for (blocks, stem) in [(2..51, "trace_history-2-50"), (101..201, "trace_history")] {
        let mut b = LogBuilder::new(Encoding::SizedStored);
        for n in blocks {
            b.push(fx.chain.id(n), &fx.chain.payload(LogKind::Trace, n));
        }
        b.write(&dir, stem).unwrap();
    }
    let mut forked = LogBuilder::new(Encoding::SizedStored);
    for n in 51..101 {
        forked.push(synthetic_id(n, 3), &fx.chain.payload(LogKind::Trace, n));
    }
    forked.write(&dir, "trace_history-51-100").unwrap();
    fx.chain.write_blocks(&dir, 2..201, FileName::Head).unwrap();

    let err = Store::open(&fx.store()).err().expect("refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("block ids disagree at block 51"), "{msg}");

    let mut cfg = fx.store();
    cfg.verify_ids = ship_archive::store::VerifyIds::Off;
    Store::open(&cfg).expect("--verify-ids off serves anyway");
}

#[test]
fn startup_checks_the_end_of_the_served_range_against_the_block_log() {
    // A trace log from another fork from block 29 on, over the canonical block log 2..61.
    let fx = Fixture::new(SyntheticChain::new(2, 59));
    forked_trace(&fx, 2..61, 29..61);
    fx.chain
        .write_blocks(&fx.dir("node"), 2..61, FileName::Head)
        .unwrap();
    let open = |range| {
        let mut cfg = fx.store();
        cfg.range = range;
        Store::open(&cfg).map(|(store, _)| store)
    };
    // A --range ending inside the fork, or the block log ending with the trace log (as copied
    // from a node): the block log serves no block after the last one, whose header hash is
    // the block log's id for it. Blocks past --range are never read.
    for (range, last) in [(Some((2, 30)), 29), (Some((10, 31)), 30), (None, 60)] {
        let msg = format!("{:#}", open(range).err().expect("refused"));
        assert!(
            msg.contains(&format!("block ids disagree at block {last}"))
                && msg.contains(&format!("the hash of block {last}'s header")),
            "{range:?}: {msg}"
        );
    }
    // Ending before the fork, the range is served.
    let store = open(Some((2, 29))).unwrap();
    assert_eq!(
        store.last,
        BlockPos {
            num: 28,
            id: fx.chain.id(28)
        }
    );
}

#[test]
fn startup_serves_only_what_the_block_logs_cover() {
    let fx = Fixture::new(SyntheticChain::new(2, 199));
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..201, Encoding::SizedStored);
    fx.ship(&dir, LogKind::ChainState, 2..201, Encoding::SizedStored);
    fx.chain
        .write_blocks(&dir, 20..150, FileName::Head)
        .unwrap();
    let (store, _) = Store::open(&fx.store()).unwrap();
    assert_eq!(store.served.trace, (20, 150));
    assert_eq!(store.served.chain_state, (20, 150));
    assert_eq!(store.served.finality, (0, 0));
    assert_eq!(store.served.blocks, (20, 150));
    assert_eq!(store.last.num, 149);
    assert_eq!(store.last.id, fx.chain.id(149));
    // The block before the first served block is known through the block log.
    assert_eq!(store.id(19).unwrap(), Some(fx.chain.id(19)));
    assert_eq!(store.id(18).unwrap(), None);
    assert_eq!(store.id(150).unwrap(), None);

    // Without block logs everything is served.
    let mut cfg = fx.store();
    cfg.blocks = BlocksMode::None;
    cfg.chain_id = Some(fx.chain.chain_id());
    let (store, _) = Store::open(&cfg).unwrap();
    assert_eq!(store.served.trace, (2, 201));
    assert_eq!(store.served.blocks, (0, 0));
}

#[test]
fn startup_explains_what_is_missing() {
    let fx = Fixture::new(SyntheticChain::new(2, 50));
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..52, Encoding::SizedStored);
    let open = |cfg: &StoreConfig| format!("{:#}", Store::open(cfg).err().expect("refused"));

    assert!(open(&fx.store()).contains("--blocks none --chain-id"));
    fx.chain.write_blocks(&dir, 2..52, FileName::Head).unwrap();
    let mut cfg = fx.store();
    cfg.range = Some((100, 200));
    let msg = open(&cfg);
    assert!(
        msg.contains("no block log holds a block of --range"),
        "{msg}"
    );
    cfg.blocks = BlocksMode::None;
    cfg.chain_id = Some(fx.chain.chain_id());
    let msg = open(&cfg);
    assert!(msg.contains("no state-history log"), "{msg}");
    cfg.dirs.clear();
    assert!(open(&cfg).contains("--dir"));
}
