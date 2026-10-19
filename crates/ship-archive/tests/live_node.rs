//! Read the original files of a running node. No fixture copies, links or input writes.
//! SHIP_LIVE_NODE=ws://localhost:8080 SHIP_LIVE_DIRS=/node/data cargo test --release
//! -p antelope-ship-archive --test live_node existing_node -- --ignored --nocapture

mod common;

use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use common::*;
use hyperion_ship::protocol::{BlockPos, Request};
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::ship::Encoding;
use hyperion_ship::testkit::{FileName, SyntheticChain};
use ship_archive::Server;
use ship_archive::fetch::{FetchLimits, Flags, Job};
use ship_archive::live_test;
use ship_archive::store::{Store, StoreConfig};
use tokio_tungstenite::{connect_async, tungstenite::Message};

fn fixture() -> Fixture {
    let fx = Fixture::new(SyntheticChain::new(10, 50));
    for kind in [LogKind::Trace, LogKind::ChainState] {
        fx.chain
            .write_ship(
                fx.tmp.path(),
                kind,
                10..55,
                Encoding::SizedDeflate,
                FileName::Head,
            )
            .unwrap();
    }
    fx.chain
        .write_blocks(fx.tmp.path(), 10..50, FileName::Head)
        .unwrap();
    fx
}

fn config(fx: &Fixture) -> StoreConfig {
    StoreConfig {
        dirs: vec![fx.tmp.path().into()],
        range: Some((12, 40)),
        ..StoreConfig::default()
    }
}

fn lib(fx: &Fixture) -> BlockPos {
    BlockPos {
        num: 45,
        id: fx.chain.id(45),
    }
}

fn build(store: &Store, n: u32) -> anyhow::Result<ship_archive::fetch::Package> {
    store.build(
        &Job {
            generation: 0,
            n,
            v1: false,
            flags: Flags {
                block: true,
                traces: true,
                deltas: true,
                finality: false,
            },
        },
        &FetchLimits {
            stream_threshold: 1,
        },
    )
}

fn append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn active_torn_tails_are_excluded_and_future_appends_do_not_change_the_view() {
    let fx = fixture();
    for stem in ["trace_history", "chain_state_history", "blocks"] {
        append(&fx.tmp.path().join(format!("{stem}.log")), &[0xaa; 17]);
        append(&fx.tmp.path().join(format!("{stem}.index")), &[0xff; 3]);
    }
    assert!(
        Store::open(&config(&fx)).is_err(),
        "ordinary archive still refuses active torn inputs"
    );
    let (store, _) = live_test::open(&config(&fx), lib(&fx)).unwrap();
    assert_eq!(store.last.num, 39);
    for stem in ["trace_history", "chain_state_history", "blocks"] {
        append(&fx.tmp.path().join(format!("{stem}.log")), &[0xbb; 33]);
        append(&fx.tmp.path().join(format!("{stem}.index")), &[0; 80]);
    }
    for n in 12..40 {
        build(&store, n).unwrap();
    }
    assert_eq!(store.last.num, 39);
}

#[test]
fn rotation_keeps_the_original_descriptors_and_never_reopens_replacements() {
    let fx = fixture();
    let (store, _) = live_test::open(&config(&fx), lib(&fx)).unwrap();
    for stem in ["trace_history", "chain_state_history", "blocks"] {
        for ext in ["log", "index"] {
            let original = fx.tmp.path().join(format!("{stem}.{ext}"));
            fs::rename(
                &original,
                fx.tmp.path().join(format!("{stem}-retained.{ext}")),
            )
            .unwrap();
            fs::write(original, [0; 100]).unwrap();
        }
    }
    for n in 12..40 {
        build(&store, n).unwrap();
    }
}

#[test]
fn truncation_and_corruption_inside_the_selected_range_fail() {
    for truncate in [false, true] {
        let fx = fixture();
        let (store, _) = live_test::open(&config(&fx), lib(&fx)).unwrap();
        let file = fx.tmp.path().join("trace_history.log");
        let mut f = OpenOptions::new().write(true).open(file).unwrap();
        if truncate {
            f.set_len(0).unwrap();
        } else {
            let log = store.ship_log(LogKind::Trace, 20).unwrap();
            let h = log.header(20).unwrap();
            f.seek(SeekFrom::Start(h.data_offset())).unwrap();
            f.write_all(&[0xff; 4]).unwrap();
        }
        assert!(build(&store, 20).is_err());
    }
}

#[test]
fn irreversible_limit_and_incomplete_coverage_are_refused() {
    let fx = fixture();
    let mut cfg = config(&fx);
    cfg.range = Some((12, 46));
    assert!(live_test::open(&cfg, lib(&fx)).is_err());
    cfg.range = Some((12, 50));
    assert!(
        live_test::open(
            &cfg,
            BlockPos {
                num: 55,
                id: fx.chain.id(55)
            }
        )
        .is_err()
    );
    cfg.range = Some((12, 40));
    cfg.max_open_files = 2;
    assert!(live_test::open(&cfg, lib(&fx)).is_err());
}

#[test]
fn newly_rotated_heads_need_no_index_outside_the_selected_range() {
    let fx = fixture();
    for stem in ["trace_history", "chain_state_history", "blocks"] {
        for ext in ["log", "index"] {
            let path = fx.tmp.path().join(format!("{stem}.{ext}"));
            let last = if stem == "blocks" { 49 } else { 54 };
            fs::rename(&path, fx.tmp.path().join(format!("{stem}-10-{last}.{ext}"))).unwrap();
        }
    }
    for stem in ["trace_history", "chain_state_history"] {
        fs::write(fx.tmp.path().join(format!("{stem}.log")), []).unwrap();
    }
    let pair = fx
        .chain
        .write_blocks(fx.tmp.path(), 50..50, FileName::Head)
        .unwrap();
    fs::remove_file(pair.index).unwrap();
    let (store, _) = live_test::open(&config(&fx), lib(&fx)).unwrap();
    for n in 12..40 {
        build(&store, n).unwrap();
    }
}

#[test]
fn the_shared_status_decoder_handles_v0_v1_and_legacy_replies() {
    use hyperion_ship::protocol::{Status, encode_status};
    let fx = fixture();
    let expected = Status {
        head: lib(&fx),
        lib: lib(&fx),
        trace: (10, 50),
        chain_state: (11, 49),
        finality: (20, 45),
        chain_id: fx.chain.chain_id(),
    };
    for v1 in [false, true] {
        let encoded = encode_status(v1, &expected);
        let decoded = parse_status(&encoded);
        assert_eq!(decoded.chain_id, Some(expected.chain_id));
        assert_eq!(decoded.trace, expected.trace);
        assert_eq!(decoded.chain_state, expected.chain_state);
    }
    let mut legacy = encode_status(false, &expected);
    legacy.truncate(89);
    assert!(parse_status(&legacy).chain_id.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs SHIP_LIVE_NODE and SHIP_LIVE_DIRS on the node's host"]
async fn existing_node() {
    let endpoint = std::env::var("SHIP_LIVE_NODE").expect("set SHIP_LIVE_NODE=ws://host:port");
    let dirs = std::env::var_os("SHIP_LIVE_DIRS")
        .expect("set SHIP_LIVE_DIRS to the existing node data directory (or a path list)");
    let count: u32 = std::env::var("SHIP_LIVE_BLOCKS").map_or(200, |n| {
        n.parse().expect("SHIP_LIVE_BLOCKS is a positive integer")
    });
    assert!(count > 0);
    let (mut node, _) = connect_async(&endpoint).await.expect("connect to node");
    let Message::Text(node_abi) = recv(&mut node).await else {
        panic!("node ABI missing");
    };
    let before = parse_status(&status(&mut node, false).await);
    assert!(
        before.trace.0 < before.trace.1 && before.chain_state.0 < before.chain_state.1,
        "enable trace-history and chain-state-history on the node"
    );
    let end = before.lib.num.min(before.trace.1).min(before.chain_state.1);
    let begin = end
        .saturating_sub(count)
        .max(before.trace.0)
        .max(before.chain_state.0)
        .max(2);
    let cfg = StoreConfig {
        dirs: std::env::split_paths(&dirs).collect(),
        recursive: true,
        range: Some((begin, end)),
        chain_id: before.chain_id,
        ..StoreConfig::default()
    };
    let mut captured = live_test::open(&cfg, before.lib);
    for attempt in 0..4 {
        if captured.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)).await;
        captured = live_test::open(&cfg, before.lib);
    }
    let (store, findings) = captured.expect("pin irreversible inputs after five attempts");
    // Read and validate every selected entry and block, with checksums and block hashes.
    for n in begin..end {
        build(&store, n).unwrap();
    }
    let fixed = store.status_bytes(false);
    let mut serving = serve_config(cfg);
    serving.stream_threshold = 1; // every nonempty payload also exercises streaming
    let server = Server::bind_store(serving, store, findings).await.unwrap();
    let address = server.local_addr();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(server.run(async {
        let _ = stopped.await;
    }));
    let (mut archive, _) = connect_async(format!("ws://{address}")).await.unwrap();
    let Message::Text(archive_abi) = recv(&mut archive).await else {
        panic!("archive ABI missing");
    };
    eprintln!("live: ABI byte equality: {}", node_abi == archive_abi);
    let v1 = std::env::var_os("SHIP_LIVE_V1").is_some();
    let mut compared = 0;
    for version in if v1 { vec![false, true] } else { vec![false] } {
        for flag_matrix in [false, true] {
            // Full payloads for the whole sample, then all flag combinations at its edge.
            let flags: Vec<u8> = if flag_matrix {
                (0..16).collect()
            } else {
                vec![15]
            };
            for mask in flags {
                let mut req = blocks(
                    begin,
                    if flag_matrix {
                        (begin + 2).min(end)
                    } else {
                        end
                    },
                    16,
                );
                req.v1 = version;
                req.fetch_block = mask & 1 != 0;
                req.fetch_traces = mask & 2 != 0;
                req.fetch_deltas = mask & 4 != 0;
                req.fetch_finality_data = version && mask & 8 != 0;
                send(&mut node, &Request::Blocks(req.clone())).await;
                send(&mut archive, &Request::Blocks(req.clone())).await;
                for n in req.start..req.end {
                    let (theirs, ours) =
                        tokio::join!(recv_binary(&mut node), recv_binary(&mut archive));
                    assert!(ours.len() >= 73 && theirs.len() >= 73);
                    assert_eq!(ours[0], theirs[0], "variant at block {n}");
                    assert!(
                        ours[73..] == theirs[73..],
                        "payload at block {n}, v1 {version}, flags {mask}: {} vs {} bytes",
                        ours.len(),
                        theirs.len()
                    );
                    compared += 1;
                    send(&mut node, &Request::Ack(1)).await;
                    send(&mut archive, &Request::Ack(1)).await;
                }
            }
        }
    }
    quiet(&mut archive).await;
    assert_eq!(status(&mut archive, false).await, fixed);
    let after = parse_status(&status(&mut node, false).await);
    assert_eq!(before.chain_id, after.chain_id);
    assert!(after.lib.num >= before.lib.num);
    eprintln!(
        "live: PASS {compared} byte-equal results, range {begin}..{end}; node head {} -> {}, LIB {} -> {}; original files read in place",
        before.head.num, after.head.num, before.lib.num, after.lib.num
    );
    drop((node, archive));
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
