//! SHiP websocket scanner.
//!
//! Requests **deltas only** (`fetch_block=0, fetch_traces=0, fetch_deltas=1`),
//! zero-copy-parses the get_blocks_result envelope, and walks only the `account`
//! table. With N connections the range is split into N contiguous chunks.
//! Point `--ship` at a fleet-router for resilient, range-aware multi-node fan-out.

use std::io::Write;

use anyhow::{anyhow, bail, Context, Result};
use futures::{SinkExt, StreamExt};
use rs_abieos::Abieos;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::tungstenite::Error as WsError;

use crate::abi::build_abi_doc;
use crate::delta::{account_setabi, for_each_account_row, parse_result};

/// Per-message/frame ceiling for the SHiP websocket. A snapshot-restored node's
/// full-state init delta exceeds any single-frame limit; that case is served by
/// `--from-disk`, and a frame over this size yields a hint pointing there.
const FRAME_LIMIT_BYTES: usize = 1_073_741_824; // 1 GiB

/// Build a get_blocks_request_v0 (variant byte `1`). The variant index of
/// get_blocks_request_v0 is stable across every Leap/Spring SHiP ABI.
pub fn build_get_blocks_request(
    start: u32,
    end: u32,
    in_flight: u32,
    irreversible: bool,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(32);
    b.push(1u8); // get_blocks_request_v0
    b.extend_from_slice(&start.to_le_bytes());
    b.extend_from_slice(&end.saturating_add(1).to_le_bytes()); // end_block_num is exclusive
    b.extend_from_slice(&in_flight.to_le_bytes());
    b.push(0u8); // have_positions: empty vector
    b.push(irreversible as u8);
    b.push(0u8); // fetch_block
    b.push(0u8); // fetch_traces
    b.push(1u8); // fetch_deltas
    b
}

/// Build a get_blocks_ack_request_v0 (variant byte `2`).
pub fn build_ack(num: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(5);
    b.push(2u8); // get_blocks_ack_request_v0
    b.extend_from_slice(&num.to_le_bytes());
    b
}

/// Scan one contiguous chunk over its own SHiP connection. Found ABI docs are
/// sent (as NDJSON strings) over `sink`. Returns (blocks_scanned, abis_found).
async fn scan_range(
    id: u32,
    ship: String,
    start: u32,
    end: u32,
    in_flight: u32,
    irreversible: bool,
    sink: mpsc::Sender<String>,
) -> Result<(u32, u64)> {
    let mut config = WebSocketConfig::default();
    config.max_message_size = Some(FRAME_LIMIT_BYTES);
    config.max_frame_size = Some(FRAME_LIMIT_BYTES);

    let (ws, _) = connect_async_with_config(&ship, Some(config), true)
        .await
        .with_context(|| format!("[c{id}] ship connect"))?;
    let (mut tx, mut rx) = ws.split();

    let abi_text = match rx.next().await {
        Some(Ok(Message::Text(t))) => t.to_string(),
        other => bail!("[c{id}] expected protocol ABI, got {other:?}"),
    };
    let abieos = Abieos::new();
    abieos
        .set_abi_json("0", &abi_text)
        .map_err(|e| anyhow!("[c{id}] set ship abi: {e:?}"))?;

    tx.send(Message::Binary(
        build_get_blocks_request(start, end, in_flight, irreversible).into(),
    ))
    .await
    .with_context(|| format!("[c{id}] send request"))?;

    let mut processed = 0u32;
    let mut found = 0u64;
    while let Some(msg) = rx.next().await {
        let msg = match msg {
            Ok(m) => m,
            // A frame over the websocket limit is, in practice, the full-state init delta a
            // snapshot-restored node emits on its first block — it can't fit in one frame.
            Err(WsError::Capacity(c)) => bail!(
                "[c{id}] SHiP frame exceeded the {} GiB websocket limit ({c}). This is the \
                 full-state init delta a snapshot-restored node emits on its first block; it \
                 cannot fit in a single websocket frame. Read it with --from-disk instead.",
                FRAME_LIMIT_BYTES >> 30
            ),
            Err(e) => return Err(anyhow!("[c{id}] stream: {e}")),
        };
        let Message::Binary(bin) = msg else {
            continue;
        };
        let Some((block_num, deltas)) = parse_result(&bin) else {
            tx.send(Message::Binary(build_ack(1).into())).await.ok();
            continue;
        };
        if !deltas.is_empty() {
            let r = for_each_account_row(deltas, |row| {
                if let Some((name, abi_hex)) = account_setabi(&abieos, row)? {
                    sink.try_send(build_abi_doc(&abieos, &name, block_num, &abi_hex))
                        .ok();
                    found += 1;
                    eprintln!("[c{id}] setabi: {name} @ {block_num}");
                }
                Ok(())
            });
            if let Err(e) = r {
                eprintln!("[c{id}] WARN block {block_num}: {e}");
            }
        }
        tx.send(Message::Binary(build_ack(1).into())).await.ok();
        processed += 1;
        // is_multiple_of() is only stable since 1.87, above our 1.74 MSRV — keep the `%` form.
        #[allow(clippy::manual_is_multiple_of)]
        if processed % 20000 == 0 {
            eprintln!("[c{id}] {processed} blocks ({found} ABIs) at {block_num}");
        }
        if block_num >= end {
            break;
        }
    }
    Ok((processed, found))
}

/// Drive a SHiP scan: split `[start..end]` into `connections` contiguous chunks,
/// each scanned over its own connection, and stream found docs to `out`.
pub async fn run_ship(
    ship: String,
    start: u32,
    end: u32,
    in_flight: u32,
    connections: u32,
    irreversible: bool,
    mut out: Box<dyn Write + Send>,
) -> Result<()> {
    let conns = connections.max(1);
    let total = (end - start + 1) as u64;
    let per = total.div_ceil(conns as u64);
    eprintln!(
        "[abi-scanner] {} block(s) [{}..{}] over {conns} connection(s) to {}",
        total, start, end, ship
    );

    // Single writer task drains the channel to the output sink.
    let (tx, mut rx) = mpsc::channel::<String>(4096);
    let writer = tokio::spawn(async move {
        let mut n = 0u64;
        while let Some(line) = rx.recv().await {
            let _ = writeln!(out, "{}", line);
            n += 1;
        }
        let _ = out.flush();
        n
    });

    let mut handles = Vec::new();
    for i in 0..conns {
        let c_start = start + (i as u64 * per) as u32;
        if c_start > end {
            break;
        }
        let c_end = ((c_start as u64 + per - 1) as u32).min(end);
        let h = tokio::spawn(scan_range(
            i,
            ship.clone(),
            c_start,
            c_end,
            in_flight,
            irreversible,
            tx.clone(),
        ));
        handles.push(h);
    }
    drop(tx); // close the channel once all senders (held by tasks) finish

    let mut blocks = 0u32;
    let mut found = 0u64;
    for h in handles {
        match h.await {
            Ok(Ok((b, f))) => {
                blocks += b;
                found += f;
            }
            Ok(Err(e)) => eprintln!("[abi-scanner] connection error: {e:#}"),
            Err(e) => eprintln!("[abi-scanner] task join error: {e}"),
        }
    }
    let written = writer.await.unwrap_or(0);
    eprintln!(
        "[abi-scanner] done: {blocks} blocks scanned, {found} ABI versions ({written} written)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_deltas_only_v0() {
        let req = build_get_blocks_request(2, 100, 50, true);
        assert_eq!(req[0], 1); // get_blocks_request_v0 variant
        assert_eq!(u32::from_le_bytes(req[1..5].try_into().unwrap()), 2); // start
        assert_eq!(u32::from_le_bytes(req[5..9].try_into().unwrap()), 101); // end exclusive
                                                                            // tail: have_positions=0, irreversible=1, fetch_block=0, fetch_traces=0, fetch_deltas=1
        assert_eq!(&req[13..18], &[0, 1, 0, 0, 1]);
        assert_eq!(build_ack(1), vec![2, 1, 0, 0, 0]);
    }
}
