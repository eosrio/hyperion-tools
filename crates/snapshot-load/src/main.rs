//! snapshot-load — decode active contract-table state directly from an Antelope portable snapshot
//! (`.bin` / `.bin.zst`) and emit Hyperion-shaped NDJSON. No nodeos, no SHiP replay; deterministic
//! point-in-time state. Handles chain snapshot v6 (commingled `contract_tables`) and v8 (split
//! per-table sections). First targets: `eosio` `voters` and every token contract's `accounts` table.
//!
//! Pipeline: a single producer thread scans the file sequentially (framing is length-prefixed, so
//! the scan can't be parallelised) and pushes owned rows onto a bounded channel; N decode workers
//! each own an `AbiHandle` registry (`Send`, not `Sync`) and decode in parallel; one writer drains
//! NDJSON. Bounded channels provide backpressure → bounded memory for EOS-scale (18 GB+) snapshots.

mod atomicassets;
mod atomicmarket;
mod keyfmt;
mod map;
mod model;
mod mongo;
mod perms;
mod reader;
mod tables;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rs_abieos::Abieos;

use model::{
    format_line, load_abis, AbiRegistry, Dec, Filter, ProducerStats, RawRow, Targets, WorkerStats,
};
use reader::{
    enumerate_sections, find, Section, Snap, SnapRead, StreamSnap, FILE_FORMAT_VERSION, MAGIC,
};

#[derive(Parser, Debug)]
#[command(
    about = "decode contract-table state (voters, token balances) directly from a portable snapshot .bin[.zst]"
)]
struct Args {
    /// portable snapshot `.bin` or `.bin.zst` (local file; seek-from-file path)
    #[arg(long)]
    snapshot: Option<String>,
    /// stream + decode directly from this URL instead of a local file
    /// (`.tar.gz` | `.tgz` | `.bin.zst` | `.zst` | `.bin`); overlaps download/decompress/decode
    #[arg(long, conflicts_with = "snapshot")]
    snapshot_url: Option<String>,
    /// while streaming (`--snapshot-url`), also write the raw decompressed `.bin` to this path
    /// (e.g. for nodeos). Forces a full read to EOF (ignores `--limit`) so the file is complete.
    #[arg(long)]
    tee: Option<String>,
    /// output NDJSON (omit -> stdout)
    #[arg(long)]
    out: Option<String>,
    /// comma-separated table selectors: `voters` | `accounts` | `*` | `table` | `code:table` | `code:scope:table`
    #[arg(long, default_value = "voters,accounts")]
    tables: String,
    /// override head block_num (else derived from the snapshot filename, or a streamed tar's inner
    /// `snapshot-<block_id>.bin` entry name)
    #[arg(long)]
    block_num: Option<u32>,
    /// stop after emitting N docs (smoke test; skips the full-consumption invariant)
    #[arg(long)]
    limit: Option<u64>,
    /// walk + decode everything but write no docs — validates invariants + ABI coverage fast
    #[arg(long, default_value_t = false)]
    stats_only: bool,
    /// emit the raw value as hex instead of ABI-decoding it (byte-level diff vs spring-util to-json)
    #[arg(long, default_value_t = false)]
    raw: bool,
    /// just dump the section list (name, rows, bytes) + chain version, then exit
    #[arg(long, default_value_t = false)]
    inspect: bool,
    /// decode worker threads
    #[arg(long, default_value_t = 8)]
    threads: usize,

    // ── MongoDB sink (additive; NDJSON stays the default when --mongo is absent) ──
    /// MongoDB URI (mongodb://[user:pass@]host:port). If set, write to Mongo instead of NDJSON.
    #[arg(long)]
    mongo: Option<String>,
    /// chain name (db = <prefix>_<chain>); required with --mongo
    #[arg(long)]
    chain: Option<String>,
    /// database_prefix (db name = <prefix>_<chain>)
    #[arg(long, default_value = "hyperion")]
    mongo_prefix: String,
    /// optional auth database (applied via the typed credential source, not appended to the URI)
    #[arg(long)]
    mongo_auth_source: Option<String>,
    /// concurrent insert_many writer tasks (in-flight futures)
    #[arg(long, default_value_t = 8)]
    mongo_writers: usize,
    /// docs per insert_many batch
    #[arg(long, default_value_t = 4000)]
    mongo_batch: usize,
    /// max connection pool size (default = writers + 2)
    #[arg(long)]
    mongo_pool: Option<u32>,
    /// drop target collections before load (idempotent re-runs)
    #[arg(long, default_value_t = false)]
    mongo_drop: bool,
    /// skip building indexes after load (for pure decode+write benchmarking)
    #[arg(long, default_value_t = false)]
    mongo_no_index: bool,
    /// build only the Light-API read-path indexes (skips the costly permissions/pub_keys indexes the
    /// serving path never uses) — strongly recommended for large chains like WAX
    #[arg(long, default_value_t = false)]
    mongo_lean_index: bool,

    /// Build a WormDB Light-API segment (.wseg) at this path directly from the snapshot — NO Mongo.
    /// Use with `--tables lightapi` on the seek path (a local `.bin`); `--chain` sets the name.
    #[arg(long)]
    wseg: Option<String>,
}

/// If `path` ends in `.zst`, decompress (pure-Rust ruzstd) to the same path without the suffix
/// (reused if present) and return that; else return `path`. The reader needs a seekable file.
fn ensure_decompressed(path: &str) -> Result<String> {
    let Some(out_path) = path.strip_suffix(".zst") else {
        return Ok(path.to_string());
    };
    if !std::path::Path::new(out_path).exists() {
        eprintln!("[snapshot-load] decompressing {path} -> {out_path}");
        // Decompress to a sibling `.tmp` then atomically rename into place, so an interrupted run
        // never leaves a half-written `<out>.bin` that a later run's `exists()` check would trust.
        let tmp_path = format!("{out_path}.tmp");
        let inf = BufReader::new(File::open(path).with_context(|| format!("open {path}"))?);
        let mut dec =
            ruzstd::StreamingDecoder::new(inf).map_err(|e| anyhow!("zstd init: {e:?}"))?;
        let mut outf =
            BufWriter::new(File::create(&tmp_path).with_context(|| format!("create {tmp_path}"))?);
        std::io::copy(&mut dec, &mut outf)?;
        outf.flush()?;
        drop(outf); // close before rename (Windows: cannot rename an open file)
        std::fs::rename(&tmp_path, out_path)
            .with_context(|| format!("rename {tmp_path} -> {out_path}"))?;
    }
    Ok(out_path.to_string())
}

/// Derive head block_num from the filename: EOSUSA `snapshot-<64-hex block_id>.bin` (first 4 bytes
/// of the block_id, big-endian, are the height) or EOS Nation `snapshot-...-<decimal>.bin[.zst]`.
fn block_num_from_filename(path: &str) -> Option<u32> {
    let file = std::path::Path::new(path).file_name()?.to_str()?;
    let stem = file
        .strip_suffix(".bin.zst")
        .or_else(|| file.strip_suffix(".bin"))?;
    if let Some(rest) = stem.strip_prefix("snapshot-") {
        if rest.len() >= 64 && rest[..64].bytes().all(|b| b.is_ascii_hexdigit()) {
            if let Ok(h) = u32::from_str_radix(&rest[..8], 16) {
                return Some(h);
            }
        }
    }
    let digits: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse::<u32>().ok()
}

/// Worker output sink: NDJSON lines (default) or typed Mongo docs.
enum OutChan {
    Ndjson(crossbeam_channel::Sender<String>),
    Mongo(crossbeam_channel::Sender<mongo::SinkItem>),
}

/// Spawn N decode workers + the chosen sink (NDJSON writer thread or the parallel Mongo bridge),
/// run `produce` (the single sequential producer) on the current thread feeding the row channel,
/// then join. Producer-agnostic: the seek path passes a closure that walks via `Snap`; the stream
/// path passes one that walks via `StreamSnap`. The workers + sink are byte-identical between the
/// two — only the producer (file seek vs forward stream) differs.
#[allow(clippy::too_many_arguments)]
fn run_workers_and_sink<P>(
    abi_raw: HashMap<u64, Vec<u8>>,
    // The AtomicAssets schema-format registry (empty for non-`atomic` runs), shared read-only across
    // workers so `map_row` can decode `serialized_data` blobs.
    schema_reg: Arc<atomicassets::SchemaRegistry>,
    block_num: u32,
    threads: usize,
    out_path: Option<String>,
    stats_only: bool,
    raw: bool,
    mongo_cfg: Option<mongo::MongoCfg>,
    // When set, mapped docs go to this external channel (the .wseg Builder sink) instead of an
    // internal Mongo/NDJSON sink; the caller owns the draining thread.
    items_tx: Option<crossbeam_channel::Sender<mongo::SinkItem>>,
    produce: P,
) -> Result<(ProducerStats, WorkerStats, Option<mongo::MongoStats>)>
where
    // The producer runs on THIS thread (not spawned), so it need not be `Send`; only the workers +
    // sink spawned in the scope below are `Send`. This is what lets the non-`Send` tar::Entry stream
    // (and the forward-only StreamSnap over it) drive the producer.
    P: FnOnce(&mut dyn FnMut(RawRow) -> Result<()>) -> Result<ProducerStats>,
{
    let n_workers = threads.max(1);
    // Emit the additive Light-API doc fields when writing to Mongo (OFF for plain NDJSON → existing
    // byte-exact lines unchanged). Computed before `mongo_cfg` is moved into the sink match below.
    let lightapi_fields = mongo_cfg.is_some() || items_tx.is_some();
    let abi_raw = Arc::new(abi_raw);
    let (row_tx, row_rx) = crossbeam_channel::bounded::<RawRow>(n_workers * 8192);

    // Build the chosen sink channel + the thread that drains it.
    let (line_tx, line_rx) = crossbeam_channel::bounded::<String>(n_workers * 8192);
    let (mongo_tx, mongo_rx) = crossbeam_channel::bounded::<mongo::SinkItem>(n_workers * 8192);
    let out_chan = if let Some(tx) = &items_tx {
        OutChan::Mongo(tx.clone()) // route mapped docs to the external .wseg Builder sink
    } else if mongo_cfg.is_some() {
        OutChan::Mongo(mongo_tx.clone())
    } else {
        OutChan::Ndjson(line_tx.clone())
    };
    drop(line_tx);
    drop(mongo_tx);

    std::thread::scope(
        |scope| -> Result<(ProducerStats, WorkerStats, Option<mongo::MongoStats>)> {
            // sink thread: NDJSON writer OR Mongo bridge (parallel writers). Exactly one branch runs, so
            // each receiver is consumed in exactly one place.
            let (writer, mongo_handle) = if items_tx.is_some() {
                // The external .wseg Builder drains the items channel — no internal sink here.
                drop(line_rx);
                drop(mongo_rx);
                (None, None)
            } else {
                match mongo_cfg {
                    Some(cfg) => {
                        drop(line_rx); // unused in mongo mode
                        let h = scope.spawn(move || -> Result<mongo::MongoStats> {
                            mongo::run_sink(cfg, mongo_rx)
                        });
                        (None, Some(h))
                    }
                    None => {
                        drop(mongo_rx); // unused in NDJSON mode
                        let w = scope.spawn(move || -> Result<()> {
                            let mut out: Box<dyn Write> = match out_path {
                                Some(p) => Box::new(BufWriter::with_capacity(
                                    1 << 20,
                                    File::create(&p).with_context(|| format!("create {p}"))?,
                                )),
                                None => Box::new(BufWriter::new(std::io::stdout())),
                            };
                            for line in line_rx.iter() {
                                out.write_all(line.as_bytes())?;
                            }
                            out.flush()?;
                            Ok(())
                        });
                        (Some(w), None)
                    }
                }
            };

            // decode workers: each owns its own ABI registry + Abieos (for name formatting)
            let out_chan = Arc::new(out_chan);
            let mut workers = Vec::with_capacity(n_workers);
            for _ in 0..n_workers {
                let row_rx = row_rx.clone();
                let abi_raw = Arc::clone(&abi_raw);
                let schema_reg = Arc::clone(&schema_reg);
                let out_chan = Arc::clone(&out_chan);
                workers.push(scope.spawn(move || -> WorkerStats {
                    let names = Abieos::new();
                    let mut reg = AbiRegistry::new(abi_raw);
                    let mut ws = WorkerStats::default();
                    let mut decoded = String::new();
                    let n = |v: u64| names.name_to_string(v).unwrap_or_else(|_| v.to_string());
                    for row in row_rx.iter() {
                        // Decode the row's value (unless --raw NDJSON), tally, then build the mapped doc.
                        if raw {
                            // raw NDJSON: emit hex value, no decode (preserves the byte-diff path).
                            if let OutChan::Ndjson(tx) = &*out_chan {
                                if !stats_only {
                                    let _ = tx.send(format_line(
                                        &names,
                                        &row,
                                        block_num,
                                        &Dec::Err,
                                        "",
                                    ));
                                }
                            }
                            continue;
                        }
                        let dec = reg.decode(row.code, row.table, &row.value, &mut decoded);
                        ws.tally(&dec);
                        if stats_only {
                            continue;
                        }
                        // Map to the exact Hyperion doc when the row decoded; unmappable rows fall back to
                        // the generic hex `value` line (NDJSON only) and are skipped entirely for Mongo.
                        let mapped = if matches!(dec, Dec::Ok) {
                            serde_json::from_str::<serde_json::Value>(&decoded)
                                .ok()
                                .and_then(|data| {
                                    let (code, scope_s, table, payer) =
                                        (n(row.code), n(row.scope), n(row.table), n(row.payer));
                                    // Only `accounts` rows need the (cached) token-contract validation.
                                    let token_ok =
                                        table == "accounts" && reg.is_token_contract(row.code);
                                    let ctx = map::RowCtx {
                                        code: &code,
                                        scope: &scope_s,
                                        table: &table,
                                        primary_key: row.pk.to_string(),
                                        payer: &payer,
                                        block_num,
                                        token_ok,
                                        lightapi_fields,
                                    };
                                    map::map_row(&ctx, data, &mut reg, &schema_reg)
                                })
                        } else {
                            None
                        };
                        match &*out_chan {
                            OutChan::Ndjson(tx) => {
                                let line = match mapped {
                                    // approvals2 carrier docs are a Mongo-join artifact — never emit to NDJSON
                                    Some((coll, ref doc))
                                        if coll == map::COLL_PROPOSALS
                                            && doc
                                                .get("__approval")
                                                .and_then(serde_json::Value::as_bool)
                                                .unwrap_or(false) =>
                                    {
                                        continue;
                                    }
                                    Some((_coll, doc)) => {
                                        let mut s = doc.to_string();
                                        s.push('\n');
                                        s
                                    }
                                    None => format_line(&names, &row, block_num, &dec, &decoded),
                                };
                                let _ = tx.send(line);
                            }
                            OutChan::Mongo(tx) => {
                                // Encode JSON -> BSON HERE (in the parallel worker), so the accumulator just
                                // batches and the parallel writers actually saturate the sink.
                                if let Some((coll, value)) = mapped {
                                    if let Ok(bdoc) = mongodb::bson::to_document(&value) {
                                        let _ = tx.send((coll, bdoc));
                                    }
                                }
                            }
                        }
                    }
                    ws.abis_parsed = reg.abis_parsed();
                    ws
                }));
            }
            drop(row_rx); // workers hold their clones
            drop(out_chan); // workers hold their Arc clones

            // producer (this thread): walk the framing (file seek or forward stream), send selected rows
            let pstats = {
                let mut sink = |row: RawRow| -> Result<()> {
                    row_tx.send(row).map_err(|_| anyhow!("row channel closed"))
                };
                produce(&mut sink)?
            };
            drop(row_tx); // close -> workers drain and exit

            let mut ws = WorkerStats::default();
            for w in workers {
                ws.merge(w.join().map_err(|_| anyhow!("decode worker panicked"))?);
            }
            if let Some(w) = writer {
                w.join().map_err(|_| anyhow!("writer panicked"))??;
            }
            let mongo_stats = match mongo_handle {
                Some(h) => Some(h.join().map_err(|_| anyhow!("mongo sink panicked"))??),
                None => None,
            };
            Ok((pstats, ws, mongo_stats))
        },
    )
}

/// Seek-from-file pipeline (unchanged behavior): enumerate-then-seek producer over `Snap`, driving
/// the shared worker+sink machinery. The producer closure is the original code moved verbatim, so
/// this path's output is byte-identical to before the streaming refactor.
#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    mut s: Snap,
    secs: &[Section],
    chain_version: u32,
    abi_raw: HashMap<u64, Vec<u8>>,
    schema_reg: Arc<atomicassets::SchemaRegistry>,
    t: &Targets,
    block_num: u32,
    threads: usize,
    out_path: Option<String>,
    limit: Option<u64>,
    stats_only: bool,
    raw: bool,
    mongo_cfg: Option<mongo::MongoCfg>,
    items_tx: Option<crossbeam_channel::Sender<mongo::SinkItem>>,
) -> Result<(ProducerStats, WorkerStats, Option<mongo::MongoStats>)> {
    run_workers_and_sink(
        abi_raw,
        schema_reg,
        block_num,
        threads,
        out_path,
        stats_only,
        raw,
        mongo_cfg,
        items_tx,
        |sink| {
            if chain_version < 7 {
                let ct = find(secs, "contract_tables")
                    .ok_or_else(|| anyhow!("no contract_tables section (v6)"))?;
                tables::walk_v6(&mut s, ct, t, limit, sink)
            } else {
                let tid = find(secs, "eosio::chain::table_id_object")
                    .ok_or_else(|| anyhow!("no table_id_object section (v8)"))?;
                let kv = find(secs, "eosio::chain::key_value_object")
                    .ok_or_else(|| anyhow!("no key_value_object section (v8)"))?;
                let interesting = tables::load_table_ids_v8(&mut s, tid, t)?;
                eprintln!(
                    "[snapshot-load] v8: {} target tables (of {} total)",
                    interesting.len(),
                    tid.rows
                );
                tables::walk_v8(&mut s, kv, &interesting, limit, sink)
            }
        },
    )
}

// ──────────────────────────────────────────────────────────────────────────────
// Streaming HTTP-direct input (additive; the seek-from-file path above is untouched)
// ──────────────────────────────────────────────────────────────────────────────

/// `Read` adapter that mirrors every byte read through it into `sink` (the local `.bin` file), so
/// nodeos can be handed the raw snapshot while indexing consumes the same bytes. On EOF the sink is
/// flushed so the on-disk `.bin` is complete.
struct TeeReader<R: Read> {
    inner: R,
    sink: BufWriter<File>,
}
impl<R: Read> TeeReader<R> {
    fn new(inner: R, path: &str) -> Result<Self> {
        Ok(Self {
            inner,
            sink: BufWriter::with_capacity(
                1 << 20,
                File::create(path).with_context(|| format!("create tee {path}"))?,
            ),
        })
    }
}
impl<R: Read> Read for TeeReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.sink.write_all(&buf[..n])?; // mirror exactly what we hand upstream
        } else {
            self.sink.flush()?; // EOF — make the .bin complete on disk
        }
        Ok(n)
    }
}

/// Open a streaming, on-the-fly-decompressed reader over the *raw snapshot `.bin` bytes* at `url`.
/// Detects kind by URL suffix: `.tar.gz`/`.tgz` (gunzip + untar the single `snapshot-*.bin`),
/// `.bin.zst`/`.zst` (streaming zstd), or bare `.bin`. The returned reader is forward-only (NOT
/// seekable). For the `.tar.gz` leg the `tar::Archive` is leaked — acceptable for a one-shot CLI.
/// Returns the inner snapshot filename for tar inputs (`snapshot-<block_id>.bin`) so the caller can
/// derive block_num from it when the URL basename can't (e.g. a `latest.tar.gz` "latest" pointer).
fn open_url_stream(url: &str) -> Result<(Box<dyn Read>, Option<String>)> {
    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    // ureq returns 4xx/5xx as Err already; into_reader() is the incremental streaming body.
    let body: Box<dyn Read + Send + Sync + 'static> = resp.into_reader();
    let body = BufReader::with_capacity(1 << 20, body); // 1 MiB net buffer

    let u = url.split(['?', '#']).next().unwrap_or(url);
    if u.ends_with(".tar.gz") || u.ends_with(".tgz") {
        let gz = flate2::read::GzDecoder::new(body);
        // Keep the Archive alive for the entry's lifetime: leak it (process is short-lived).
        let archive: &'static mut tar::Archive<
            flate2::read::GzDecoder<BufReader<Box<dyn Read + Send + Sync + 'static>>>,
        > = Box::leak(Box::new(tar::Archive::new(gz)));
        for entry in archive.entries().context("tar entries")? {
            let entry = entry.context("tar entry")?;
            let path = entry.path().context("tar path")?;
            let name = path.to_string_lossy().to_string();
            // The single inner snapshot file: snapshot-*.bin (skip dirs, MANIFEST, etc.)
            if name.ends_with(".bin")
                && name
                    .rsplit('/')
                    .next()
                    .unwrap_or("")
                    .starts_with("snapshot")
            {
                return Ok((Box::new(entry), Some(name))); // tar::Entry: Read, streams forward from here
            }
        }
        bail!("no snapshot-*.bin entry found in tar stream {url}");
    } else if u.ends_with(".bin.zst") || u.ends_with(".zst") {
        let dec = ruzstd::StreamingDecoder::new(body).map_err(|e| anyhow!("zstd init: {e:?}"))?;
        Ok((Box::new(dec), None))
    } else if u.ends_with(".bin") {
        Ok((Box::new(body), None))
    } else {
        bail!(
            "unknown snapshot extension in URL (expected .tar.gz/.tgz, .bin.zst/.zst, .bin): {url}"
        );
    }
}

/// Forward streaming driver for contract tables: ONE forward pass over the snapshot, discovering
/// sections inline and dispatching per name. ABIs (`account_object`) and v8 table_ids
/// (`table_id_object`) precede the row section in writer order, so they are fully collected before
/// the row section is reached; the row section is then streamed straight into the live worker
/// pipeline so download/decompress/decode all overlap. Reuses the exact workers + sink as the seek
/// path; the walkers' exact-consumption tripwire still fires (end = payload_off + payload_len).
#[allow(clippy::too_many_arguments)]
fn run_stream<R: Read>(
    mut s: StreamSnap<R>,
    t: &Targets,
    block_num: u32,
    threads: usize,
    out_path: Option<String>,
    limit: Option<u64>,
    stats_only: bool,
    raw: bool,
    mongo_cfg: Option<mongo::MongoCfg>,
    tee_active: bool,
) -> Result<(ProducerStats, WorkerStats, Option<mongo::MongoStats>)> {
    // header: magic + file_format_version (mirror enumerate_sections)
    let magic = s.u32()?;
    if magic != MAGIC {
        bail!("bad magic 0x{magic:08x} (expected 0x{MAGIC:08x}) — not a portable snapshot");
    }
    let fv = s.u32()?;
    if fv != FILE_FORMAT_VERSION {
        bail!("unsupported snapshot file-format version {fv} (expected {FILE_FORMAT_VERSION})");
    }

    let mut abi_raw: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut interesting: HashMap<u64, (u64, u64, u64)> = HashMap::new();
    let mut chain_version: Option<u32> = None;

    // Forward-scan sections, accumulating abi_raw / interesting / chain_version, STOPPING when the
    // next frame is the row section (contract_tables v6 or key_value_object v8). At that point the
    // frame header is consumed and we are positioned at the row payload start.
    loop {
        let size = s.u64()?;
        if size == u64::MAX {
            bail!("hit end-marker before any row section (no contract_tables/key_value_object)");
        }
        // EARLY guard (forward stream): `size` must cover at least row_count(8) BEFORE we read 8 bytes
        // of `rows`; on `size < 8` those 8 bytes would come from the next frame, desyncing the scan.
        if size < 8 {
            bail!("malformed section framing: size {size} < 8 (cannot hold row_count)");
        }
        let after_size = s.pos();
        let rows = s.u64()?;
        let name = s.cstr()?;
        let name_bytes = name.len() as u64 + 1; // + NUL
        if size < 8 + name_bytes {
            bail!("malformed section framing: size {size} < header for '{name}'");
        }
        let payload_off = after_size + 8 + name_bytes;
        let payload_len = size - 8 - name_bytes;
        let sec = Section {
            name: name.clone(),
            payload_off,
            rows,
            payload_len,
        };

        match name.as_str() {
            "eosio::chain::chain_snapshot_header" => {
                let v = s.u32()?;
                match v {
                    2..=6 | 8 => {}
                    7 => bail!("chain snapshot version 7 is unsupported (transient Spring 1.0.0)"),
                    other => bail!("unsupported chain snapshot version {other}"),
                }
                chain_version = Some(v);
                // skip the remainder of the header payload
                s.seek_to(payload_off + payload_len)?;
            }
            "eosio::chain::account_object" => {
                abi_raw = load_abis(&mut s, &sec)?;
                eprintln!(
                    "[snapshot-load] {} contract ABIs from {} accounts",
                    abi_raw.len(),
                    rows
                );
            }
            "eosio::chain::table_id_object" => {
                interesting = tables::load_table_ids_v8(&mut s, &sec, t)?;
                eprintln!(
                    "[snapshot-load] v8: {} target tables (of {} total)",
                    interesting.len(),
                    rows
                );
            }
            "contract_tables" | "eosio::chain::key_value_object" => {
                // The row section — stream it into the live pipeline. We are at payload_off, so the
                // walker's entry seek_to(payload_off) degenerates to a no-op forward skip.
                let cv = chain_version
                    .ok_or_else(|| anyhow!("row section before chain_snapshot_header"))?;
                eprintln!(
                    "[snapshot-load] streaming row section '{}' ({} rows, {} bytes) v{cv}",
                    name, rows, payload_len
                );
                return run_workers_and_sink(
                    abi_raw,
                    Arc::new(atomicassets::SchemaRegistry::default()), // atomic preset is seek-only
                    block_num,
                    threads,
                    out_path,
                    stats_only,
                    raw,
                    mongo_cfg,
                    None, // --wseg uses the seek path; streaming has no external Builder sink
                    move |sink| {
                        let ps = if name == "contract_tables" {
                            tables::walk_v6(&mut s, &sec, t, limit, sink)?
                        } else {
                            tables::walk_v8(&mut s, &sec, &interesting, limit, sink)?
                        };
                        // With --tee, the on-disk .bin must mirror EVERY source byte. The walk stops
                        // at the row section's end (and `--limit` stops it even earlier), so drain the
                        // remainder of the stream to EOF — this is also what triggers the TeeReader's
                        // final EOF flush, making the teed file byte-identical to the source.
                        if tee_active {
                            let drained = s.drain_to_eof()?;
                            eprintln!(
                                "[snapshot-load] --tee: drained {drained} trailing bytes to complete the .bin"
                            );
                        }
                        Ok(ps)
                    },
                );
            }
            _ => {
                s.seek_to(payload_off + payload_len)?; // not interesting — forward-skip the payload
            }
        }
    }
}

/// Forward streaming driver for permissions. Permissions are native sections; the link map must be
/// built before rendering each permission's `linked_actions`, but in writer order
/// `permission_object` precedes `permission_link_object`. On a forward stream we buffer the
/// permission_object payload until the link section arrives, then call the shared
/// `decode_permissions_bufs` (same body as the seek path) with both buffers in hand.
#[allow(clippy::too_many_arguments)]
fn run_stream_permissions<R: Read>(
    mut s: StreamSnap<R>,
    names: &Abieos,
    eosio: u64,
    block_num: u32,
    out: &mut dyn Write,
    limit: Option<u64>,
    stats_only: bool,
    tee_active: bool,
) -> Result<perms::PermStats> {
    let magic = s.u32()?;
    if magic != MAGIC {
        bail!("bad magic 0x{magic:08x} (expected 0x{MAGIC:08x}) — not a portable snapshot");
    }
    let fv = s.u32()?;
    if fv != FILE_FORMAT_VERSION {
        bail!("unsupported snapshot file-format version {fv} (expected {FILE_FORMAT_VERSION})");
    }

    let mut abi_raw: HashMap<u64, Vec<u8>> = HashMap::new();
    let mut perm_buf: Option<(Vec<u8>, u64)> = None; // (payload bytes, rows)

    loop {
        let size = s.u64()?;
        if size == u64::MAX {
            bail!("hit end-marker before permission_link_object");
        }
        // EARLY guard (forward stream): `size` must cover at least row_count(8) BEFORE we read 8 bytes
        // of `rows`; on `size < 8` those 8 bytes would come from the next frame, desyncing the scan.
        if size < 8 {
            bail!("malformed section framing: size {size} < 8 (cannot hold row_count)");
        }
        let after_size = s.pos();
        let rows = s.u64()?;
        let name = s.cstr()?;
        let name_bytes = name.len() as u64 + 1;
        if size < 8 + name_bytes {
            bail!("malformed section framing: size {size} < header for '{name}'");
        }
        let payload_off = after_size + 8 + name_bytes;
        let payload_len = size - 8 - name_bytes;

        match name.as_str() {
            "eosio::chain::account_object" => {
                let sec = Section {
                    name: name.clone(),
                    payload_off,
                    rows,
                    payload_len,
                };
                abi_raw = load_abis(&mut s, &sec)?;
            }
            "eosio::chain::permission_object" => {
                // 64-bit on disk; `try_from` (not `as`) so a >usize::MAX payload bails instead of
                // truncating (a no-op on 64-bit; correct on a 32-bit target).
                let plen = usize::try_from(payload_len).map_err(|_| {
                    anyhow!("permission_object payload_len {payload_len} overflows usize")
                })?;
                let mut buf = Vec::new();
                s.read_into(plen, &mut buf)?;
                perm_buf = Some((buf, rows));
            }
            "eosio::chain::permission_link_object" => {
                let plen = usize::try_from(payload_len).map_err(|_| {
                    anyhow!("permission_link_object payload_len {payload_len} overflows usize")
                })?;
                let mut lbuf = Vec::new();
                s.read_into(plen, &mut lbuf)?;
                let (pbuf, perm_rows) = perm_buf
                    .ok_or_else(|| anyhow!("permission_link_object before permission_object"))?;
                let st = perms::decode_permissions_bufs(
                    &lbuf, rows, &pbuf, perm_rows, &abi_raw, names, eosio, block_num, out, limit,
                    stats_only,
                )?;
                // With --tee, mirror the rest of the source bytes to disk (and trigger the EOF flush).
                if tee_active {
                    let drained = s.drain_to_eof()?;
                    eprintln!(
                        "[snapshot-load] --tee: drained {drained} trailing bytes to complete the .bin"
                    );
                }
                return Ok(st);
            }
            _ => {
                s.seek_to(payload_off + payload_len)?;
            }
        }
    }
}

/// Build the forward-only `StreamSnap` over the URL's decompressed body, optionally tee'd to disk.
/// `block_num` is required up front (computed by the caller) because the streamed body's URL may not
/// carry a parseable height, and a forward stream cannot be re-opened to look.
/// Buffered forward-only streaming reader over the raw snapshot `.bin` bytes (HTTP body → optional
/// gunzip+untar or zstd → optional tee), wrapped in `StreamSnap`.
type SnapStream = StreamSnap<BufReader<Box<dyn Read>>>;

fn open_stream_snap(url: &str, tee: Option<&str>) -> Result<(SnapStream, Option<String>)> {
    let (raw, inner_name) = open_url_stream(url)?;
    let raw: Box<dyn Read> = match tee {
        Some(p) => Box::new(TeeReader::new(raw, p)?),
        None => raw,
    };
    Ok((
        StreamSnap::new(BufReader::with_capacity(1 << 20, raw)),
        inner_name,
    ))
}

/// Parse the `--tables` contract-table selectors into resolved `Targets` (shared by both paths).
fn build_targets(specs: &[&str], names: &Abieos) -> Result<Targets> {
    let nm = |x: &str| {
        names
            .string_to_name(x)
            .map_err(|e| anyhow!("string_to_name({x}): {e:?}"))
    };
    let mut filters = Vec::new();
    for spec in specs.iter().copied() {
        let f = match spec {
            "*" | "all" => Filter::All,
            "voters" => Filter::CodeScopeTable(nm("eosio")?, nm("eosio")?, nm("voters")?),
            "accounts" => Filter::Table(nm("accounts")?),
            sp => match sp.split(':').collect::<Vec<_>>().as_slice() {
                [table] => Filter::Table(nm(table)?),
                [code, table] => Filter::CodeTable(nm(code)?, nm(table)?),
                [code, scope, table] => Filter::CodeScopeTable(nm(code)?, nm(scope)?, nm(table)?),
                _ => bail!("bad --tables spec '{sp}' (use: table | code:table | code:scope:table | voters | accounts | *)"),
            },
        };
        filters.push(f);
    }
    if filters.is_empty() {
        bail!("--tables selected nothing");
    }
    Ok(Targets { filters })
}

/// Contract tables the `lightapi` preset loads (besides the native `permissions` pass). These feed
/// the cc32d9 resources/rex/topram/topstake/balances endpoints. Missing collections on a given chain
/// (e.g. no REX on some chains) simply yield empty sections — tolerated downstream.
const LIGHTAPI_TABLES: &[&str] = &[
    "voters",
    "accounts",
    "eosio:global",
    "eosio:userres",
    "eosio:delband",
    "eosio:rexbal",
    "eosio:rexfund",
    "eosio:rexpool",
];

/// Parse the `--tables` selector list, expanding the `lightapi` and `atomicassets`/`atomicmarket`/
/// `atomic` presets. Returns `(table_specs, want_permissions, lightapi_preset, atomic_preset)` where
/// `table_specs` is the contract-table selectors with the native `permissions` selector removed and
/// `atomic_preset` flags an AtomicAssets/AtomicMarket run (which needs the schema-registry pre-pass).
fn parse_table_specs(tables: &str) -> (Vec<&str>, bool, bool, bool) {
    let mut specs: Vec<&str> = tables
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .collect();
    let lightapi_preset = specs.contains(&"lightapi");
    if lightapi_preset {
        specs.retain(|s| *s != "lightapi");
        for t in LIGHTAPI_TABLES {
            if !specs.contains(t) {
                specs.push(t);
            }
        }
    }
    // AtomicAssets / AtomicMarket presets: `atomicassets`, `atomicmarket`, or `atomic` (both).
    let want_aa = specs
        .iter()
        .any(|s| matches!(*s, "atomicassets" | "atomic"));
    let want_am = specs
        .iter()
        .any(|s| matches!(*s, "atomicmarket" | "atomic"));
    let atomic_preset = want_aa || want_am;
    if atomic_preset {
        specs.retain(|s| !matches!(*s, "atomic" | "atomicassets" | "atomicmarket"));
        if want_aa {
            for t in atomicassets::ATOMICASSETS_TABLES {
                if !specs.contains(t) {
                    specs.push(t);
                }
            }
        }
        if want_am {
            for t in atomicmarket::ATOMICMARKET_TABLES {
                if !specs.contains(t) {
                    specs.push(t);
                }
            }
        }
    }
    let want_permissions = lightapi_preset || specs.contains(&"permissions");
    let table_specs: Vec<&str> = specs.into_iter().filter(|s| *s != "permissions").collect();
    (
        table_specs,
        want_permissions,
        lightapi_preset,
        atomic_preset,
    )
}

/// Run the native-section pass straight into MongoDB (a self-contained sink), emitting the
/// `permissions` collection, the `pub_keys` reverse index, and `account_codehash` (for `/codehash`).
/// Used by `--tables lightapi`/permissions when `--mongo` is set. Returns the permission stats.
#[allow(clippy::too_many_arguments)]
fn load_permissions_to_mongo(
    s: &mut Snap,
    secs: &[Section],
    abi_raw: &HashMap<u64, Vec<u8>>,
    names: &Abieos,
    eosio: u64,
    block_num: u32,
    cfg: mongo::MongoCfg,
    limit: Option<u64>,
) -> Result<perms::PermStats> {
    let (tx, rx) = crossbeam_channel::bounded::<mongo::SinkItem>(8192);
    let handle = std::thread::spawn(move || mongo::run_sink(cfg, rx));
    let pst =
        perms::decode_permissions_mongo(s, secs, abi_raw, names, eosio, block_num, &tx, limit);

    // account_codehash (account ↔ contract hash) from the account_metadata_object section, if present.
    let mut codehashes = 0u64;
    if let Some(meta_sec) = find(secs, "eosio::chain::account_metadata_object") {
        match model::load_codehashes(s, meta_sec) {
            Ok(rows) => {
                for (name, hash) in rows {
                    let acct = names
                        .name_to_string(name)
                        .unwrap_or_else(|_| name.to_string());
                    let d = mongodb::bson::doc! {
                        "account": acct,
                        "code_hash": hex::encode(hash),
                        "block_num": block_num as i64,
                    };
                    if tx.send((map::COLL_CODEHASH, d)).is_ok() {
                        codehashes += 1;
                    }
                }
            }
            Err(e) => eprintln!("[snapshot-load][mongo] account_codehash skipped: {e}"),
        }
    }

    drop(tx); // close → sink drains and builds indexes
    let ms = handle
        .join()
        .map_err(|_| anyhow!("mongo permissions sink panicked"))??;
    let pst = pst?;
    eprintln!(
        "[snapshot-load][mongo] permissions: {} docs in {:.1}s | errors={} | {} permissions, {} links, {} codehashes",
        ms.docs, ms.write_secs, ms.errors, pst.permissions, pst.links, codehashes
    );
    Ok(pst)
}

/// Permissions native pass that sends `permissions` + `pub_keys` + `account_codehash` docs to an
/// external sink channel (the `.wseg` Builder) — same decode as the Mongo path, no internal sink.
#[allow(clippy::too_many_arguments)]
fn load_permissions_to_sink(
    s: &mut Snap,
    secs: &[Section],
    abi_raw: &HashMap<u64, Vec<u8>>,
    names: &Abieos,
    eosio: u64,
    block_num: u32,
    tx: &crossbeam_channel::Sender<mongo::SinkItem>,
    limit: Option<u64>,
) -> Result<perms::PermStats> {
    let pst =
        perms::decode_permissions_mongo(s, secs, abi_raw, names, eosio, block_num, tx, limit)?;
    if let Some(meta_sec) = find(secs, "eosio::chain::account_metadata_object") {
        match model::load_codehashes(s, meta_sec) {
            Ok(rows) => {
                for (name, hash) in rows {
                    let acct = names
                        .name_to_string(name)
                        .unwrap_or_else(|_| name.to_string());
                    let d = mongodb::bson::doc! {
                        "account": acct,
                        "code_hash": hex::encode(hash),
                        "block_num": block_num as i64,
                    };
                    let _ = tx.send((map::COLL_CODEHASH, d));
                }
            }
            Err(e) => eprintln!("[snapshot-load] account_codehash skipped: {e}"),
        }
    }
    Ok(pst)
}

/// Build the optional Mongo sink config from CLI args (shared by both paths). NDJSON is the default
/// when `--mongo` is absent (returns `None`).
fn build_mongo_cfg(args: &Args) -> Result<Option<mongo::MongoCfg>> {
    let Some(base_uri) = &args.mongo else {
        return Ok(None);
    };
    // The drop set depends on the preset: an `atomic` run owns the AtomicAssets/AtomicMarket
    // collections, NOT voters/accounts/proposals — so `--mongo-drop` doesn't wipe a prior lightapi
    // load's collections (mirrors how `build_perms_mongo_cfg` narrows the set to permissions/pub_keys).
    let atomic_preset = parse_table_specs(&args.tables).3;
    let special_drops = if atomic_preset {
        atomicassets::all_collections()
    } else {
        vec![map::COLL_VOTERS, map::COLL_ACCOUNTS, map::COLL_PROPOSALS]
    };
    let chain = args
        .chain
        .as_deref()
        .ok_or_else(|| anyhow!("--chain is required with --mongo (db = <prefix>_<chain>)"))?;
    if args.raw {
        bail!("--raw and --mongo are mutually exclusive (raw emits hex, not decodable docs)");
    }
    // --mongo-auth-source is applied via the typed credential `source` inside the sink (see
    // mongo::run_writer), NOT string-appended to the URI.
    let uri = base_uri.clone();
    let writers = args.mongo_writers.max(1);
    let db_name = format!("{}_{}", args.mongo_prefix, chain);
    eprintln!(
        "[snapshot-load][mongo] {uri} -> db={db_name} | writers={writers} batch={} pool={} drop={} index={} authSource={}",
        args.mongo_batch,
        args.mongo_pool.unwrap_or(writers as u32 + 2),
        args.mongo_drop,
        !args.mongo_no_index,
        args.mongo_auth_source.as_deref().unwrap_or("<uri-default>"),
    );
    Ok(Some(mongo::MongoCfg {
        uri,
        db_name,
        auth_source: args.mongo_auth_source.clone(),
        writers,
        batch: args.mongo_batch.max(1),
        pool: args.mongo_pool.unwrap_or(writers as u32 + 2),
        drop: args.mongo_drop,
        no_index: args.mongo_no_index,
        lean_index: args.mongo_lean_index,
        special_drops,
    }))
}

/// Mongo config for the permissions pass: same connection, but only drops `permissions`/`pub_keys`
/// (so a single `lightapi` invocation can load contract tables and permissions without one pass
/// wiping the other's collections).
fn build_perms_mongo_cfg(args: &Args) -> Result<Option<mongo::MongoCfg>> {
    let mut cfg = build_mongo_cfg(args)?;
    if let Some(c) = &mut cfg {
        c.special_drops = vec![
            map::COLL_PERMISSIONS,
            map::COLL_PUB_KEYS,
            map::COLL_CODEHASH,
        ];
    }
    Ok(cfg)
}

/// Print the producer/decode/mongo summary block (shared by both paths).
fn report_run(
    args: &Args,
    t0: Instant,
    ps: &ProducerStats,
    ws: &WorkerStats,
    ms: &Option<mongo::MongoStats>,
) {
    eprintln!("[snapshot-load] done in {:.1?}", t0.elapsed());
    eprintln!("[snapshot-load] producer {ps:#?}");
    eprintln!("[snapshot-load] decode   {ws:#?}");
    if let Some(ms) = ms {
        let dps = if ms.write_secs > 0.0 {
            ms.docs as f64 / ms.write_secs
        } else {
            0.0
        };
        let grand = t0.elapsed().as_secs_f64();
        eprintln!(
            "[snapshot-load][mongo] {} docs in {:.1}s -> {:.0} docs/s | {} batches | {} writers | errors={}",
            ms.docs, ms.write_secs, dps, ms.batches, args.mongo_writers, ms.errors
        );
        let mut per = ms.per_coll.clone();
        per.sort();
        for (coll, n) in &per {
            eprintln!("[snapshot-load][mongo]   {coll}: {n} docs");
        }
        eprintln!(
            "[snapshot-load][mongo] indexes built in {:.1}s | grand total {:.1}s",
            ms.index_secs, grand
        );
    }
}

/// Derive head block_num from CLI override or the URL/file basename.
fn resolve_block_num(explicit: Option<u32>, name_for_derive: &str) -> Result<u32> {
    match explicit {
        Some(b) => Ok(b),
        None => block_num_from_filename(name_for_derive)
            .ok_or_else(|| anyhow!("could not derive block_num from name; pass --block-num")),
    }
}

/// Streaming HTTP-direct dispatch (`--snapshot-url`): one forward pass, decode overlaps download.
fn run_url_path(args: &Args) -> Result<()> {
    let url = args.snapshot_url.as_deref().unwrap();
    if args.inspect {
        bail!("--inspect needs random access; it is unavailable in --snapshot-url (stream) mode");
    }
    let names = Abieos::new();
    let nm = |x: &str| {
        names
            .string_to_name(x)
            .map_err(|e| anyhow!("string_to_name({x}): {e:?}"))
    };

    let (table_specs, want_permissions, lightapi_preset, atomic_preset) =
        parse_table_specs(&args.tables);
    // Permissions-to-Mongo (and thus the `lightapi` preset) needs random access to interleave the
    // native permission sections with the contract-table pass; it is seek-path only for now.
    if lightapi_preset {
        bail!("--tables lightapi is supported on the seek path only (--snapshot <file>); download the snapshot then load it");
    }
    // The AtomicAssets/AtomicMarket preset needs a two-pass (schema-format registry) over the
    // contract-table section, which requires random access — seek path only.
    if atomic_preset {
        bail!("--tables atomicassets/atomicmarket/atomic is supported on the seek path only (--snapshot <file>); download the snapshot then load it");
    }
    if want_permissions && args.mongo.is_some() {
        bail!("loading `permissions` into --mongo is supported on the seek path only (--snapshot <file>) for now");
    }
    if want_permissions && !table_specs.is_empty() {
        bail!("`permissions` is a native section, not a contract table — run it alone: --tables permissions");
    }

    let t0 = Instant::now();

    // Build targets / mongo config up front (cheap, validates --tables/--mongo) so a bad arg fails
    // before we open the network stream. `permissions` is a native path needing neither.
    let main_cfg = if want_permissions {
        None
    } else {
        Some((build_targets(&table_specs, &names)?, build_mongo_cfg(args)?))
    };

    eprintln!("[snapshot-load] streaming from {url}");
    if args.tee.is_some() && args.limit.is_some() {
        eprintln!("[snapshot-load] note: --tee forces a full read; --limit only short-circuits the decode walk and would truncate the teed .bin");
    }

    // Open the stream first: a `.tar.gz` yields its inner `snapshot-<block_id>.bin` entry name, which
    // is the authoritative block_num source when the URL basename has none (e.g. `latest.tar.gz`).
    let (s, inner_name) = open_stream_snap(url, args.tee.as_deref())?;
    let url_basename = url.split(['?', '#']).next().unwrap_or(url);
    if let Some(n) = &inner_name {
        eprintln!("[snapshot-load] tar entry {n}");
    }
    let block_num = resolve_block_num(
        args.block_num,
        inner_name.as_deref().unwrap_or(url_basename),
    )?;
    eprintln!("[snapshot-load] head block_num={block_num}");

    if want_permissions {
        let mut out: Box<dyn Write> = match &args.out {
            Some(p) => Box::new(BufWriter::with_capacity(
                1 << 20,
                File::create(p).with_context(|| format!("create {p}"))?,
            )),
            None => Box::new(BufWriter::new(std::io::stdout())),
        };
        let pst = run_stream_permissions(
            s,
            &names,
            nm("eosio")?,
            block_num,
            &mut *out,
            args.limit,
            args.stats_only,
            args.tee.is_some(),
        )?;
        out.flush()?;
        eprintln!("[snapshot-load] done in {:.1?}", t0.elapsed());
        eprintln!("[snapshot-load] permissions {pst:#?}");
        return Ok(());
    }

    let (t, mongo_cfg) = main_cfg.expect("main_cfg built when !want_permissions");
    let (ps, ws, ms) = run_stream(
        s,
        &t,
        block_num,
        args.threads,
        args.out.clone(),
        args.limit,
        args.stats_only,
        args.raw,
        mongo_cfg,
        args.tee.is_some(),
    )?;
    report_run(args, t0, &ps, &ws, &ms);
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Dispatch: streaming HTTP-direct path (additive) vs the seek-from-file path (default reference).
    match (&args.snapshot_url, &args.snapshot) {
        (Some(_), _) => return run_url_path(&args),
        (None, Some(_)) => {} // fall through to the file path below
        (None, None) => bail!("provide exactly one of --snapshot <file> or --snapshot-url <url>"),
    }

    let names = Abieos::new(); // name<->u64 only; no ABI loaded
    let nm = |x: &str| {
        names
            .string_to_name(x)
            .map_err(|e| anyhow!("string_to_name({x}): {e:?}"))
    };

    // --tables selectors, expanding the `lightapi` + `atomic` presets. `permissions` is a native
    // section (not a contract table); `table_specs` excludes it and it is loaded by a dedicated pass.
    let (table_specs, want_permissions, lightapi_preset, atomic_preset) =
        parse_table_specs(&args.tables);

    let t0 = Instant::now();
    let snap_path = ensure_decompressed(args.snapshot.as_deref().unwrap())?;
    let mut s = Snap::open(&snap_path)?;
    let secs = enumerate_sections(&mut s)?;

    if args.inspect {
        eprintln!(
            "[snapshot-load] file_format_version={FILE_FORMAT_VERSION} sections={}",
            secs.len()
        );
        if let Some(csh) = find(&secs, "eosio::chain::chain_snapshot_header") {
            s.seek_to(csh.payload_off)?;
            eprintln!("[snapshot-load] chain_snapshot_version={}", s.u32()?);
        }
        for sec in &secs {
            println!(
                "off={:>14}  rows={:>10}  bytes={:>14}  {}",
                sec.payload_off, sec.rows, sec.payload_len, sec.name
            );
        }
        return Ok(());
    }

    let csh = find(&secs, "eosio::chain::chain_snapshot_header")
        .ok_or_else(|| anyhow!("no chain_snapshot_header section"))?;
    s.seek_to(csh.payload_off)?;
    let chain_version = s.u32()?;
    eprintln!(
        "[snapshot-load] file_format_version={FILE_FORMAT_VERSION} chain_snapshot_version={chain_version} sections={} threads={}",
        secs.len(), args.threads
    );
    match chain_version {
        // 2..=6 all use the commingled `contract_tables` section (FIO is v2, Telos/WAX v6); 8 is split.
        // The consumption + count invariants guard against any per-version layout drift.
        2..=6 | 8 => {}
        7 => bail!("chain snapshot version 7 is unsupported (transient Spring 1.0.0 format)"),
        v => bail!("unsupported chain snapshot version {v}"),
    }
    if chain_version < 6 {
        eprintln!("[snapshot-load] note: v{chain_version} (pre-v6) — commingled layout, relying on consumption/count invariants");
    }

    let block_num = match args.block_num {
        Some(b) => b,
        None => block_num_from_filename(&snap_path)
            .ok_or_else(|| anyhow!("could not derive block_num from filename; pass --block-num"))?,
    };
    eprintln!("[snapshot-load] head block_num={block_num}");

    let acct = find(&secs, "eosio::chain::account_object")
        .ok_or_else(|| anyhow!("no account_object section"))?;
    let abi_raw = load_abis(&mut s, acct)?;
    eprintln!(
        "[snapshot-load] {} contract ABIs from {} accounts ({:.1?})",
        abi_raw.len(),
        acct.rows,
        t0.elapsed()
    );

    // ── --wseg: build a WormDB Light-API segment straight from the snapshot — no MongoDB ──
    // One push-based Builder, fed by BOTH native passes (permissions + contract tables) via a single
    // channel; the sink thread owns the Builder and writes the .wseg when both passes finish.
    if let Some(wseg_out) = args.wseg.clone() {
        let chain = args.chain.as_deref().unwrap_or("chain").to_string();
        let (tx, rx) = crossbeam_channel::unbounded::<mongo::SinkItem>();
        let out = wseg_out.clone();
        let sink = std::thread::spawn(move || -> Result<(usize, usize)> {
            let mut b = wseg_build::Builder::new();
            for (coll, doc) in rx.iter() {
                b.push(coll, &doc);
            }
            b.finish(&out).map_err(anyhow::Error::from)
        });
        if want_permissions {
            load_permissions_to_sink(
                &mut s,
                &secs,
                &abi_raw,
                &names,
                nm("eosio")?,
                block_num,
                &tx,
                args.limit,
            )?;
        }
        if !table_specs.is_empty() {
            let t = build_targets(&table_specs, &names)?;
            run_pipeline(
                s,
                &secs,
                chain_version,
                abi_raw,
                Arc::new(atomicassets::SchemaRegistry::default()), // --wseg is Light-API only
                &t,
                block_num,
                args.threads,
                None,
                args.limit,
                args.stats_only,
                args.raw,
                None,
                Some(tx.clone()),
            )?;
        }
        drop(tx);
        let (holders, accounts) = sink.join().map_err(|_| anyhow!("wseg sink panicked"))??;
        eprintln!(
            "[snapshot-load] wrote {wseg_out} (chain {chain}): {holders} holders, {accounts} accounts, in {:.1?}",
            t0.elapsed()
        );
        return Ok(());
    }

    let mongo_cfg = build_mongo_cfg(&args)?;
    if lightapi_preset && mongo_cfg.is_none() {
        bail!("--tables lightapi requires --mongo (it bootstraps the Light-API collections into MongoDB)");
    }
    // Over NDJSON, `permissions` must be the sole selector (native single-threaded decode). With
    // --mongo it is loaded as a dedicated pass that can accompany the contract-table pipeline.
    if want_permissions && mongo_cfg.is_none() && !table_specs.is_empty() {
        bail!("`permissions` is a native section — over NDJSON run it alone (--tables permissions), or use --mongo to load it alongside contract tables");
    }

    // Permissions native pass (`permission_object` / `permission_link_object`).
    if want_permissions {
        match &mongo_cfg {
            // To Mongo: emit the `permissions` collection + `pub_keys` reverse index.
            Some(_) => {
                let pcfg = build_perms_mongo_cfg(&args)?
                    .expect("perms mongo cfg present when mongo_cfg is Some");
                load_permissions_to_mongo(
                    &mut s,
                    &secs,
                    &abi_raw,
                    &names,
                    nm("eosio")?,
                    block_num,
                    pcfg,
                    args.limit,
                )?;
            }
            // NDJSON standalone (sole selector — guarded above).
            None => {
                let mut out: Box<dyn Write> = match &args.out {
                    Some(p) => Box::new(BufWriter::with_capacity(
                        1 << 20,
                        File::create(p).with_context(|| format!("create {p}"))?,
                    )),
                    None => Box::new(BufWriter::new(std::io::stdout())),
                };
                let pst = perms::decode_permissions(
                    &mut s,
                    &secs,
                    &abi_raw,
                    &names,
                    nm("eosio")?,
                    block_num,
                    &mut *out,
                    args.limit,
                    args.stats_only,
                )?;
                out.flush()?;
                eprintln!("[snapshot-load] done in {:.1?}", t0.elapsed());
                eprintln!("[snapshot-load] permissions {pst:#?}");
                return Ok(());
            }
        }
    }

    // No contract-table selectors left (permissions-only Mongo run) → done.
    if table_specs.is_empty() {
        eprintln!("[snapshot-load] done in {:.1?}", t0.elapsed());
        return Ok(());
    }

    // AtomicAssets/AtomicMarket: a seek-path pre-pass over `schemas` + `config` builds the
    // schema-format registry so the main pass can decode every `serialized_data` blob. `s` stays
    // re-seekable (the walkers seek back to the section start), so the main `run_pipeline` is intact.
    let schema_reg = if atomic_preset {
        let reg =
            atomicassets::build_schema_registry(&mut s, &secs, chain_version, &abi_raw, &names)?;
        eprintln!(
            "[snapshot-load] atomicassets: schema registry = {} (collection,schema) formats + {} collection-format fields ({:.1?})",
            reg.schema_count(),
            reg.collection_format_len(),
            t0.elapsed()
        );
        Arc::new(reg)
    } else {
        Arc::new(atomicassets::SchemaRegistry::default())
    };

    // Contract-table selectors + optional Mongo sink (shared with the streaming path).
    let t = build_targets(&table_specs, &names)?;

    let (ps, ws, ms) = run_pipeline(
        s,
        &secs,
        chain_version,
        abi_raw,
        schema_reg,
        &t,
        block_num,
        args.threads,
        args.out.clone(),
        args.limit,
        args.stats_only,
        args.raw,
        mongo_cfg,
        None,
    )?;

    report_run(&args, t0, &ps, &ws, &ms);
    Ok(())
}
