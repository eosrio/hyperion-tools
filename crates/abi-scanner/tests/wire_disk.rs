//! Public CLI controls using frozen log framing and the captured Wire schema/ABI.
//! The small prefix is constructed, not asserted to be a node-consensus capture.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use hyperion_ship::provider::ProtocolProvider;
use hyperion_ship::testkit::ship::{Encoding, LogBuilder, synthetic_id};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

struct Fixture {
    directory: PathBuf,
    provider: ProtocolProvider,
    schema: Value,
    template: Value,
    abi: String,
    state: LogBuilder,
    trace: LogBuilder,
    chain_id: String,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let directory = std::env::temp_dir().join(format!(
            "wire-disk-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let capture: Value =
            serde_json::from_str(include_str!("../../core/testdata/wire/ship-capture.json"))
                .unwrap();
        let schema: Value =
            serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
        fs::write(
            directory.join("ship.json"),
            capture["handshakeUtf8"].as_str().unwrap(),
        )
        .unwrap();
        let provider: ProtocolProvider = "wire-sysio-6508c9e8-engineering-v1".parse().unwrap();
        let raw = capture["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "captured-ship-block-result")
            .unwrap()["hex"]
            .as_str()
            .unwrap();
        let bundle = provider
            .decode_bundle(&schema, &hex::decode(raw).unwrap())
            .unwrap();
        let abi = bundle["deltaRows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["stream"] == "account")
            .unwrap()["value"][1]["abi"]
            .as_str()
            .unwrap()
            .to_owned();
        let template = bundle["traces"][0].clone();
        let mut state = LogBuilder::new(Encoding::SizedDeflate);
        state.push(
            synthetic_id(1, 0),
            &provider
                .encode_value(&schema, "table_delta[]", &json!([]))
                .unwrap(),
        );
        Self {
            directory,
            provider,
            schema,
            template,
            abi,
            state,
            trace: LogBuilder::new(Encoding::SizedDeflate),
            chain_id: "22".repeat(32),
        }
    }
    fn action(
        &self,
        ordinal: u32,
        gs: u64,
        account: &str,
        name: &str,
        data: &str,
        abi_sequence: u32,
    ) -> Value {
        let mut action = self.template[1]["action_traces"][0].clone();
        action[1]["action_ordinal"] = json!(ordinal);
        action[1]["creator_action_ordinal"] = json!(0);
        action[1]["receiver"] = json!(account);
        action[1]["act"] = json!({"account":account,"name":name,"authorization":[],"data":data});
        action[1]["receipt"][1]["receiver"] = json!(account);
        action[1]["receipt"][1]["global_sequence"] = json!(gs.to_string());
        action[1]["receipt"][1]["abi_sequence"] = json!(abi_sequence);
        action[1]["except"] = Value::Null;
        action[1]["error_code"] = Value::Null;
        action
    }
    fn setabi(&self, ordinal: u32, gs: u64, abi: &str) -> Value {
        let mut bytes = rs_abieos::Abieos::new()
            .string_to_name("target")
            .unwrap()
            .to_le_bytes()
            .to_vec();
        let raw = hex::decode(abi).unwrap();
        let mut n = raw.len();
        loop {
            let b = (n & 127) as u8;
            n >>= 7;
            bytes.push(if n == 0 { b } else { b | 128 });
            if n == 0 {
                break;
            }
        }
        bytes.extend_from_slice(&raw);
        self.action(ordinal, gs, "sysio", "setabi", &hex::encode(bytes), 0)
    }
    fn append(&mut self, block: u32, abi: &str, actions: Vec<Value>, fork: u8) {
        self.append_row(block, 1, abi, actions, fork);
    }
    /// A block whose `target` account row has `present` and `abi`, and one transaction.
    fn append_row(&mut self, block: u32, present: u8, abi: &str, actions: Vec<Value>, fork: u8) {
        let account=self.provider.encode_value(&self.schema,"account",&json!(["account_v0",{"name":"target","creation_date":"2025-01-01T00:00:00.000","abi":abi}])).unwrap();
        // Account is deliberately not the first delta table.
        let deltas = json!([["table_delta_v0",{"name":"resource_limits","rows":[]}],["table_delta_v0",{"name":"account","rows":[{"present":present,"data":hex::encode(account)}]}]]);
        self.state.push(
            synthetic_id(block, fork),
            &self
                .provider
                .encode_value(&self.schema, "table_delta[]", &deltas)
                .unwrap(),
        );
        let mut tx = self.template.clone();
        tx[1]["status"] = json!(0);
        tx[1]["id"] = json!("11".repeat(32));
        tx[1]["action_traces"] = json!(actions);
        tx[1]["except"] = Value::Null;
        tx[1]["error_code"] = Value::Null;
        self.trace.push(
            synthetic_id(block, fork),
            &self
                .provider
                .encode_value(&self.schema, "transaction_trace[]", &json!([tx]))
                .unwrap(),
        );
    }
    /// A block with no deltas and no transactions.
    fn empty(&mut self, block: u32) {
        self.state.push(synthetic_id(block, 0), &[0]);
        self.trace.push(synthetic_id(block, 0), &[0]);
    }
    fn run(&self, end: u32, fork: u8) -> Output {
        self.scan(1, end, fork, &self.directory.join("abis.ndjson"), &[])
    }
    fn scan(&self, start: u32, end: u32, fork: u8, out: &Path, extra: &[&str]) -> Output {
        self.state
            .write(&self.directory, "chain_state_history")
            .unwrap();
        self.trace.write(&self.directory, "trace_history").unwrap();
        Command::new(env!("CARGO_BIN_EXE_abi-scanner"))
            .args([
                "--protocol",
                self.provider.id(),
                "--from-disk",
                self.directory.to_str().unwrap(),
                "--start",
                &start.to_string(),
                "--end",
                &end.to_string(),
                "--ship-abi",
                self.directory.join("ship.json").to_str().unwrap(),
                "--chain-id",
                &self.chain_id,
                "--irreversible-block-id",
                &hex::encode(synthetic_id(end, fork)),
                "--out",
                out.to_str().unwrap(),
            ])
            .args(extra)
            .output()
            .unwrap()
    }
    /// A fresh subdirectory for one run's outputs.
    fn output(&self, name: &str) -> PathBuf {
        let directory = self.directory.join(name);
        fs::create_dir(&directory).unwrap();
        directory.join("abis.ndjson")
    }
    fn manifest(&self) -> Value {
        serde_json::from_slice(&fs::read(self.directory.join("abis.ndjson.manifest.json")).unwrap())
            .unwrap()
    }
    fn rows(&self) -> Vec<Value> {
        fs::read_to_string(self.directory.join("abis.ndjson"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn wire_disk_cli_seals_sparse_deployments_and_exact_source_membership() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(
        2,
        &abi,
        vec![
            f.setabi(1, 1, &abi),
            f.action(2, 2, "target", "transfer", "", 1),
        ],
        0,
    );
    success(f.run(2, 0));
    let manifest = f.manifest();
    let rows = f.rows();
    assert_eq!(manifest["format"], "hyperion-wire-abi-history-v1");
    assert_eq!(manifest["complete"], true);
    assert_eq!(manifest["startBlock"], 1);
    assert_eq!(manifest["endBlock"], 2);
    assert_eq!(manifest["lastGlobalSequence"], "2");
    assert_eq!(manifest["nativeBaseline"]["genesisTraceAbsent"], true);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["phase"], "deployment");
    assert_eq!(rows[0]["account"], "target");
    assert_eq!(rows[0]["abiSequence"], "1");
    assert_eq!(rows[0]["action"]["globalSequence"], "1");
    assert_eq!(rows[0]["abiHex"], abi);
    assert!(rows[0].get("abiDecodeError").is_none());
    assert_eq!(rows[1]["phase"], "block-final");
    assert_eq!(
        fs::read(f.directory.join("abis.ndjson.blocks.bin")).unwrap(),
        [synthetic_id(1, 0).as_slice(), synthetic_id(2, 0).as_slice()].concat()
    );
    for (entry, path) in [
        ("deployments", "abis.ndjson"),
        ("blockIds", "abis.ndjson.blocks.bin"),
    ] {
        let bytes = fs::read(f.directory.join(path)).unwrap();
        assert_eq!(manifest[entry]["bytes"], bytes.len() as u64);
        assert_eq!(
            manifest[entry]["sha256"],
            hex::encode(Sha256::digest(&bytes))
        );
    }
}

#[test]
fn same_block_intermediate_revisions_same_value_and_clear_are_retained() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(
        2,
        "",
        vec![
            f.setabi(1, 1, &abi),
            f.action(2, 2, "target", "transfer", "", 1),
            f.setabi(3, 3, &abi),
            f.action(4, 4, "target", "transfer", "", 2),
            f.setabi(5, 5, ""),
        ],
        0,
    );
    success(f.run(2, 0));
    let rows = f.rows();
    let deployments = rows
        .iter()
        .filter(|r| r["phase"] == "deployment")
        .collect::<Vec<_>>();
    assert_eq!(deployments.len(), 3);
    assert_eq!(
        deployments
            .iter()
            .map(|r| r["abiSequence"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["1", "2", "3"]
    );
    assert_eq!(deployments[1]["abiHex"], abi);
    assert_eq!(deployments[2]["abiHex"], "");
    assert_eq!(deployments[2]["present"], true);
}

#[test]
fn canonical_index_avoids_physical_fork_orphans() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.state.push(synthetic_id(2, 0), b"invalid abandoned state");
    f.trace.push(synthetic_id(2, 0), b"invalid abandoned trace");
    f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 1);
    success(f.run(2, 1));
    assert_eq!(f.manifest()["lastBlockId"], hex::encode(synthetic_id(2, 1)));
}

#[test]
fn corrupt_source_never_publishes_a_complete_manifest() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 0);
    f.trace.corrupt_checksum(2);
    let output = f.run(2, 0);
    assert!(!output.status.success());
    assert!(!f.directory.join("abis.ndjson.manifest.json").exists());
}

#[test]
fn receipt_revision_mismatch_fails_without_sealing() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(
        2,
        &abi,
        vec![
            f.setabi(1, 1, &abi),
            f.action(2, 2, "target", "transfer", "", 2),
        ],
        0,
    );
    let output = f.run(2, 0);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ABI receipt revision"));
    assert!(!f.directory.join("abis.ndjson.manifest.json").exists());
}

#[test]
fn malformed_contract_abi_retains_bytes_and_isolates_decode_failure() {
    let mut f = Fixture::new();
    f.append(2, "00", vec![f.setabi(1, 1, "00")], 0);
    success(f.run(2, 0));
    let rows = f.rows();
    assert_eq!(rows[0]["abiHex"], "00");
    assert!(rows[0]["abiDecodeError"].is_string());
    assert_eq!(f.manifest()["complete"], true);
}

#[test]
fn absent_trace_prefix_and_cross_stream_identity_mismatch_never_seal() {
    for missing in [true, false] {
        let mut f = Fixture::new();
        let abi = f.abi.clone();
        f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 0);
        if missing {
            f.trace = LogBuilder::new(Encoding::SizedDeflate);
        } else {
            f.trace.slots_mut().clear();
            let traces = f
                .provider
                .encode_value(&f.schema, "transaction_trace[]", &json!([]))
                .unwrap();
            f.trace.push(synthetic_id(2, 1), &traces);
        }
        let output = f.run(2, 0);
        assert!(!output.status.success());
        assert!(!f.directory.join("abis.ndjson.manifest.json").exists());
    }
}

#[test]
fn unexplained_abi_change_never_seals_and_existing_artifacts_are_preserved() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(2, &abi, vec![f.action(1, 1, "sysio", "onblock", "", 0)], 0);
    let output = f.run(2, 0);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("without a successful setabi"));
    assert!(!f.directory.join("abis.ndjson.manifest.json").exists());

    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 0);
    fs::write(f.directory.join("abis.ndjson"), "preserve\n").unwrap();
    let output = f.run(2, 0);
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(f.directory.join("abis.ndjson")).unwrap(),
        "preserve\n"
    );
    assert!(!f.directory.join("abis.ndjson.manifest.json").exists());
}

#[test]
fn retained_catalog_hole_never_seals() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 0);
    f.append(
        3,
        &abi,
        vec![f.action(1, 2, "target", "transfer", "", 1)],
        0,
    );
    let empty = f
        .provider
        .encode_value(&f.schema, "table_delta[]", &json!([]))
        .unwrap();
    let mut first = LogBuilder::new(Encoding::SizedDeflate);
    first.push(synthetic_id(1, 0), &empty);
    first
        .write(&f.directory, "chain_state_history-1-1")
        .unwrap();
    let mut last = LogBuilder::new(Encoding::SizedDeflate);
    last.push(synthetic_id(3, 0), &empty);
    last.write(&f.directory, "chain_state_history-3-3").unwrap();
    f.state = LogBuilder::new(Encoding::SizedDeflate);
    let output = f.run(3, 0);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("hole"));
    assert!(!f.directory.join("abis.ndjson.manifest.json").exists());
}

#[test]
fn setabi_notifications_do_not_add_revisions() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    let mut notification = f.setabi(2, 2, &abi);
    notification[1]["receiver"] = json!("target");
    notification[1]["receipt"][1]["receiver"] = json!("target");
    f.append(
        2,
        &abi,
        vec![
            f.setabi(1, 1, &abi),
            notification,
            f.action(3, 3, "target", "transfer", "", 1),
        ],
        0,
    );
    success(f.run(2, 0));
    let rows = f.rows();
    assert_eq!(
        rows.iter().filter(|r| r["phase"] == "deployment").count(),
        1
    );
    assert_eq!(rows[0]["abiSequence"], "1");
    assert_eq!(f.manifest()["lastGlobalSequence"], "3");
}

#[test]
fn unsupported_transaction_effects_are_refused_instead_of_skipped() {
    for (status, scheduled) in [(1, 0), (2, 0), (3, 0), (4, 0), (0, 1)] {
        let mut f = Fixture::new();
        let abi = f.abi.clone();
        f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 0);
        let mut tx = f.template.clone();
        tx[1]["status"] = json!(status);
        tx[1]["scheduled"] = json!(scheduled);
        tx[1]["action_traces"] = json!([f.setabi(1, 1, &abi)]);
        f.trace = LogBuilder::new(Encoding::SizedDeflate);
        f.trace.push(
            synthetic_id(2, 0),
            &f.provider
                .encode_value(&f.schema, "transaction_trace[]", &json!([tx]))
                .unwrap(),
        );
        let output = f.run(2, 0);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported_feature:"));
        assert!(!f.directory.join("abis.ndjson.manifest.json").exists());
    }
}

fn sha256_file(path: &Path) -> String {
    hex::encode(Sha256::digest(fs::read(path).unwrap()))
}
fn members(out: &Path) -> [Vec<u8>; 3] {
    ["", ".blocks.bin", ".manifest.json"].map(|tail| {
        let mut path = out.as_os_str().to_os_string();
        path.push(tail);
        fs::read(path).unwrap()
    })
}
fn manifest_of(out: &Path) -> Value {
    serde_json::from_slice(&members(out)[2]).unwrap()
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn extension<'a>(manifest: &'a Path, sha: &'a str) -> [&'a str; 4] {
    [
        "--base-manifest",
        manifest.to_str().unwrap(),
        "--base-manifest-sha256",
        sha,
    ]
}

/// Blocks 2..=end, with deployments at work-chunk edges and the rest empty. The deployed ABI
/// cycles through the captured one, a malformed one and none.
fn long_history(f: &mut Fixture, end: u32) -> usize {
    let abi = f.abi.clone();
    let deployments = [2, 2048, 2049, 4096, 4097, end];
    let (mut sequence, mut revision) = (0, 0);
    for block in 2..=end {
        if !deployments.contains(&block) {
            f.empty(block);
            continue;
        }
        let value = match revision % 3 {
            0 => abi.as_str(),
            1 => "00",
            _ => "",
        };
        revision += 1;
        let setabi = f.setabi(1, sequence + 1, value);
        let transfer = f.action(2, sequence + 2, "target", "transfer", "", revision);
        sequence += 2;
        f.append(block, value, vec![setabi, transfer], 0);
    }
    deployments.len()
}

/// Rows are content-addressed by their line's hash, so field order is part of the format.
#[test]
fn rows_and_manifest_keep_their_field_order() {
    let mut f = Fixture::new();
    let abi = f.abi.clone();
    f.append(2, &abi, vec![f.setabi(1, 1, &abi)], 0);
    success(f.run(2, 0));
    let rows = fs::read_to_string(f.directory.join("abis.ndjson")).unwrap();
    let mut lines = rows.lines();
    let id = hex::encode(synthetic_id(2, 0));
    assert!(lines.next().unwrap().starts_with(&format!(
        r#"{{"account":"target","blockNum":2,"blockId":"{id}","phase":"deployment","present":true,"abiHex":"{abi}","abiSequence":"1","action":{{"transactionId":"#
    )));
    assert!(lines.next().unwrap().starts_with(&format!(
        r#"{{"account":"target","blockNum":2,"blockId":"{id}","phase":"block-final","present":true,"abiHex":"{abi}","abiSequence":"1","deltaOrdinal":0,"abi":{{"#
    )));
    let manifest = fs::read_to_string(f.directory.join("abis.ndjson.manifest.json")).unwrap();
    assert!(manifest.starts_with(r#"{"format":"hyperion-wire-abi-history-v1","chainId":"#));
}

#[test]
fn output_is_identical_for_every_thread_count() {
    let mut f = Fixture::new();
    let end = 3 * 2048 + 5;
    let deployments = long_history(&mut f, end);
    let mut outputs = Vec::new();
    for threads in ["1", "3", "16"] {
        let out = f.output(&format!("threads-{threads}"));
        success(f.scan(1, end, 0, &out, &["--threads", threads]));
        outputs.push(members(&out));
    }
    assert!(outputs.windows(2).all(|pair| pair[0] == pair[1]));
    let rows = String::from_utf8(outputs[0][0].clone()).unwrap();
    assert_eq!(rows.matches(r#""phase":"deployment""#).count(), deployments);
}

#[test]
fn extension_reproduces_a_full_scan() {
    let mut f = Fixture::new();
    let end = 3 * 2048 + 5;
    long_history(&mut f, end);
    let full = f.output("full");
    success(f.scan(1, end, 0, &full, &[]));
    // At a deployment, right after one, between two, and one block before a final one.
    for base_end in [2048, 2049, 4100, end - 1] {
        let base = f.output(&format!("base-{base_end}"));
        success(f.scan(1, base_end, 0, &base, &[]));
        let manifest = base.with_file_name("abis.ndjson.manifest.json");
        let sha = sha256_file(&manifest);
        let extended = f.output(&format!("extended-{base_end}"));
        success(f.scan(base_end + 1, end, 0, &extended, &extension(&manifest, &sha)));
        let (got, want) = (members(&extended), members(&full));
        assert_eq!(
            got[..2],
            want[..2],
            "records and block ids from base {base_end}"
        );
        let mut got = manifest_of(&extended);
        assert_eq!(
            got["base"],
            json!({"manifestSha256":sha,"endBlock":base_end,"lastBlockId":hex::encode(synthetic_id(base_end, 0))})
        );
        got.as_object_mut().unwrap().remove("base");
        assert_eq!(got, manifest_of(&full), "manifest from base {base_end}");
    }
}

#[test]
fn extension_refuses_a_base_it_cannot_verify() {
    let mut f = Fixture::new();
    let end = 40;
    let abi = f.abi.clone();
    for block in 2..=end {
        if block == 2 || block == 30 {
            let sequence = u64::from(block);
            f.append(block, &abi, vec![f.setabi(1, sequence, &abi)], 0);
        } else {
            f.empty(block);
        }
    }
    let base = f.output("base");
    success(f.scan(1, 20, 0, &base, &[]));
    let manifest = base.with_file_name("abis.ndjson.manifest.json");
    let sha = sha256_file(&manifest);
    let refused = |f: &Fixture, name: &str, start: u32, end: u32, pin: &str, message: &str| {
        let out = f.output(name);
        let output = f.scan(start, end, 0, &out, &extension(&manifest, pin));
        assert!(!output.status.success(), "{name} was accepted");
        assert!(
            stderr(&output).contains(message),
            "{name}: {}",
            stderr(&output)
        );
        assert!(!out.with_file_name("abis.ndjson.manifest.json").exists());
    };
    refused(
        &f,
        "pin",
        21,
        end,
        &"0".repeat(64),
        "differs from the pinned value",
    );
    refused(&f, "start", 20, end, &sha, "must start at 21");
    refused(&f, "end", 20, 20, &sha, "must end before --end");
    // Another chain's id must not extend this chain's catalog.
    f.chain_id = "33".repeat(32);
    refused(&f, "chain", 21, end, &sha, "belongs to another chain");
    f.chain_id = "22".repeat(32);

    // A member that differs from the pinned manifest: records, then block ids.
    let records = base.clone();
    let original = fs::read(&records).unwrap();
    let mut tampered = original.clone();
    let at = tampered.iter().position(|b| *b == b'2').unwrap();
    tampered[at] = b'3';
    fs::write(&records, &tampered).unwrap();
    refused(&f, "records", 21, end, &sha, "differs from its manifest");
    fs::write(&records, &original).unwrap();
    let ids = base.with_file_name("abis.ndjson.blocks.bin");
    let original = fs::read(&ids).unwrap();
    let mut tampered = original.clone();
    tampered[10 * 32] ^= 1;
    fs::write(&ids, &tampered).unwrap();
    refused(
        &f,
        "ids",
        21,
        end,
        &sha,
        "block membership differs from its manifest",
    );
    fs::write(&ids, &original).unwrap();

    // The logs replaced block 20 and everything after it: the chain forked below the base end.
    f.state.push(synthetic_id(20, 1), &[0]);
    f.trace.push(synthetic_id(20, 1), &[0]);
    for block in 21..=end {
        f.state.push(synthetic_id(block, 1), &[0]);
        f.trace.push(synthetic_id(block, 1), &[0]);
    }
    let out = f.output("fork");
    let output = f.scan(21, end, 1, &out, &extension(&manifest, &sha));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("forked past it"),
        "{}",
        stderr(&output)
    );
    assert!(!out.with_file_name("abis.ndjson.manifest.json").exists());
}

/// Accounts that only ever held an empty ABI have no records, so a base cannot prove their
/// removal; a scan from genesis records it.
#[test]
fn extension_refuses_a_removal_the_base_cannot_prove() {
    let mut f = Fixture::new();
    f.append_row(2, 1, "", vec![f.action(1, 1, "sysio", "onblock", "", 0)], 0);
    f.empty(3);
    f.append_row(4, 0, "", vec![f.action(1, 2, "sysio", "onblock", "", 0)], 0);
    let full = f.output("full");
    success(f.scan(1, 4, 0, &full, &[]));
    let rows = String::from_utf8(members(&full)[0].clone()).unwrap();
    assert_eq!(rows.lines().count(), 1);
    assert!(rows.contains(r#""blockNum":4,"#) && rows.contains(r#""present":false"#));
    let base = f.output("base");
    success(f.scan(1, 3, 0, &base, &[]));
    assert!(members(&base)[0].is_empty());
    let manifest = base.with_file_name("abis.ndjson.manifest.json");
    let sha = sha256_file(&manifest);
    let out = f.output("extended");
    let output = f.scan(4, 4, 0, &out, &extension(&manifest, &sha));
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("cannot prove the state target removes at block 4"),
        "{}",
        stderr(&output)
    );
    assert!(!out.with_file_name("abis.ndjson.manifest.json").exists());
}
