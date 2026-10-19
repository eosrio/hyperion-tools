//! The offline commands, through the library and through the binary: `check` and its exit
//! codes (T18) and deep modes, `index` and serving the indexes it writes (T19), and `abi`.

mod common;

use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::process::Command;

use common::*;
use hyperion_ship::protocol::Request;
use hyperion_ship::shiplog::LogKind;
use hyperion_ship::testkit::ship::{
    Encoding, LogBuilder, sized_payload, synthetic_id, synthetic_payload, zlib_stored,
};
use hyperion_ship::testkit::{FileName, SyntheticChain};
use serde_json::Value;
use ship_archive::check::{self, Beside, CheckConfig, IndexConfig, WARN_ENTRY_BYTES};
use ship_archive::store::{BlocksMode, Store, StoreConfig, VerifyIds};

const BIN: &str = env!("CARGO_BIN_EXE_antelope-ship-archive");

/// Run the binary with `args`, logging warnings only; its exit code and standard output.
fn run<I, S>(args: I) -> (i32, String)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new(BIN)
        .args(args)
        .env("RUST_LOG", "warn")
        .output()
        .expect("run the binary");
    let stdout = String::from_utf8(out.stdout).expect("UTF-8 output");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let code = out.status.code().expect("an exit code");
    (
        code,
        format!(
            "{stdout}{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!("\n[stderr]\n{stderr}")
            }
        ),
    )
}

/// `check --dir <dir> --recursive` and `extra`.
fn check_args(dir: &Path, extra: &[&str]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        "check".into(),
        "--dir".into(),
        dir.into(),
        "--recursive".into(),
    ];
    args.extend(extra.iter().map(OsString::from));
    args
}

/// `check` as the command line runs it by default.
fn config(store: StoreConfig) -> CheckConfig {
    CheckConfig {
        store,
        deep: false,
        hash: false,
        warn_entry_bytes: WARN_ENTRY_BYTES,
    }
}

// --- T18: check -----------------------------------------------------------------------------

#[test]
fn t18_check_exits_0_when_clean_1_on_errors_and_2_on_warnings_with_strict() {
    // Clean: the standard layout, whose empty head trace log is a note, not a warning.
    let fx = Fixture::standard();
    let report = check::check(&config(fx.store()));
    assert_eq!(report.error_count(), 0, "{}", report.text());
    assert_eq!(report.warning_count(), 0, "{}", report.text());
    assert_eq!(report.findings.len(), 1, "the empty head log");
    let coverage = report.coverage.as_ref().expect("the store opened");
    assert_eq!(coverage.chain_id, fx.chain.chain_id());
    assert_eq!(coverage.last.num, 200);
    let logs: Vec<_> = coverage
        .logs
        .iter()
        .map(|l| (l.log, l.held, l.served, l.files.len(), l.seams.clone()))
        .collect();
    assert_eq!(
        logs,
        [
            ("trace", Some((2, 201)), (2, 201), 4, vec![51, 101, 151]),
            ("chain_state", Some((2, 201)), (2, 201), 1, vec![]),
            ("finality", Some((100, 201)), (100, 201), 1, vec![]),
            ("blocks", Some((2, 201)), (2, 201), 4, vec![65, 129, 193]),
        ]
    );
    for strict in [false, true] {
        let extra: &[&str] = if strict { &["--strict"] } else { &[] };
        let (code, out) = run(check_args(fx.tmp.path(), extra));
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("result: clean"), "{out}");
        assert!(
            out.contains("serving 2..201 of 2..201, in 4 files"),
            "{out}"
        );
    }
    let (code, out) = run(check_args(fx.tmp.path(), &["--json"]));
    assert_eq!(code, 0, "{out}");
    let json: Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(json["ok"], true);
    assert_eq!(json["coverage"]["last"]["num"], 200);
    assert_eq!(json["coverage"]["logs"]["finality"]["served"]["begin"], 100);
    assert_eq!(json["coverage"]["logs"]["blocks"]["seams"][2], 193);
    assert_eq!(json["findings"][0]["kind"], "empty");
    assert_eq!(json["findings"][0]["warning"], false);

    // A hole between two trace files is an error: exit 1.
    let fx = Fixture::new(SyntheticChain::new(2, 199));
    let dir = fx.dir("node");
    for blocks in [2..51, 101..201] {
        fx.chain
            .write_ship(
                &dir,
                LogKind::Trace,
                blocks,
                Encoding::SizedStored,
                FileName::Retained,
            )
            .unwrap();
    }
    fx.chain.write_blocks(&dir, 2..201, FileName::Head).unwrap();
    let report = check::check(&config(fx.store()));
    assert!(report.coverage.is_none());
    assert_eq!(report.error_count(), 1);
    assert!(
        report.errors[0].contains("blocks 51..101 are in no file"),
        "{}",
        report.text()
    );
    for extra in [&[][..], &["--strict"][..]] {
        let (code, out) = run(check_args(fx.tmp.path(), extra));
        assert_eq!(code, 1, "{out}");
        assert!(
            out.contains("error: ") && out.contains("contiguous range"),
            "{out}"
        );
    }

    // Entries over --warn-entry-bytes are warnings: exit 0, or 2 with --strict. The Type A size
    // comes from its header, the Type B one from a counting pass.
    let mut chain = SyntheticChain::new(2, 29);
    chain
        .set_payload_len(LogKind::Trace, 10, 5000)
        .set_payload_len(LogKind::ChainState, 12, 6000);
    let fx = Fixture::new(chain);
    let dir = fx.dir("node");
    fx.ship(&dir, LogKind::Trace, 2..31, Encoding::SizedStored);
    fx.ship(&dir, LogKind::ChainState, 2..31, Encoding::LegacyLength);
    fx.chain.write_blocks(&dir, 2..31, FileName::Head).unwrap();
    let mut cfg = config(fx.store());
    cfg.warn_entry_bytes = 4096;
    let report = check::check(&cfg);
    let large: Vec<String> = report.findings.iter().map(ToString::to_string).collect();
    assert_eq!(
        large,
        [
            "the trace entry of block 10 inflates to 5000 bytes",
            "the chain_state entry of block 12 inflates to 6000 bytes",
        ]
    );
    assert_eq!((report.exit_code(false), report.exit_code(true)), (0, 2));
    let (code, out) = run(check_args(fx.tmp.path(), &["--warn-entry-bytes", "4096"]));
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("warning: the trace entry of block 10"),
        "{out}"
    );
    assert!(out.contains("result: 2 warnings"), "{out}");
    let (code, out) = run(check_args(
        fx.tmp.path(),
        &["--warn-entry-bytes", "4096", "--strict", "--json"],
    ));
    assert_eq!(code, 2, "{out}");
    let json: Value = serde_json::from_str(&out).expect("JSON");
    assert_eq!(
        (json["ok"].clone(), json["findings"][1]["kind"].clone()),
        (Value::Bool(true), "large_entry".into())
    );
    // Under the default limit, nothing is large.
    let (code, out) = run(check_args(fx.tmp.path(), &["--strict"]));
    assert_eq!(code, 0, "{out}");
}

#[test]
fn t18_an_entry_of_4_gib_or_more_is_an_error_unless_the_range_excludes_it() {
    let fx = Fixture::new(SyntheticChain::new(2, 29));
    let dir = fx.dir("node");
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for n in 2..31 {
        let raw = fx.chain.payload(LogKind::Trace, n);
        if n == 20 {
            // A header declaring 5 GiB: no varuint32 reader can take the length.
            trace.push_encoded(fx.chain.id(n), &sized_payload(5 << 30, &zlib_stored(&raw)));
        } else {
            trace.push(fx.chain.id(n), &raw);
        }
    }
    trace.write(&dir, "trace_history").unwrap();
    fx.chain.write_blocks(&dir, 2..31, FileName::Head).unwrap();

    let report = check::check(&config(fx.store()));
    assert_eq!(report.error_count(), 1, "{}", report.text());
    assert!(
        report.errors[0].contains("block 20 inflates to 5368709120 bytes: from 4 GiB"),
        "{}",
        report.text()
    );
    let (code, out) = run(check_args(fx.tmp.path(), &[]));
    assert_eq!(code, 1, "{out}");
    // --range leaves it out.
    let (code, out) = run(check_args(fx.tmp.path(), &["--range", "2-20", "--strict"]));
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("serving 2..20 of 2..31"), "{out}");
}

#[test]
fn deep_checks_read_every_entry_and_compare_every_id() {
    let fx = Fixture::new(SyntheticChain::new(2, 59));
    let dir = fx.dir("node");
    let mut trace = fx
        .chain
        .ship_log(LogKind::Trace, 2..61, Encoding::SizedStored);
    trace.corrupt_checksum(30);
    trace.write(&dir, "trace_history").unwrap();
    // A chain_state entry naming another fork's block 40, away from every seam.
    let mut deltas = LogBuilder::new(Encoding::LegacyLength);
    for n in 2..61 {
        let id = if n == 40 {
            synthetic_id(40, 7)
        } else {
            fx.chain.id(n)
        };
        deltas.push(id, &fx.chain.payload(LogKind::ChainState, n));
    }
    deltas.write(&dir, "chain_state_history").unwrap();
    fx.chain.write_blocks(&dir, 2..61, FileName::Head).unwrap();

    // Headers alone see nothing wrong.
    let report = check::check(&config(fx.store()));
    assert_eq!(report.error_count(), 0, "{}", report.text());
    assert!(report.deep.is_none());
    let (code, out) = run(check_args(fx.tmp.path(), &[]));
    assert_eq!(code, 0, "{out}");

    let mut cfg = config(fx.store());
    cfg.deep = true;
    let report = check::check(&cfg);
    assert_eq!(report.error_count(), 2, "{}", report.text());
    assert!(
        report.errors[0].starts_with("reading the trace log: ")
            && report.errors[0].contains("block 30"),
        "{}",
        report.text()
    );
    assert!(
        report.errors[1].starts_with("block ids disagree at block 40: the trace entry has "),
        "{}",
        report.text()
    );
    let deep = report.deep.expect("deep statistics");
    // Every entry but the damaged one inflated, every block of the block log read.
    assert_eq!((deep.entries, deep.blocks, deep.hashed), (59 + 58, 59, 0));
    let inflated: u64 = (2..61)
        .flat_map(|n| [LogKind::Trace, LogKind::ChainState].map(|k| (k, n)))
        .filter(|&(k, n)| !(k == LogKind::Trace && n == 30))
        .map(|(k, n)| fx.chain.payload_len(k, n) as u64)
        .sum();
    assert_eq!(deep.inflated_bytes, inflated);

    // --hash also recomputes the ids from the block headers, which side with the trace log.
    let (code, out) = run(check_args(fx.tmp.path(), &["--hash"]));
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("59 blocks, 59 block ids recomputed"), "{out}");
    assert!(
        out.contains("block ids disagree at block 40: the trace entry has")
            && out.contains("the chain_state entry has"),
        "{out}"
    );
    assert!(!out.contains("the hash of its block header has"), "{out}");
}

#[test]
fn a_deep_check_compares_the_last_block_with_its_header_hash() {
    // Only the last block, 30, is from another fork; the block log ends with the trace log, so
    // no next block names 30's id.
    let fx = Fixture::new(SyntheticChain::new(2, 29));
    let dir = fx.dir("node");
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for n in 2..31 {
        let id = if n == 30 {
            synthetic_id(30, 3)
        } else {
            fx.chain.id(n)
        };
        trace.push(id, &fx.chain.payload(LogKind::Trace, n));
    }
    trace.write(&dir, "trace_history").unwrap();
    fx.chain.write_blocks(&dir, 2..31, FileName::Head).unwrap();

    // The startup check refuses it.
    let report = check::check(&config(fx.store()));
    assert_eq!(report.error_count(), 1, "{}", report.text());
    assert!(
        report.errors[0].contains("block ids disagree at block 30"),
        "{}",
        report.text()
    );
    // Without it, a deep check still finds it, with --hash or not.
    for hash in [false, true] {
        let mut cfg = config(fx.store());
        cfg.store.verify_ids = VerifyIds::Off;
        cfg.deep = true;
        cfg.hash = hash;
        let report = check::check(&cfg);
        assert_eq!(report.error_count(), 1, "{}", report.text());
        assert!(
            report.errors[0].starts_with("block ids disagree at block 30: the trace entry has ")
                && report.errors[0].contains("the hash of its block header has"),
            "{}",
            report.text()
        );
        assert_eq!(report.deep.unwrap().hashed, if hash { 29 } else { 0 });
    }
}

#[test]
fn a_clean_deep_check_agrees_with_every_block_header() {
    let fx = Fixture::standard();
    let mut cfg = config(fx.store());
    cfg.deep = true;
    cfg.hash = true;
    let report = check::check(&cfg);
    assert_eq!(report.error_count(), 0, "{}", report.text());
    let deep = report.deep.unwrap();
    assert_eq!((deep.blocks, deep.hashed), (199, 199));
    assert_eq!(deep.entries, 199 + 199 + 101);

    // --hash without block logs has nothing to hash.
    let mut cfg = config(fx.store());
    cfg.store.blocks = BlocksMode::None;
    cfg.store.chain_id = Some(fx.chain.chain_id());
    cfg.deep = true;
    cfg.hash = true;
    let report = check::check(&cfg);
    assert_eq!(report.error_count(), 0, "{}", report.text());
    assert_eq!(report.deep.unwrap().hashed, 0);
    assert!(
        report.warnings[0].contains("--hash with --blocks none"),
        "{}",
        report.text()
    );
    assert_eq!(report.exit_code(true), 2);
}

// --- T19: index -----------------------------------------------------------------------------

/// Blocks 2..41 whose trace log holds a fork that was abandoned: 2..=14, then 15'..=20' (with
/// payloads of their own), then 15..=40 after the switch. The index beside it is right; returns
/// the log, and the offsets of the orphans 15'..=20'.
fn orphaned_fixture() -> (Fixture, LogBuilder, Vec<u64>) {
    let fx = Fixture::new(SyntheticChain::new(2, 39));
    let dir = fx.dir("node");
    let mut trace = LogBuilder::new(Encoding::SizedStored);
    for n in 2..15 {
        trace.push(fx.chain.id(n), &fx.chain.payload(LogKind::Trace, n));
    }
    for n in 15..21 {
        trace.push(synthetic_id(n, 5), &synthetic_payload(n, 77));
    }
    let orphans: Vec<u64> = (15..21).map(|n| trace.position(n)).collect();
    for n in 15..41 {
        trace.push(fx.chain.id(n), &fx.chain.payload(LogKind::Trace, n));
    }
    trace.write(&dir, "trace_history").unwrap();
    fx.ship(&dir, LogKind::ChainState, 2..41, Encoding::SizedDeflate);
    fx.chain.write_blocks(&dir, 2..41, FileName::Head).unwrap();
    (fx, trace, orphans)
}

/// Every result of `fetch_all` blocks 2..41 from a server over `store`, against the chain.
async fn serves_the_chain(fx: &Fixture, store: StoreConfig) {
    let model = Model {
        chain: &fx.chain,
        trace: 2..41,
        chain_state: 2..41,
        finality: 0..0,
        blocks: 2..41,
    };
    let server = start(serve_config(store)).await;
    let mut ws = connect(&server).await;
    let req = fetch_all(blocks(2, 41, 100));
    send(&mut ws, &Request::Blocks(req.clone())).await;
    for n in 2..41 {
        assert_eq!(
            recv_binary(&mut ws).await,
            model.result(n, &req),
            "block {n}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t19_index_rebuilds_an_orphaned_index_into_a_sidecar_that_serve_reads() {
    let (fx, trace, orphans) = orphaned_fixture();
    let (node, side) = (fx.dir("node"), fx.dir("side"));
    let beside = node.join("trace_history.index");
    let sidecar = side.join("trace_history.index");
    let mut with_sidecar = fx.store();
    with_sidecar.dirs = vec![node.clone()];
    with_sidecar.index_dirs = vec![side.clone()];

    // A missing index: the store refuses to open, and `index` writes the one Spring would.
    std::fs::remove_file(&beside).unwrap();
    let err = format!("{:#}", Store::open(&fx.store()).err().expect("no index"));
    assert!(err.contains("run `antelope-ship-archive index`"), "{err}");
    let (code, out) = run([
        OsStr::new("index"),
        "--dir".as_ref(),
        node.as_os_str(),
        "--out".as_ref(),
        side.as_os_str(),
    ]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("(blocks 2..41) has no index beside it: wrote"),
        "{out}"
    );
    assert!(out.contains("1 of 2 indexes written to"), "{out}");
    assert_eq!(std::fs::read(&sidecar).unwrap(), trace.index());
    assert!(!beside.exists(), "nothing is written beside the logs");
    serves_the_chain(&fx, with_sidecar.clone()).await;

    // An index pointing at the orphans, as a forward scan builds it, opens (its length and last
    // slot are right) but serves another fork: a deep check finds it, and `index` replaces it.
    let mut forward = trace.clone();
    forward.slots_mut()[13..19].copy_from_slice(&orphans);
    std::fs::write(&beside, forward.index()).unwrap();
    std::fs::remove_dir_all(&side).unwrap();
    let (code, out) = run(check_args(&node, &["--deep"]));
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("block ids disagree at block 15"), "{out}");
    let done = check::index(&IndexConfig {
        dirs: vec![node.clone()],
        recursive: false,
        out: side.clone(),
    })
    .unwrap();
    let trace_done = done
        .iter()
        .find(|i| i.log.ends_with("trace_history.log"))
        .unwrap();
    assert_eq!(
        (
            trace_done.blocks,
            trace_done.beside,
            trace_done.written.as_deref()
        ),
        (Some((2, 41)), Beside::Wrong, Some(sidecar.as_path()))
    );
    let deltas_done = done
        .iter()
        .find(|i| i.log.ends_with("chain_state_history.log"))
        .unwrap();
    assert_eq!(
        (deltas_done.beside, deltas_done.written.clone()),
        (Beside::Matches, None)
    );
    assert_eq!(
        std::fs::read(&beside).unwrap(),
        forward.index(),
        "left as it was"
    );
    assert_eq!(std::fs::read(&sidecar).unwrap(), trace.index());
    let (code, out) = run(check_args(
        &node,
        &["--deep", "--index-dir", side.to_str().unwrap()],
    ));
    assert_eq!(code, 0, "{out}");
    serves_the_chain(&fx, with_sidecar).await;

    // The indexes never go beside the logs.
    let err = check::index(&IndexConfig {
        dirs: vec![node.clone()],
        recursive: false,
        out: node.clone(),
    })
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("never written beside its log"),
        "{err:#}"
    );
    assert_eq!(std::fs::read(&beside).unwrap(), forward.index());
}

// --- abi ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abi_prints_the_abi_text_a_node_sends() {
    let fx = Fixture::standard();
    let path = fx.dir("served.abi.json");
    let text = r#"{"version":"eosio::abi/1.1","structs":[{"name":"x","base":"","fields":[]}]}"#;
    std::fs::write(&path, text).unwrap();
    let mut cfg = fx.serve();
    cfg.store.abi = Some(path);
    let server = start(cfg).await;

    assert_eq!(ship_archive::node_abi(&server.url()).await.unwrap(), text);
    let (url, out) = (server.url(), fx.dir("fetched.abi.json"));
    let fetched = out.clone();
    let (code, stdout) = tokio::task::spawn_blocking(move || {
        run([
            OsStr::new("abi"),
            "--from".as_ref(),
            url.as_ref(),
            "--out".as_ref(),
            fetched.as_os_str(),
        ])
    })
    .await
    .unwrap();
    assert_eq!(code, 0, "{stdout}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
    let url = server.url();
    let (code, stdout) = tokio::task::spawn_blocking(move || run(["abi", "--from", url.as_str()]))
        .await
        .unwrap();
    assert_eq!((code, stdout.as_str()), (0, text));

    // Nothing listening.
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let err = ship_archive::node_abi(&format!("ws://{closed}/"))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("connect to"), "{err:#}");
}
