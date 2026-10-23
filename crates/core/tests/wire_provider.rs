use hyperion_ship::provider::{ProtocolProvider, TimestampProfile};
use serde_json::{Value, json};

fn fixture(path: &str) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(format!(
            "{}/testdata/wire/{path}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap(),
    )
    .unwrap()
}

#[test]
fn a_single_validated_profile_drives_top_level_and_nested_timestamps() {
    let abi = json!({"version":"sysio::abi/1.2","types":[],"actions":[],"tables":[],"ricardian_clauses":[],
        "structs":[{"name":"nested","base":"","fields":[{"name":"values","type":"block_timestamp_type[]"}]}]});
    let wire: ProtocolProvider = "wire-sysio-6508c9e8-engineering-v1".parse().unwrap();
    let bytes = [2, 0, 0, 0, 0, 1, 0, 0, 0];
    assert_eq!(
        wire.decode_value(&abi, "nested", &bytes).unwrap(),
        json!({"values":["2025-01-01T00:00:00.000","2025-01-01T00:00:00.500"]})
    );
    assert_eq!(
        wire.timestamp_profile().unwrap().slot_to_iso(1),
        "2025-01-01T00:00:00.500"
    );
    assert_eq!(
        hyperion_ship::blocks::slot_to_iso_with_provider(1, &wire).unwrap(),
        "2025-01-01T00:00:00.500"
    );
    #[cfg(feature = "chain-api")]
    assert_eq!(
        hyperion_ship::chainapi::text::block_timestamp_with_provider(1, &wire).unwrap(),
        "2025-01-01T00:00:00.500"
    );
    assert_eq!(
        TimestampProfile::ANTELOPE.slot_to_iso(1),
        "2000-01-01T00:00:00.500"
    );
    let ProtocolProvider::Wire(mut altered) = wire else {
        unreachable!()
    };
    altered.epoch_milliseconds += 1;
    let invalid = ProtocolProvider::Wire(altered);
    assert!(invalid.timestamp_profile().is_err());
    assert!(invalid.decode_value(&abi, "nested", &bytes).is_err());
}

#[test]
fn complete_captured_transport_keeps_raw_bytes_and_corrects_only_the_pinned_bug() {
    let abi = fixture("ship.abi.json");
    let capture = fixture("ship-capture.json");
    let frame = capture["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "captured-ship-block-result")
        .unwrap();
    let bytes = hex::decode(frame["hex"].as_str().unwrap()).unwrap();
    let wire: ProtocolProvider = "wire-sysio-7dede884-engineering-v1".parse().unwrap();
    let bundle = wire.decode_bundle(&abi, &bytes).unwrap();
    assert_eq!(bundle["rawFrameHex"], frame["hex"]);
    assert_eq!(bundle["block"]["timestamp"], "2026-10-19T21:08:19.500");
    assert_eq!(
        bundle["deltaFraming"]["compatibility"],
        "wire-resource-index-count-v1"
    );
    assert!(bundle["traces"].as_array().is_some());
    assert!(
        bundle["deltaRows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["stream"] == "account")
    );
    assert!(wire.decode_bundle(&abi, &bytes[..bytes.len() - 1]).is_err());
    assert!(wire.require_antelope("legacy archive").is_err());
    assert!(
        ProtocolProvider::default()
            .require_antelope("legacy archive")
            .is_ok()
    );
}

#[test]
fn exact_locked_abieos_preserves_sysio_table_metadata() {
    let vectors = fixture("oracle-vectors.json");
    let vector = vectors["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "6508c9e8-abi-test_kv_sec_query")
        .unwrap();
    let bytes = hex::decode(vector["hex"].as_str().unwrap()).unwrap();
    let engine = rs_abieos::Abieos::new();
    let decoded: Value = serde_json::from_str(&engine.abi_bin_to_json(&bytes).unwrap()).unwrap();
    // Compare with the retained independent clio observation, not a second codec's output.
    assert_eq!(decoded["version"], vector["expected"]["version"]);
    assert_eq!(decoded["tables"], vector["expected"]["tables"]);
    let handle = rs_abieos::AbiHandle::from_bin(&bytes).unwrap();
    assert_eq!(handle.type_for_table_name("users"), Some("user_val"));
}

#[test]
fn nonempty_receipts_and_traces_preserve_distinct_source_fields() {
    let fixture: Value =
        serde_json::from_str(include_str!("../testdata/wire-host-receipts.json")).unwrap();
    assert_eq!(fixture["consensusValidation"], false);
    let provider: ProtocolProvider = fixture["profileId"].as_str().unwrap().parse().unwrap();
    let bundle = provider
        .decode_bundle(
            &fixture["handshake"],
            &hex::decode(fixture["frameHex"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
    assert_eq!(bundle, fixture["expected"]);
    let receipt = &bundle["block"]["transactions"][0];
    assert_eq!(receipt["cpu_usage_us"], json!([23, 456]));
    assert!(receipt["trx"].is_object());
    assert!(receipt.get("status").is_none());
    assert!(receipt.get("net_usage_words").is_none());
    let trace = &bundle["traces"][0][1];
    assert_eq!(trace["status"], 0);
    assert_eq!(trace["cpu_usage_us"], 479);
    assert_eq!(trace["net_usage_words"], 9);
    assert_eq!(trace["net_usage"], "65");
}
