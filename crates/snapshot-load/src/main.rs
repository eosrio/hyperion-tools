//! snapshot-load — decode active contract-table state directly from an Antelope portable snapshot
//! (`.bin` / `.bin.zst`) and emit Hyperion-shaped NDJSON. No nodeos, no SHiP replay; deterministic
//! point-in-time state. Handles chain snapshot v6 (commingled `contract_tables`) and v8 (split
//! per-table sections). First targets: `eosio` `voters` and every token contract's `accounts` table.
//!
//! Pipeline: a single producer thread scans the file sequentially (framing is length-prefixed, so
//! the scan can't be parallelised) and pushes owned rows onto a bounded channel; N decode workers
//! each own an `AbiHandle` registry (`Send`, not `Sync`) and decode in parallel; one writer drains
//! NDJSON. Bounded channels provide backpressure → bounded memory for EOS-scale (18 GB+) snapshots.

mod map;
mod model;
mod mongo;
mod perms;
mod reader;
mod tables;

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use rs_abieos::Abieos;

use model::{
    format_line, load_abis, AbiRegistry, Dec, Filter, ProducerStats, RawRow, Targets, WorkerStats,
};
use reader::{enumerate_sections, find, Section, Snap, FILE_FORMAT_VERSION};

#[derive(Parser, Debug)]
#[command(
    about = "decode contract-table state (voters, token balances) directly from a portable snapshot .bin[.zst]"
)]
struct Args {
    /// portable snapshot `.bin` or `.bin.zst`
    #[arg(long)]
    snapshot: String,
    /// output NDJSON (omit -> stdout)
    #[arg(long)]
    out: Option<String>,
    /// comma-separated table selectors: `voters` | `accounts` | `*` | `table` | `code:table` | `code:scope:table`
    #[arg(long, default_value = "voters,accounts")]
    tables: String,
    /// override head block_num (else derived from the snapshot filename)
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
    /// optional authSource appended to the URI
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
}

/// If `path` ends in `.zst`, decompress (pure-Rust ruzstd) to the same path without the suffix
/// (reused if present) and return that; else return `path`. The reader needs a seekable file.
fn ensure_decompressed(path: &str) -> Result<String> {
    let Some(out_path) = path.strip_suffix(".zst") else {
        return Ok(path.to_string());
    };
    if !std::path::Path::new(out_path).exists() {
        eprintln!("[snapshot-load] decompressing {path} -> {out_path}");
        let inf = BufReader::new(File::open(path).with_context(|| format!("open {path}"))?);
        let mut dec =
            ruzstd::StreamingDecoder::new(inf).map_err(|e| anyhow!("zstd init: {e:?}"))?;
        let mut outf =
            BufWriter::new(File::create(out_path).with_context(|| format!("create {out_path}"))?);
        std::io::copy(&mut dec, &mut outf)?;
        outf.flush()?;
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

/// Spawn the producer (this thread) + N decode workers + the chosen sink (NDJSON writer thread or the
/// parallel Mongo bridge), wired by bounded channels.
#[allow(clippy::too_many_arguments)]
fn run_pipeline(
    mut s: Snap,
    secs: &[Section],
    chain_version: u32,
    abi_raw: HashMap<u64, Vec<u8>>,
    t: &Targets,
    block_num: u32,
    threads: usize,
    out_path: Option<String>,
    limit: Option<u64>,
    stats_only: bool,
    raw: bool,
    mongo_cfg: Option<mongo::MongoCfg>,
) -> Result<(ProducerStats, WorkerStats, Option<mongo::MongoStats>)> {
    let n_workers = threads.max(1);
    let abi_raw = Arc::new(abi_raw);
    let (row_tx, row_rx) = crossbeam_channel::bounded::<RawRow>(n_workers * 8192);

    // Build the chosen sink channel + the thread that drains it.
    let (line_tx, line_rx) = crossbeam_channel::bounded::<String>(n_workers * 8192);
    let (mongo_tx, mongo_rx) = crossbeam_channel::bounded::<mongo::SinkItem>(n_workers * 8192);
    let out_chan = if mongo_cfg.is_some() {
        OutChan::Mongo(mongo_tx.clone())
    } else {
        OutChan::Ndjson(line_tx.clone())
    };
    drop(line_tx);
    drop(mongo_tx);

    std::thread::scope(|scope| -> Result<(ProducerStats, WorkerStats, Option<mongo::MongoStats>)> {
        // sink thread: NDJSON writer OR Mongo bridge (parallel writers). Exactly one branch runs, so
        // each receiver is consumed in exactly one place.
        let (writer, mongo_handle) = match mongo_cfg {
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
        };

        // decode workers: each owns its own ABI registry + Abieos (for name formatting)
        let out_chan = Arc::new(out_chan);
        let mut workers = Vec::with_capacity(n_workers);
        for _ in 0..n_workers {
            let row_rx = row_rx.clone();
            let abi_raw = Arc::clone(&abi_raw);
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
                                let _ = tx.send(format_line(&names, &row, block_num, &Dec::Err, ""));
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
                                };
                                map::map_row(&ctx, data, &mut reg)
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

        // producer (this thread): walk the framing, send selected rows
        let pstats = {
            let mut sink = |row: RawRow| -> Result<()> {
                row_tx.send(row).map_err(|_| anyhow!("row channel closed"))
            };
            if chain_version < 7 {
                let ct = find(secs, "contract_tables")
                    .ok_or_else(|| anyhow!("no contract_tables section (v6)"))?;
                tables::walk_v6(&mut s, ct, t, limit, &mut sink)?
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
                tables::walk_v8(&mut s, kv, &interesting, limit, &mut sink)?
            }
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
    })
}

fn main() -> Result<()> {
    let args = Args::parse();
    let names = Abieos::new(); // name<->u64 only; no ABI loaded
    let nm = |x: &str| {
        names
            .string_to_name(x)
            .map_err(|e| anyhow!("string_to_name({x}): {e:?}"))
    };

    // --tables selectors. `permissions` is special (a native section, not a contract table) and must be
    // run on its own; all other selectors are contract-table filters parsed in the pipeline branch below.
    let specs: Vec<&str> = args
        .tables
        .split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .collect();
    let want_permissions = specs.contains(&"permissions");
    if want_permissions && specs.len() != 1 {
        bail!("`permissions` is a native section, not a contract table — run it alone: --tables permissions");
    }

    let t0 = Instant::now();
    let snap_path = ensure_decompressed(&args.snapshot)?;
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

    // Permissions: native sections (`permission_object` / `permission_link_object`), decoded by a
    // dedicated single-threaded path (different decode from the contract-table pipeline).
    if want_permissions {
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

    // Contract-table selectors.
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
    let t = Targets { filters };

    // Build the optional Mongo sink config. NDJSON stays the default when --mongo is absent.
    let mongo_cfg = match &args.mongo {
        Some(base_uri) => {
            let chain = args
                .chain
                .as_deref()
                .ok_or_else(|| anyhow!("--chain is required with --mongo (db = <prefix>_<chain>)"))?;
            if args.raw {
                bail!("--raw and --mongo are mutually exclusive (raw emits hex, not decodable docs)");
            }
            let mut uri = base_uri.clone();
            if let Some(src) = &args.mongo_auth_source {
                if uri.contains("?authSource=") || uri.contains("&authSource=") {
                    // already present
                } else if uri.contains('?') {
                    uri.push_str(&format!("&authSource={src}"));
                } else {
                    uri.push_str(&format!("/?authSource={src}"));
                }
            }
            let writers = args.mongo_writers.max(1);
            let db_name = format!("{}_{}", args.mongo_prefix, chain);
            eprintln!(
                "[snapshot-load][mongo] {uri} -> db={db_name} | writers={writers} batch={} pool={} drop={} index={}",
                args.mongo_batch,
                args.mongo_pool.unwrap_or(writers as u32 + 2),
                args.mongo_drop,
                !args.mongo_no_index,
            );
            Some(mongo::MongoCfg {
                uri,
                db_name,
                writers,
                batch: args.mongo_batch.max(1),
                pool: args.mongo_pool.unwrap_or(writers as u32 + 2),
                drop: args.mongo_drop,
                no_index: args.mongo_no_index,
            })
        }
        None => None,
    };

    let (ps, ws, ms) = run_pipeline(
        s,
        &secs,
        chain_version,
        abi_raw,
        &t,
        block_num,
        args.threads,
        args.out,
        args.limit,
        args.stats_only,
        args.raw,
        mongo_cfg,
    )?;

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
    Ok(())
}
