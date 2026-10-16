//! SHIP v0 WebSocket acquisition of packed blocks and history payloads. Network data is
//! staged in a new directory, never written to the target.
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, protocol::WebSocketConfig},
};

use crate::format::{
    Blocks, Cursor, Kind, Log, block_header, encode_container, encode_ship, num, u32le,
};
use crate::io::{MAX_BYTES, create_dir, create_file, hash};
use crate::repair::{LogFile, Sources, rebuild, target_flags};
use crate::report::{Out, arg};
use crate::target::{Extent, Target};

/// Results the server may send ahead of acknowledgements. Each result is acknowledged on
/// receipt, and the per-endpoint byte budget still bounds the whole transfer.
pub const IN_FLIGHT: u32 = 16;
/// get_blocks_ack_request_v0 { num_messages: 1 }
const ACK: [u8; 5] = [2, 1, 0, 0, 0];

pub fn request(first: u32, last: u32, kind: Kind) -> Result<Vec<u8>> {
    let mut b = vec![1];
    b.extend_from_slice(&first.to_le_bytes());
    b.extend_from_slice(
        &last
            .checked_add(1)
            .context("SHIP range overflow")?
            .to_le_bytes(),
    );
    b.extend_from_slice(&IN_FLIGHT.to_le_bytes());
    b.extend_from_slice(&[
        0,
        1,
        1,
        u8::from(kind == Kind::Trace),
        u8::from(kind == Kind::ChainState),
    ]);
    Ok(b)
}

fn fields(abi: &Value, name: &str, expected: &[(&str, &str)]) -> Result<()> {
    let s = abi["structs"]
        .as_array()
        .context("SHIP ABI missing structs")?
        .iter()
        .find(|s| s["name"] == name)
        .with_context(|| format!("SHIP ABI missing {name}"))?;
    ensure!(
        s.get("base").is_none_or(|v| v == ""),
        "unsupported SHIP ABI inheritance"
    );
    let f = s["fields"].as_array().context("SHIP ABI missing fields")?;
    ensure!(
        f.len() == expected.len()
            && f.iter()
                .zip(expected)
                .all(|(v, (n, t))| v["name"] == *n && v["type"] == *t),
        "unsupported SHIP ABI layout for {name}"
    );
    Ok(())
}

pub fn validate_abi(text: &str) -> Result<()> {
    let abi: Value = serde_json::from_str(text).context("SHIP protocol ABI is not JSON")?;
    for (name, prefix) in [
        (
            "request",
            &[
                "get_status_request_v0",
                "get_blocks_request_v0",
                "get_blocks_ack_request_v0",
            ][..],
        ),
        (
            "result",
            &["get_status_result_v0", "get_blocks_result_v0"][..],
        ),
    ] {
        let v = abi["variants"]
            .as_array()
            .context("SHIP ABI missing variants")?
            .iter()
            .find(|v| v["name"] == name)
            .context("SHIP ABI variant missing")?;
        let types = v["types"]
            .as_array()
            .context("SHIP ABI variant types missing")?;
        ensure!(
            types.len() >= prefix.len() && types.iter().zip(prefix).all(|(a, b)| a == b),
            "unsupported SHIP {name} variant ordering"
        );
    }
    fields(
        &abi,
        "block_position",
        &[("block_num", "uint32"), ("block_id", "checksum256")],
    )?;
    fields(
        &abi,
        "get_blocks_request_v0",
        &[
            ("start_block_num", "uint32"),
            ("end_block_num", "uint32"),
            ("max_messages_in_flight", "uint32"),
            ("have_positions", "block_position[]"),
            ("irreversible_only", "bool"),
            ("fetch_block", "bool"),
            ("fetch_traces", "bool"),
            ("fetch_deltas", "bool"),
        ],
    )?;
    fields(
        &abi,
        "get_blocks_ack_request_v0",
        &[("num_messages", "uint32")],
    )?;
    fields(
        &abi,
        "get_blocks_result_v0",
        &[
            ("head", "block_position"),
            ("last_irreversible", "block_position"),
            ("this_block", "block_position?"),
            ("prev_block", "block_position?"),
            ("block", "bytes?"),
            ("traces", "bytes?"),
            ("deltas", "bytes?"),
        ],
    )
}

fn position(c: &mut Cursor<'_>) -> Result<(u32, [u8; 32])> {
    let n = u32le(c.take(4)?);
    let id = c.id()?;
    ensure!(num(&id) == n, "SHIP position number disagrees with id");
    Ok((n, id))
}
fn opt_position(c: &mut Cursor<'_>) -> Result<Option<(u32, [u8; 32])>> {
    match c.byte()? {
        0 => Ok(None),
        1 => Ok(Some(position(c)?)),
        _ => bail!("invalid SHIP optional flag"),
    }
}
fn opt_bytes(c: &mut Cursor<'_>) -> Result<Option<Vec<u8>>> {
    match c.byte()? {
        0 => Ok(None),
        1 => {
            let n = c.var()?;
            Ok(Some(c.take(n as usize)?.to_vec()))
        }
        _ => bail!("invalid SHIP optional bytes flag"),
    }
}

pub struct Recovered {
    pub number: u32,
    pub block: Vec<u8>,
    /// Trace or delta payload; `None` for block-log targets.
    pub payload: Option<Vec<u8>>,
}
pub fn decode(bytes: &[u8], kind: Kind) -> Result<Recovered> {
    let mut c = Cursor::new(bytes);
    ensure!(c.var()? == 1, "expected get_blocks_result_v0");
    let (head, _) = position(&mut c)?;
    let (lib, _) = position(&mut c)?;
    let (n, id) = opt_position(&mut c)?.context("SHIP result has no block")?;
    let (pn, prev) = opt_position(&mut c)?.context("SHIP result has no previous block")?;
    ensure!(
        n <= lib && lib <= head && pn.checked_add(1) == Some(n),
        "SHIP source returned reversible or discontinuous history"
    );
    let block = opt_bytes(&mut c)?.context("SHIP source did not supply packed block bytes")?;
    let traces = opt_bytes(&mut c)?;
    let deltas = opt_bytes(&mut c)?;
    ensure!(c.pos == bytes.len(), "trailing bytes in SHIP result");
    let h = block_header(&block)?;
    ensure!(
        h.number == n && h.id == id && h.previous == prev,
        "SHIP block header does not match result positions"
    );
    let payload = match kind {
        Kind::Blocks => None,
        Kind::Trace | Kind::ChainState => {
            let payload = if kind == Kind::Trace { traces } else { deltas }
                .context("SHIP source does not hold requested history payload")?;
            ensure!(
                !payload.is_empty(),
                "SHIP source returned an empty history payload"
            );
            Some(payload)
        }
    };
    Ok(Recovered {
        number: n,
        block,
        payload,
    })
}

/// One endpoint: validated ABI, then every result in `first..=last`, in order.
async fn fetch_endpoint(url: &str, first: u32, last: u32, kind: Kind) -> Result<Vec<Recovered>> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_BYTES as usize))
        .max_frame_size(Some(MAX_BYTES as usize));
    let (mut ws, _) = connect_async_with_config(url, Some(config), false)
        .await
        .context("connect SHIP WebSocket")?;
    let abi = match ws.next().await.context("SHIP closed before ABI")?? {
        Message::Text(s) => s,
        _ => bail!("expected SHIP protocol ABI text"),
    };
    validate_abi(&abi)?;
    ws.send(Message::Binary(request(first, last, kind)?.into()))
        .await?;
    let mut out = Vec::new();
    let mut total = 0u64;
    let mut expected = first;
    loop {
        let bytes = match ws
            .next()
            .await
            .context("SHIP closed before complete range")??
        {
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Binary(bytes) => bytes,
            _ => bail!("unexpected SHIP message before range completion"),
        };
        let r = decode(&bytes, kind)?;
        ensure!(
            r.number == expected,
            "SHIP returned block {}, expected {expected}; gaps/forks/duplicates are refused",
            r.number
        );
        let size = r.block.len() + r.payload.as_ref().map_or(0, Vec::len);
        total = total
            .checked_add(size as u64)
            .context("source size overflow")?;
        ensure!(
            total <= MAX_BYTES,
            "SHIP range exceeds 256 MiB safety budget; use a disk donor for large entries"
        );
        out.push(r);
        if expected == last {
            break;
        }
        ws.send(Message::Binary(ACK.to_vec().into())).await?;
        expected += 1;
    }
    let _ = ws.close(None).await;
    Ok(out)
}

const TRUST: &str = "Block headers are authenticated by the id chain to both local anchors; \
    transaction bodies and SHIP traces/deltas are not. Use trusted independent operators; with \
    several endpoints every byte must agree. SHIP entries are re-encoded as stored zlib, and \
    another compressor may not fit. Incompatible artifacts are kept for diagnosis; build and \
    apply refuse them.";

/// One region's agreed results and the SHIP payload marker of its anchors.
struct Fetched {
    first: u32,
    last: u32,
    marker: Option<u32>,
    results: Vec<Recovered>,
}

pub async fn fetch(
    log: &Log,
    t: &Target,
    urls: &[String],
    out: &Path,
    timeout_secs: u64,
) -> Result<Out> {
    ensure!(
        (1..=3600).contains(&timeout_secs),
        "timeout must be between 1 and 3600 seconds"
    );
    ensure!(!urls.is_empty(), "at least one --ship endpoint is required");
    for (i, url) in urls.iter().enumerate() {
        ensure!(
            (url.starts_with("ws://") || url.starts_with("wss://")) && !url.contains('@'),
            "use ws:// or wss:// without embedded credentials"
        );
        ensure!(!urls[..i].contains(url), "duplicate --ship endpoint {url}");
    }
    ensure!(!out.exists(), "source output directory already exists");
    let before = log.log.stamp()?;
    let idx_before = log.index.stamp()?;
    let mut fetched = Vec::new();
    for r in &t.regions {
        let (first, last) = r.source_range();
        let marker = if log.kind == Kind::Blocks {
            None
        } else {
            let left = log.body(first)?;
            let right = log.body(last)?;
            let marker = u32le(&left[48..]);
            ensure!(
                marker.min(2) == u32le(&right[48..]).min(2),
                "SHIP payload header format changes across blocks {first}..{last}; use a \
                 matching disk donor"
            );
            Some(marker)
        };
        let mut agreed: Option<Vec<Recovered>> = None;
        for (i, url) in urls.iter().enumerate() {
            eprintln!(
                "Fetching irreversible blocks {first}..{last} from endpoint {}/{}; \
                 target remains read-only.",
                i + 1,
                urls.len()
            );
            let deadline = Duration::from_secs(timeout_secs);
            let got = tokio::time::timeout(deadline, fetch_endpoint(url, first, last, log.kind))
                .await
                .with_context(|| {
                    format!("SHIP acquisition from {url} timed out (no target writes performed)")
                })?
                .with_context(|| format!("SHIP acquisition from {url}"))?;
            match &agreed {
                None => agreed = Some(got),
                Some(prev) => {
                    for (a, b) in prev.iter().zip(&got) {
                        ensure!(
                            a.block == b.block && a.payload == b.payload,
                            "SHIP endpoints disagree at block {}; all sources refused",
                            a.number
                        );
                    }
                }
            }
        }
        fetched.push(Fetched {
            first,
            last,
            marker,
            results: agreed.context("no endpoint returned data")?,
        });
    }
    ensure!(
        log.log.stamp()? == before && log.index.stamp()? == idx_before,
        "target changed during fetch; stop nodeos and repeat"
    );

    create_dir(out)?;
    // source.json names files relative to `out`, so the directory can be moved.
    let mut relative = Sources::default();
    let mut files = Vec::new();
    let mut block_count = 0;
    for f in fetched {
        let mut blocks = Blocks::new();
        let mut entries = Vec::new();
        for r in f.results {
            if let (Some(marker), Some(payload)) = (f.marker, &r.payload) {
                entries.push(encode_ship(&block_header(&r.block)?.id, payload, marker)?);
            }
            blocks.insert(r.number, r.block);
        }
        block_count += blocks.len();
        let container = encode_container(&blocks)?;
        let blocks_name = format!("blocks-{}-{}.p2pblk", f.first, f.last);
        create_file(&out.join(&blocks_name), &container)?;
        relative.blocks.push(blocks_name.into());
        let mut file = json!({
            "range": [f.first, f.last],
            "blocks_sha256": hash(&container),
        });
        if log.kind != Kind::Blocks {
            let mut bytes = Vec::new();
            let mut index = Vec::new();
            for body in &entries {
                let p = bytes.len() as u64;
                index.extend_from_slice(&p.to_le_bytes());
                bytes.extend_from_slice(body);
                bytes.extend_from_slice(&p.to_le_bytes());
            }
            let stem = format!("{}-{}-{}", log.kind.stem(), f.first, f.last);
            create_file(&out.join(format!("{stem}.log")), &bytes)?;
            create_file(&out.join(format!("{stem}.index")), &index)?;
            relative.donors.push(LogFile {
                log: format!("{stem}.log").into(),
                index: None,
            });
            file["donor_sha256"] = json!(hash(&bytes));
        }
        files.push(file);
    }
    let validation = relative
        .under(out)
        .load(log.kind)
        .and_then(|loaded| rebuild(log, t, &loaded));
    let report = json!({
        "ok": validation.is_ok(),
        "source": "ship_websocket",
        "endpoints": urls,
        "regions": files,
        "sources": relative,
        "target_compatible": validation.is_ok(),
        "compatibility_error": validation.as_ref().err().map(|e| format!("{e:#}")),
        "trust": TRUST,
    });
    create_file(
        &out.join("source.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    validation.context(
        "fetched data was saved but does not fit the target; see source.json; use a matching \
         archive or blocklog+snapshot replay",
    )?;
    let extents: Vec<Extent> = t.extents().copied().collect();
    let text = format!(
        "Fetched {block_count} block(s) for {} region(s) from {} endpoint(s) into {}.\n\
         All endpoints agree, and the data fits the target's lengths and readable bytes.\n\n\
         Trust: {TRUST}\n\nNext:\n  antelope-log-repair build {} --sources {} --out BUNDLE\n",
        t.regions.len(),
        urls.len(),
        out.display(),
        target_flags(log, t.record_size, &extents),
        arg(out.to_string_lossy()),
    );
    Ok(Out { json: report, text })
}
