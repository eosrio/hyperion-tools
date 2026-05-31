//! es-load — fast parallel NDJSON -> Elasticsearch `_bulk` loader (Rust, no GIL).
//!
//! For LOCAL ES write-ceiling benchmarking + tuning: reads decoded action/delta NDJSON (produced
//! by action-proto / delta-proto) and POSTs it to a LOCAL Elasticsearch with N parallel posters,
//! applying the Hyperion `_id`/`_index` rules. Co-locate this with the ES under test (same box /
//! LAN) so the measured docs/s reflects ES, not the loader or a WAN link. The Python `bulk-load.py`
//! is GIL-bound (~137k docs/s single-process); this saturates ES instead.
//!
//! SAFETY: refuses a non-loopback ES host unless BENCH_ALLOW_EXTERNAL_ES=1 — local benchmarking only.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{bail, Result};
use clap::Parser;
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(about = "Fast parallel NDJSON -> Elasticsearch _bulk loader (local benchmarking).")]
struct Args {
    /// NDJSON file of decoded docs (action or delta).
    #[arg(long)]
    file: String,
    /// Elasticsearch base URL (loopback only unless BENCH_ALLOW_EXTERNAL_ES=1).
    #[arg(long, default_value = "http://localhost:9200")]
    es: String,
    /// "action" (_id = global_sequence) or "delta" (_id = block-code-scope-table-pk).
    #[arg(long, default_value = "action")]
    mode: String,
    #[arg(long, default_value = "wax")]
    chain: String,
    #[arg(long, default_value = "v1")]
    index_version: String,
    #[arg(long, default_value_t = 10_000_000)]
    partition_size: u64,
    /// docs per `_bulk` request.
    #[arg(long, default_value_t = 4000)]
    batch: usize,
    /// concurrent poster threads.
    #[arg(long, default_value_t = 8)]
    workers: usize,
}

fn partition(block: u64, size: u64) -> String {
    format!("{:06}", block.div_ceil(size.max(1)).max(1))
}

/// (`_index`, `_id`) for a doc per the Hyperion rules (mirrors bench/scripts/bulk-load.py).
fn meta(mode: &str, v: &Value, chain: &str, ver: &str, size: u64) -> Option<(String, String)> {
    let block = v.get("block_num")?.as_u64()?;
    let part = partition(block, size);
    if mode == "delta" {
        let g = |k: &str| v.get(k).and_then(Value::as_str);
        let id = format!("{}-{}-{}-{}-{}", block, g("code")?, g("scope")?, g("table")?, g("primary_key")?);
        Some((format!("{chain}-delta-{ver}-{part}"), id))
    } else {
        let gs = v.get("global_sequence")?;
        let id = gs
            .as_u64()
            .map(|n| n.to_string())
            .or_else(|| gs.as_str().map(str::to_string))?;
        Some((format!("{chain}-action-{ver}-{part}"), id))
    }
}

fn guard_local(es: &str) -> Result<()> {
    let host = es.split("://").last().unwrap_or(es).split(['/', ':']).next().unwrap_or("");
    let local = matches!(host, "localhost" | "127.0.0.1" | "::1" | "");
    if !local && std::env::var("BENCH_ALLOW_EXTERNAL_ES").as_deref() != Ok("1") {
        bail!("REFUSING: ES host '{host}' is not loopback — local benchmarking only (set BENCH_ALLOW_EXTERNAL_ES=1 to override).");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    guard_local(&args.es)?;
    let url = format!("{}/_bulk", args.es.trim_end_matches('/'));
    let reader = Arc::new(Mutex::new(BufReader::with_capacity(8 << 20, File::open(&args.file)?)));
    let (docs, bytes, reqs, errors) = (
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicU64::new(0)),
    );
    eprintln!(
        "[es-load] {} -> {} | mode={} index={}-{}-{}-<part> | batch={} workers={}",
        args.file, url, args.mode, args.chain, args.mode, args.index_version, args.batch, args.workers
    );
    let t0 = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..args.workers.max(1) {
            let (reader, docs, bytes, reqs, errors, url, args) =
                (reader.clone(), docs.clone(), bytes.clone(), reqs.clone(), errors.clone(), url.clone(), &args);
            s.spawn(move || {
                let mut body = String::with_capacity(args.batch * 256);
                loop {
                    // Drain a batch of raw lines under the lock (fast in-memory reads), parse+post outside it.
                    let mut lines: Vec<String> = Vec::with_capacity(args.batch);
                    {
                        let mut r = reader.lock().unwrap();
                        let mut buf = String::new();
                        while lines.len() < args.batch {
                            buf.clear();
                            match r.read_line(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(_) => {
                                    if !buf.trim().is_empty() {
                                        lines.push(std::mem::take(&mut buf));
                                    }
                                }
                            }
                        }
                    }
                    if lines.is_empty() {
                        break;
                    }
                    body.clear();
                    let mut n = 0u64;
                    for line in &lines {
                        let line = line.trim_end();
                        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
                        let Some((idx, id)) = meta(&args.mode, &v, &args.chain, &args.index_version, args.partition_size) else {
                            continue;
                        };
                        body.push_str("{\"index\":{\"_index\":\"");
                        body.push_str(&idx);
                        body.push_str("\",\"_id\":\"");
                        body.push_str(&id);
                        body.push_str("\"}}\n");
                        body.push_str(line);
                        body.push('\n');
                        n += 1;
                    }
                    if n == 0 {
                        continue;
                    }
                    bytes.fetch_add(body.len() as u64, Relaxed);
                    let ok = match minreq::post(&url)
                        .with_header("Content-Type", "application/x-ndjson")
                        .with_body(body.as_str())
                        .send()
                    {
                        Ok(resp) => {
                            (200..300).contains(&resp.status_code)
                                && resp.as_str().map(|s| !s.contains("\"errors\":true")).unwrap_or(false)
                        }
                        Err(_) => false,
                    };
                    if !ok {
                        errors.fetch_add(1, Relaxed);
                    }
                    docs.fetch_add(n, Relaxed);
                    reqs.fetch_add(1, Relaxed);
                }
            });
        }
    });
    let dt = t0.elapsed().as_secs_f64().max(1e-9);
    let (d, mb) = (docs.load(Relaxed), bytes.load(Relaxed) as f64 / 1e6);
    eprintln!(
        "[es-load] {d} docs in {dt:.1}s -> {:.0} docs/s | {mb:.1} MB ({:.1} MB/s) | {} bulk reqs | {} workers | errors={}",
        d as f64 / dt,
        mb / dt,
        reqs.load(Relaxed),
        args.workers,
        errors.load(Relaxed),
    );
    Ok(())
}
