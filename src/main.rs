//! abi-scanner — high-performance Antelope SHiP ABI scanner.
//!
//! Streams a block range from a SHiP endpoint requesting **deltas only**
//! (no blocks, no traces), zero-copy-parses the get_blocks_result envelope to
//! slice out the `deltas` bytes, then custom-parses `table_delta[]` to touch
//! ONLY the `account` table — skipping the dense `contract_row` payload
//! entirely. Each setabi (non-empty `abi` on an account row) is decoded via
//! rs_abieos and emitted as an NDJSON line in the Hyperion abi-index shape:
//!   {account, block, abi, abi_hex, actions[], tables[]}
//!
//! With --connections N the range is split into N contiguous chunks scanned
//! concurrently. Point --ship at a fleet-router for resilient, range-aware
//! multi-node fan-out (each chunk is routed to a node that has the range).

use std::fs::File;
use std::io::{BufWriter, Write};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use rs_abieos::Abieos;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async_with_config;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

#[derive(Parser, Debug, Clone)]
#[command(about = "Scan a SHiP block range for contract ABI versions (setabi), deltas-only.")]
struct Args {
    /// SHiP websocket endpoint (a fleet-router or a nodeos SHiP node). Omit when using --from-disk.
    #[arg(long)]
    ship: Option<String>,
    /// Read directly from the nodeos state-history dir (contains chain_state_history.{log,index}).
    /// Bypasses nodeos/SHiP entirely — read-only, parallel, decodes from the append-only log.
    #[arg(long)]
    from_disk: Option<String>,
    /// First block to scan (inclusive)
    #[arg(long)]
    start: u32,
    /// Last block to scan (inclusive)
    #[arg(long)]
    end: u32,
    /// Output NDJSON file (one abi-index doc per line). Defaults to stdout.
    #[arg(long)]
    out: Option<String>,
    /// SHiP max_messages_in_flight (flow-control window) per connection
    #[arg(long, default_value_t = 50)]
    in_flight: u32,
    /// SHiP: parallel connections. Disk: parallel reader threads. (range split into N contiguous chunks)
    #[arg(long, default_value_t = 1)]
    connections: u32,
    /// Disk reader threads (defaults to --connections if unset)
    #[arg(long)]
    threads: Option<u32>,
    /// Request irreversible blocks only (safe for historical ranges)
    #[arg(long, default_value_t = true)]
    irreversible_only: bool,
}

/// Read a LEB128 varuint32. Returns (value, bytes_consumed).
fn read_varuint(buf: &[u8]) -> Option<(usize, usize)> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    let mut i = 0usize;
    loop {
        let byte = *buf.get(i)?;
        i += 1;
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value as usize, i));
        }
        shift += 7;
        if shift > 35 {
            return None;
        }
    }
}

/// Parse a get_blocks_result_v0 envelope (zero-copy). Returns (block_num, deltas_bytes).
/// Layout: variant(1) head(36) lib(36) this_block(opt 36) prev_block(opt 36)
///         block(opt bytes) traces(opt bytes) deltas(opt bytes).
fn parse_result(bin: &[u8]) -> Option<(u32, &[u8])> {
    if bin.first().copied() != Some(1) {
        return None; // not get_blocks_result_v0
    }
    let mut off = 1usize + 36 + 36; // variant + head + last_irreversible
    let this_present = *bin.get(off)?;
    off += 1;
    let block_num;
    if this_present == 1 {
        block_num = u32::from_le_bytes(bin.get(off..off + 4)?.try_into().ok()?);
        off += 36;
    } else {
        return None; // idle / no block in this message
    }
    let prev_present = *bin.get(off)?;
    off += 1;
    if prev_present == 1 {
        off += 36;
    }
    for present_optional_bytes in 0..2 {
        // block, traces — skip if present
        let present = *bin.get(off)?;
        off += 1;
        if present == 1 {
            let (len, k) = read_varuint(bin.get(off..)?)?;
            off += k + len;
        }
        let _ = present_optional_bytes;
    }
    // deltas (optional bytes)
    let deltas_present = *bin.get(off)?;
    off += 1;
    if deltas_present == 1 {
        let (len, k) = read_varuint(bin.get(off..)?)?;
        off += k;
        return Some((block_num, bin.get(off..off + len)?));
    }
    Some((block_num, &[]))
}

/// Walk table_delta[] and call `f` on each `account` table row (raw bytes),
/// skipping all other tables (e.g. the dense contract_row) by length.
fn for_each_account_row<F: FnMut(&[u8]) -> Result<()>>(deltas: &[u8], mut f: F) -> Result<()> {
    let mut off = 0usize;
    let (n_tables, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad table count"))?;
    off += k;
    for _ in 0..n_tables {
        let (_variant, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad variant"))?;
        off += k;
        let (name_len, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad name len"))?;
        off += k;
        let name = deltas
            .get(off..off + name_len)
            .ok_or_else(|| anyhow!("name oob"))?;
        off += name_len;
        let (rows, k) = read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad rows count"))?;
        off += k;
        let is_account = name == b"account";
        for _ in 0..rows {
            off += 1; // present byte
            let (data_len, k) =
                read_varuint(&deltas[off..]).ok_or_else(|| anyhow!("bad data len"))?;
            off += k;
            if is_account {
                let data = deltas
                    .get(off..off + data_len)
                    .ok_or_else(|| anyhow!("data oob"))?;
                f(data)?;
            }
            off += data_len;
        }
    }
    Ok(())
}

/// Decode a SHiP `account` table row **manually** (no SHiP ABI required):
///   [varuint variant=0][name u64][creation_date u32][abi: varuint len + bytes]
/// Returns (account_name, abi_hex) when the row carries a non-empty ABI (a setabi).
/// Works identically for the SHiP and from-disk paths.
fn account_setabi(abieos: &Abieos, row: &[u8]) -> Result<Option<(String, String)>> {
    let (_variant, k) = read_varuint(row).ok_or_else(|| anyhow!("account variant"))?;
    let mut off = k;
    if off + 12 > row.len() {
        return Ok(None);
    }
    let name_u64 = u64::from_le_bytes(row[off..off + 8].try_into().unwrap());
    off += 8;
    off += 4; // creation_date (block_timestamp_type, u32)
    let (abi_len, k) = read_varuint(&row[off..]).ok_or_else(|| anyhow!("account abi len"))?;
    off += k;
    if abi_len == 0 {
        return Ok(None);
    }
    let abi_bytes = row
        .get(off..off + abi_len)
        .ok_or_else(|| anyhow!("account abi oob"))?;
    let name = abieos
        .name_to_string(name_u64)
        .map_err(|e| anyhow!("name_to_string: {e:?}"))?;
    Ok(Some((name, hex::encode(abi_bytes))))
}

/// zlib-inflate a SHiP log payload (the bytes after the 4-byte size prefix).
fn inflate(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut dec = flate2::read::ZlibDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).context("zlib inflate")?;
    Ok(out)
}

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The high 32 bits of a state-history entry magic are the "ship" name; the low
/// 16 are the version. Validates we're really reading a state-history log.
fn is_ship_magic(magic: u64) -> bool {
    (magic & 0xffff_ffff_0000_0000) == 0xc35d_5000_0000_0000
}

/// Decode the zlib payload of a state-history entry into the raw table_delta[] bytes.
/// Handles both Leap encodings: `[u32=1][u64 decompressed_size][zlib]` and the
/// default `[u32 (ignored)][zlib]` (see read_unpacked_entry in Leap's log.hpp).
fn decode_payload(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() < 4 {
        return Ok(Vec::new());
    }
    let s = u32::from_le_bytes(payload[0..4].try_into().unwrap());
    let zstart = if s == 1 && payload.len() >= 12 { 12 } else { 4 };
    if payload.len() <= zstart {
        return Ok(Vec::new());
    }
    inflate(&payload[zstart..])
}

/// Scan one contiguous block chunk: seek to the chunk's start offset (via the index),
/// then read entries sequentially. Found ABI docs are streamed out over `sink`.
/// Per-block/row errors are logged and skipped — a single bad entry never aborts the scan.
fn worker_scan(
    log_path: &str,
    idx_path: &str,
    first_block: u32,
    cs: u32,
    ce: u32,
    sink: &std::sync::mpsc::Sender<String>,
    scanned: &AtomicU64,
) -> Result<()> {
    use std::io::{BufReader, Read, Seek, SeekFrom};
    let abieos = Abieos::new();
    let mut idx = File::open(idx_path)?;
    idx.seek(SeekFrom::Start((cs - first_block) as u64 * 8))?;
    let mut ob = [0u8; 8];
    idx.read_exact(&mut ob)?;
    let mut log = BufReader::with_capacity(1 << 20, File::open(log_path)?);
    log.seek(SeekFrom::Start(u64::from_le_bytes(ob)))?;

    let mut hdr = [0u8; 48];
    loop {
        if log.read_exact(&mut hdr).is_err() {
            break; // clean EOF (or truncated tail) — stop this worker
        }
        let block_num = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
        if block_num > ce {
            break;
        }
        let payload_size = u64::from_le_bytes(hdr[40..48].try_into().unwrap()) as usize;
        if payload_size > (1usize << 31) {
            // a sane delta payload never approaches 2 GiB — this means our offset is wrong.
            return Err(anyhow!(
                "payload_size {payload_size} at block {block_num}: format/offset error"
            ));
        }
        let mut payload = vec![0u8; payload_size];
        log.read_exact(&mut payload)?;
        log.seek_relative(8)?; // trailing entry-position uint64
        scanned.fetch_add(1, Relaxed);

        let deltas = match decode_payload(&payload) {
            Ok(d) if !d.is_empty() => d,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("[disk] block {block_num}: inflate failed: {e}");
                continue;
            }
        };
        let walk = for_each_account_row(&deltas, |row| {
            match account_setabi(&abieos, row) {
                Ok(Some((name, abi_hex))) => {
                    let _ = sink.send(build_abi_doc(&abieos, &name, block_num, &abi_hex));
                }
                Ok(None) => {}
                Err(e) => eprintln!("[disk] block {block_num}: account row: {e}"),
            }
            Ok(())
        });
        if let Err(e) = walk {
            eprintln!("[disk] block {block_num}: delta walk: {e}");
        }
    }
    Ok(())
}

/// Read ABIs directly from the append-only state-history log (no nodeos), in parallel,
/// streaming results to `out`. Bounds the range to the indexed (committed) blocks so it
/// never races the entry nodeos is currently appending.
fn scan_disk<W: Write + Send>(
    dir: &str,
    start: u32,
    end: u32,
    threads: u32,
    out: &mut W,
) -> Result<()> {
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};

    let log_path = format!("{dir}/chain_state_history.log");
    let idx_path = format!("{dir}/chain_state_history.index");
    let mut f = File::open(&log_path).with_context(|| format!("open {log_path}"))?;
    let mut hdr = [0u8; 48];
    f.read_exact(&mut hdr).context("read first header")?;
    if !is_ship_magic(u64::from_le_bytes(hdr[0..8].try_into().unwrap())) {
        bail!("{log_path} is not a state-history log (bad ship magic)");
    }
    let first_block = u32::from_be_bytes(hdr[8..12].try_into().unwrap());
    // The index has one committed entry per block; clamp to it so we never read the live tail.
    let n_idx = (std::fs::metadata(&idx_path)?.len() / 8) as u32;
    let last_block = first_block + n_idx.saturating_sub(1);
    let start = start.max(first_block);
    let end = end.min(last_block);
    if start > end {
        bail!("empty range after clamp to log [{first_block}..{last_block}]");
    }
    let threads = threads.max(1);
    let total = (end - start + 1) as u64;
    let chunk = total.div_ceil(threads as u64) as u32;
    eprintln!("[disk] log [{first_block}..{last_block}]; scanning [{start}..{end}] ({total} blocks) with {threads} threads");
    let t0 = Instant::now();

    let scanned = Arc::new(AtomicU64::new(0));
    let found = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<String>();

    std::thread::scope(|s| {
        // single writer — streams docs to `out` as they're found
        let found_w = found.clone();
        s.spawn(move || {
            for line in rx {
                let _ = writeln!(out, "{line}");
                found_w.fetch_add(1, Relaxed);
            }
            let _ = out.flush();
        });
        // progress monitor
        let (scanned_m, found_m, done_m) = (scanned.clone(), found.clone(), done.clone());
        s.spawn(move || {
            let (mut last, mut last_t) = (0u64, Instant::now());
            while !done_m.load(Relaxed) {
                std::thread::sleep(Duration::from_secs(3));
                let sc = scanned_m.load(Relaxed);
                let rate = (sc - last) as f64 / last_t.elapsed().as_secs_f64().max(1e-9);
                eprintln!(
                    "[disk] {sc}/{total} ({:.1}%)  {} ABIs  {rate:.0} blk/s",
                    sc as f64 / total as f64 * 100.0,
                    found_m.load(Relaxed)
                );
                (last, last_t) = (sc, Instant::now());
            }
        });
        // workers
        let mut handles = Vec::new();
        for i in 0..threads {
            let cs = start.saturating_add(i.saturating_mul(chunk));
            if cs > end {
                break;
            }
            let ce = ((cs as u64 + chunk as u64 - 1).min(end as u64)) as u32;
            let (lp, ip) = (log_path.clone(), idx_path.clone());
            let (txc, scc) = (tx.clone(), scanned.clone());
            handles.push(s.spawn(move || {
                if let Err(e) = worker_scan(&lp, &ip, first_block, cs, ce, &txc, &scc) {
                    eprintln!("[disk] worker {i} [{cs}..{ce}] FAILED: {e:#}");
                }
            }));
        }
        drop(tx); // so rx closes once all worker tx clones drop
        for h in handles {
            let _ = h.join();
        }
        done.store(true, Relaxed); // stop the monitor; writer ends when rx drains
    });

    let secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "[disk] done: {} blocks in {secs:.1}s ({:.0} blk/s), {} ABI versions",
        scanned.load(Relaxed),
        scanned.load(Relaxed) as f64 / secs.max(1e-9),
        found.load(Relaxed)
    );
    Ok(())
}

/// Decode a serialized abi_def (hex) to its JSON + action/table name lists.
fn decode_abi_def(abieos: &Abieos, abi_hex: &str) -> Result<(String, Vec<String>, Vec<String>)> {
    let abi_bin = hex::decode(abi_hex).context("abi hex decode")?;
    let abi_json = abieos
        .abi_bin_to_json(&abi_bin)
        .map_err(|e| anyhow!("abi_bin_to_json: {e:?}"))?;
    let v: Value = serde_json::from_str(&abi_json)?;
    let names = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok((abi_json, names("actions"), names("tables")))
}

/// Build the abi-index NDJSON doc for a setabi. On success it carries
/// {abi, actions, tables}; on a decode failure (a malformed on-chain ABI) it
/// preserves the raw `abi_hex` and tags the doc with `abi_decode_error` so
/// downstream ingestion can flag it instead of inferring from an empty `abi`.
fn build_abi_doc(abieos: &Abieos, account: &str, block: u32, abi_hex: &str) -> String {
    match decode_abi_def(abieos, abi_hex) {
        Ok((abi, actions, tables)) => serde_json::json!({
            "account": account, "block": block, "abi": abi,
            "abi_hex": abi_hex, "actions": actions, "tables": tables,
        })
        .to_string(),
        Err(e) => serde_json::json!({
            "account": account, "block": block, "abi": "",
            "abi_hex": abi_hex, "actions": [], "tables": [],
            "abi_decode_error": e.to_string(),
        })
        .to_string(),
    }
}

fn build_get_blocks_request(start: u32, end: u32, in_flight: u32, irreversible: bool) -> Vec<u8> {
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

fn build_ack(num: u32) -> Vec<u8> {
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
    config.max_message_size = Some(1_073_741_824);
    config.max_frame_size = Some(1_073_741_824);

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
        let Message::Binary(bin) = msg.with_context(|| format!("[c{id}] stream"))? else {
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
        if processed % 20000 == 0 {
            eprintln!("[c{id}] {processed} blocks ({found} ABIs) at {block_num}");
        }
        if block_num >= end {
            break;
        }
    }
    Ok((processed, found))
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut out: Box<dyn Write + Send> = match &args.out {
        Some(path) => Box::new(BufWriter::new(
            File::create(path).context("create out file")?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout())),
    };

    // Direct-from-disk mode: no nodeos, no SHiP — read the append-only log in parallel.
    if let Some(dir) = &args.from_disk {
        let threads = args.threads.unwrap_or(args.connections);
        return scan_disk(dir, args.start, args.end, threads, &mut out);
    }
    let ship = args
        .ship
        .clone()
        .context("--ship is required (or use --from-disk)")?;

    let conns = args.connections.max(1);
    let total = (args.end - args.start + 1) as u64;
    let per = total.div_ceil(conns as u64);
    eprintln!(
        "[abi-scanner] {} block(s) [{}..{}] over {conns} connection(s) to {}",
        total, args.start, args.end, ship
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
        let c_start = args.start + (i as u64 * per) as u32;
        if c_start > args.end {
            break;
        }
        let c_end = ((c_start as u64 + per - 1) as u32).min(args.end);
        let h = tokio::spawn(scan_range(
            i,
            ship.clone(),
            c_start,
            c_end,
            args.in_flight,
            args.irreversible_only,
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
    fn varuint() {
        assert_eq!(read_varuint(&[0x05]), Some((5, 1)));
        assert_eq!(read_varuint(&[0xd4, 0x10]), Some((2132, 2))); // eosio abi length @ WAX block 2
        assert_eq!(read_varuint(&[0x80, 0x01]), Some((128, 2)));
        assert_eq!(read_varuint(&[]), None);
        assert_eq!(read_varuint(&[0x80]), None); // truncated
    }

    #[test]
    fn ship_magic() {
        assert!(is_ship_magic(0xc35d_5000_0000_0000)); // version 0 (WAX)
        assert!(is_ship_magic(0xc35d_5000_0000_0001)); // same name, version 1
        assert!(!is_ship_magic(0));
    }

    #[test]
    fn account_row_parse() {
        let abieos = Abieos::new();
        // account_v0 row from WAX block 2: [variant 0][name "eosio"][creation_date][abi bytes]
        let mut row = vec![0x00];
        row.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0xea, 0x30, 0x55]); // "eosio"
        row.extend_from_slice(&[0x80, 0xd6, 0x14, 0x49]); // creation_date
        row.extend_from_slice(&[0x02, 0x0e, 0x65]); // abi: varuint len 2, bytes 0e 65
        let (name, abi_hex) = account_setabi(&abieos, &row).unwrap().unwrap();
        assert_eq!(name, "eosio");
        assert_eq!(abi_hex, "0e65");

        // empty abi -> not a setabi
        let mut empty = row[..13].to_vec();
        empty.push(0x00); // abi len 0
        assert!(account_setabi(&abieos, &empty).unwrap().is_none());
    }

    #[test]
    fn payload_both_encodings() {
        use std::io::Write as _;
        let original = b"raw table_delta[] bytes here";
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(original).unwrap();
        let z = enc.finish().unwrap();

        // default: [u32 (ignored)][zlib]
        let mut def = (z.len() as u32).to_le_bytes().to_vec();
        def.extend_from_slice(&z);
        assert_eq!(decode_payload(&def).unwrap(), original);

        // s==1: [u32=1][u64 decompressed_size][zlib]
        let mut s1 = 1u32.to_le_bytes().to_vec();
        s1.extend_from_slice(&(original.len() as u64).to_le_bytes());
        s1.extend_from_slice(&z);
        assert_eq!(decode_payload(&s1).unwrap(), original);

        assert!(decode_payload(&[1, 2]).unwrap().is_empty()); // too short
    }

    #[test]
    fn malformed_abi_is_tagged() {
        // "00" = abi_def with an empty version string -> unsupported -> decode error.
        let doc = build_abi_doc(&Abieos::new(), "badcontract", 100, "00");
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["account"], "badcontract");
        assert_eq!(v["abi_hex"], "00");
        assert_eq!(v["abi"], "");
        assert!(v.get("abi_decode_error").is_some());
    }
}
