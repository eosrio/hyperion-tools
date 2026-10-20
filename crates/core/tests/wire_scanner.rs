#![cfg(feature = "ship")]
use futures::{SinkExt, StreamExt};
use hyperion_ship::{provider::ProtocolProvider, ship::run_ship_with_provider};
use serde_json::Value;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<u8>>>);
impl Write for Output {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn capture() -> Value {
    serde_json::from_str(include_str!("../testdata/wire/ship-capture.json")).unwrap()
}

#[tokio::test]
async fn advertised_wire_provider_drives_requests_results_and_contract_abi_extraction() {
    let capture = capture();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.send(Message::Text(
            capture["handshakeUtf8"].as_str().unwrap().into(),
        ))
        .await
        .unwrap();
        let request = ws.next().await.unwrap().unwrap().into_data();
        let abi: Value = serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
        let provider: ProtocolProvider = "wire-sysio-7dede884-engineering-v1".parse().unwrap();
        let decoded = provider.decode_value(&abi, "request", &request).unwrap();
        assert_eq!(decoded[0], "get_blocks_request_v0");
        assert_eq!(decoded[1]["fetch_deltas"], 1);
        let frame = capture["vectors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == "captured-ship-block-result")
            .unwrap();
        ws.send(Message::Binary(
            hex::decode(frame["hex"].as_str().unwrap()).unwrap().into(),
        ))
        .await
        .unwrap();
        let ack = ws.next().await.unwrap().unwrap().into_data();
        assert_eq!(
            provider.decode_value(&abi, "request", &ack).unwrap()[0],
            "get_blocks_ack_request_v0"
        );
    });
    let output = Output::default();
    run_ship_with_provider(
        format!("ws://{addr}"),
        6350,
        6350,
        1,
        1,
        true,
        Box::new(output.clone()),
        "wire-sysio-7dede884-engineering-v1".parse().unwrap(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
    let docs: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0]["account"], "sysio");
    assert_eq!(docs[0]["block"], 6350);
    assert_eq!(
        docs[0]["block_id"],
        "000018ce9d1807b4b4d7ef60f99447683e7e480ed17851b87f69ae3f8d3d8ac7"
    );
    assert_eq!(docs[0]["ordinal"], 0);
    assert_eq!(
        docs[0]["protocol_profile"],
        "wire-sysio-7dede884-engineering-v1"
    );
    assert_eq!(docs[0]["abi_scope"], "block-final-state");
    assert!(!docs[0]["abi_hex"].as_str().unwrap().is_empty());
    assert!(docs[0].get("abi_decode_error").is_none());
}

#[tokio::test]
async fn fork_identity_and_original_row_ordinal_survive_abi_scanning() {
    let captured = capture();
    let abi: Value = serde_json::from_str(captured["handshakeUtf8"].as_str().unwrap()).unwrap();
    let provider: ProtocolProvider = "wire-sysio-7dede884-engineering-v1".parse().unwrap();
    let frame = captured["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "captured-ship-block-result")
        .unwrap();
    let decoded = provider
        .decode_bundle(&abi, &hex::decode(frame["hex"].as_str().unwrap()).unwrap())
        .unwrap();
    // Serialization-only fork variants: block identity comes from each SHiP
    // source result. No consensus validity of transformed block bytes is claimed.
    let deltas = decoded["deltas"].as_array().unwrap();
    let account = deltas.iter().find(|v| v[1]["name"] == "account").unwrap();
    let resource = deltas
        .iter()
        .find(|v| v[1]["name"] == "resource_limits")
        .unwrap();
    let reordered = serde_json::json!([resource, account]);
    let deltas = provider
        .encode_value(&abi, "table_delta[]", &reordered)
        .unwrap();
    let mut documents = Vec::new();
    for fill in ['a', 'b'] {
        let id = format!("000018ce{}", fill.to_string().repeat(56));
        let mut envelope = decoded["envelope"].clone();
        envelope[1]["this_block"]["block_id"] = serde_json::json!(id);
        envelope[1]["deltas"] = serde_json::json!(hex::encode(&deltas));
        let frame = provider.encode_value(&abi, "result", &envelope).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handshake = abi.to_string();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(handshake.into())).await.unwrap();
            ws.next().await.unwrap().unwrap();
            ws.send(Message::Binary(frame.into())).await.unwrap();
            ws.next().await.unwrap().unwrap();
        });
        let output = Output::default();
        run_ship_with_provider(
            format!("ws://{addr}"),
            6350,
            6350,
            1,
            1,
            true,
            Box::new(output.clone()),
            provider.clone(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        let text = String::from_utf8(output.0.lock().unwrap().clone()).unwrap();
        let document: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(document["block_id"], id);
        assert_eq!(document["ordinal"], 1);
        assert_eq!(document["protocol_profile"], provider.id());
        assert_eq!(document["abi_scope"], "block-final-state");
        documents.push(document);
    }
    assert_eq!(documents[0]["account"], documents[1]["account"]);
    assert_eq!(documents[0]["abi_hex"], documents[1]["abi_hex"]);
    assert_eq!(documents[0]["block"], documents[1]["block"]);
    assert_ne!(documents[0]["block_id"], documents[1]["block_id"]);
}

#[tokio::test]
async fn wrong_protocol_or_early_close_never_reports_a_successful_scan() {
    for selected in ["antelope", "wire-sysio-7dede884-engineering-v1"] {
        let capture = capture();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            ws.send(Message::Text(
                capture["handshakeUtf8"].as_str().unwrap().into(),
            ))
            .await
            .unwrap();
            ws.close(None).await.unwrap();
        });
        let output = Output::default();
        let result = run_ship_with_provider(
            format!("ws://{addr}"),
            6350,
            6350,
            1,
            1,
            true,
            Box::new(output.clone()),
            selected.parse().unwrap(),
        )
        .await;
        assert!(result.is_err(), "{selected}");
        assert!(output.0.lock().unwrap().is_empty());
        server.await.unwrap();
    }
}

#[tokio::test]
async fn handshake_variant_order_is_used_before_any_request_or_result_fast_path() {
    let capture = capture();
    let mut abi: Value = serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
    for variant in abi["variants"].as_array_mut().unwrap() {
        if matches!(variant["name"].as_str(), Some("request" | "result")) {
            variant["types"].as_array_mut().unwrap().reverse();
        }
    }
    let provider: ProtocolProvider = "wire-sysio-7dede884-engineering-v1".parse().unwrap();
    let original_abi: Value =
        serde_json::from_str(capture["handshakeUtf8"].as_str().unwrap()).unwrap();
    let frame = capture["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == "captured-ship-block-result")
        .unwrap();
    let envelope = provider
        .decode_value(
            &original_abi,
            "result",
            &hex::decode(frame["hex"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
    let frame = provider.encode_value(&abi, "result", &envelope).unwrap();
    assert_ne!(frame[0], 1, "test must bypass the old fixed result index");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        ws.send(Message::Text(abi.to_string().into()))
            .await
            .unwrap();
        let request = ws.next().await.unwrap().unwrap().into_data();
        assert_ne!(
            request[0], 1,
            "request must use the advertised variant order"
        );
        assert_eq!(
            provider.decode_value(&abi, "request", &request).unwrap()[0],
            "get_blocks_request_v0"
        );
        ws.send(Message::Binary(frame.into())).await.unwrap();
        let ack = ws.next().await.unwrap().unwrap().into_data();
        assert_eq!(
            provider.decode_value(&abi, "request", &ack).unwrap()[0],
            "get_blocks_ack_request_v0"
        );
    });
    let output = Output::default();
    run_ship_with_provider(
        format!("ws://{addr}"),
        6350,
        6350,
        1,
        1,
        true,
        Box::new(output.clone()),
        "wire-sysio-7dede884-engineering-v1".parse().unwrap(),
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(
        String::from_utf8(output.0.lock().unwrap().clone())
            .unwrap()
            .lines()
            .count(),
        1
    );
}
