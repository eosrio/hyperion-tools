//! action-proto: decode action traces straight from `trace_history.{log,index}` into Hyperion
//! action documents (NDJSON files or Elasticsearch `_bulk`). See the library for the decode.
//!
//! The range is cut into 512-block chunks that worker threads take one at a time. Each chunk is
//! read as one slab per log; one fold hands the chunks' documents to the sink in block order, so
//! an output file is deterministic and lists blocks in order.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, mpsc};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;

use action_proto::{
    BlockInput, BlockWorker, DOC_BACKLOG, DocKind, DocMsg, EsCfg, EsStats, Failures, Producer,
    Segment, Stats, WireDocs, es_poster, init_protocol, is_wire, load_abi_index,
};
use hyperion_ship::blocks::BlockLog;
use hyperion_ship::slab::{BlockFile, Chunks, ShipFile, ordered_chunks};

/// Blocks per work unit. Chunks hold their slabs and documents until the ordered fold takes them,
/// so memory grows with the chunk; 512 blocks keeps reads large and is as fast as 2,048 on Wire
/// while holding a third of the memory.
const CHUNK: u32 = 512;

#[derive(Parser, Debug)]
#[command(about = "Experimental: decode action_traces directly from the trace_history log.")]
struct Args {
    /// Protocol selection is checked before inputs, outputs or workers are opened.
    #[arg(long, default_value = "antelope")]
    protocol: hyperion_ship::provider::ProtocolProvider,

    /// nodeos state-history dir (trace_history.{log,index})
    #[arg(long)]
    from_disk: String,
    /// abi-index NDJSON produced by abi-scanner ({account, block, abi_hex, ...})
    #[arg(long)]
    abi_index: String,
    #[arg(long)]
    start: u32,
    #[arg(long)]
    end: u32,
    #[arg(long, default_value_t = 8)]
    threads: u32,
    /// output NDJSON (omit to measure pure decode throughput)
    #[arg(long)]
    out: Option<String>,
    /// nodeos blocks dir (blocks.{log,index}) — supplies the block-header fields `@timestamp`
    /// and `producer`, which live only in signed_block (Phase B). Omit to leave them out.
    #[arg(long)]
    blocks_dir: Option<String>,
    /// match Hyperion's `features.index_transfer_memo`: move `memo` from a transfer's `act.data`
    /// into `@transfer.memo` (default: keep `memo` in `act.data`).
    #[arg(long, default_value_t = false)]
    index_transfer_memo: bool,
    /// COLD TIER: emit metadata-only docs — drop `act.data` (the on-demand payload that
    /// `archive-server` reconstructs from the logs), keeping all searchable metadata + the computed
    /// `@`-fields. `block_num` + `global_sequence` stay, so the API/archive can fetch act.data.
    #[arg(long, default_value_t = false)]
    metadata_only: bool,
    /// direct Elasticsearch `_bulk` sink (e.g. http://localhost:9200). When set, decoded docs are
    /// POSTed straight to ES (the `--out` NDJSON path is ignored) to find the true ES write ceiling
    /// without the GIL-bound Python loader.
    #[arg(long)]
    es: Option<String>,
    /// chain name for the ES `_index` (`<chain>-action-<version>-<partition>`).
    #[arg(long, default_value = "wax")]
    chain: String,
    /// index version for the ES `_index` (`<chain>-action-<version>-<partition>`).
    #[arg(long, default_value = "v1")]
    index_version: String,
    /// `INDEX_PARTITION_SIZE`: partition = ceil(block_num / partition_size), zero-padded to 6 digits.
    #[arg(long, default_value_t = 10_000_000)]
    partition_size: u64,
    /// docs per `_bulk` request (per poster thread).
    #[arg(long, default_value_t = 4000)]
    es_batch: usize,
    /// concurrent `_bulk` poster threads pulling from the doc channel.
    #[arg(long, default_value_t = 8)]
    es_workers: usize,
    /// Wire: chain id (64 lowercase hex). Every action document then carries schema-3 occurrence
    /// metadata (`protocol`), which the 4.5 API proves from the node's logs. Needs --blocks-dir.
    #[arg(long)]
    chain_id: Option<String>,
    /// Wire: also write a `<chain>-block-<version>` document (`_id` = block number) for every block
    /// of the range, including blocks before the trace log's first entry. Needs --blocks-dir.
    #[arg(long, default_value_t = false)]
    blocks: bool,
    /// NDJSON output for block documents, when --out writes NDJSON instead of --es.
    #[arg(long)]
    blocks_out: Option<String>,
    /// Wire: `protocol.profileId` written into block documents (default: the --protocol profile id).
    #[arg(long)]
    doc_profile_id: Option<String>,
}

/// One worker's state: its decoder and, for Antelope headers, its own block-log reader.
struct Worker<'a> {
    decoder: BlockWorker<'a>,
    headers: Option<BlockLog>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    args.protocol.validate()?;
    let timestamps = args.protocol.timestamp_profile()?;
    init_protocol(&args.protocol)?;
    eprintln!("[action-proto] protocol {}", args.protocol.id());
    let wire = is_wire();
    let wire_profile_id = match &args.protocol {
        hyperion_ship::provider::ProtocolProvider::Wire(profile) => Some(profile.id.clone()),
        _ => None,
    };
    if args.chain_id.is_some() || args.blocks {
        if wire_profile_id.is_none() {
            bail!(
                "--chain-id and --blocks write Hyperion 4.5 Wire documents; select a Wire --protocol"
            );
        }
        if args.blocks_dir.is_none() {
            bail!(
                "--chain-id and --blocks need --blocks-dir: every block's id and parent come from its header"
            );
        }
    }
    if let Some(id) = &args.chain_id
        && (id.len() != 64 || !id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')))
    {
        bail!("--chain-id must be 64 lowercase hex characters");
    }
    if args.blocks && args.es.is_none() && args.out.is_some() && args.blocks_out.is_none() {
        bail!("--blocks with --out needs --blocks-out for the block documents");
    }
    let docs = WireDocs {
        chain_id: args.chain_id.clone(),
        blocks: args.blocks,
        profile_id: args
            .doc_profile_id
            .clone()
            .or(wire_profile_id)
            .unwrap_or_default(),
    };
    eprintln!("[action-proto] loading ABI index {} ...", args.abi_index);
    let abi_index = load_abi_index(&args.abi_index)?;
    eprintln!("[action-proto] {} contracts in ABI index", abi_index.len());

    let trace = ShipFile::open(Path::new(&args.from_disk), "trace_history")?;
    let first_block = trace.first_block();
    let last_block = trace
        .last_block()?
        .context("the trace log's index is empty")?;
    // Every document needs its block header; never run outside the block log.
    let block_file = match args.blocks_dir.as_deref() {
        Some(dir) => Some(BlockFile::open(Path::new(dir))?),
        None => None,
    };
    let (requested_start, end) = match &block_file {
        Some(blocks) => {
            let (first, last) = (
                blocks.first_block(),
                blocks.last_block()?.context("the block log is empty")?,
            );
            if args.start < first || args.end.min(last_block) > last {
                eprintln!(
                    "[action-proto] block log holds [{first}..{last}]; clamping the range so every document has its block header"
                );
            }
            (args.start.max(first), args.end.min(last_block).min(last))
        }
        None => (args.start, args.end.min(last_block)),
    };
    let start = requested_start.max(first_block);
    if start > end {
        bail!("empty range after clamp to [{first_block}..{last_block}]");
    }
    // Block documents also cover blocks the trace log does not reach.
    let range_start = if args.blocks {
        requested_start.min(start)
    } else {
        start
    };
    let threads = args.threads.max(1) as usize;
    let total = (end - start + 1) as u64;
    eprintln!(
        "[action-proto] decoding action_traces [{start}..{end}] ({total} blocks) with {threads} threads"
    );

    let stats = Stats::default();
    let failures = Failures::default();
    let (tx, rx) = mpsc::sync_channel::<DocMsg>(DOC_BACKLOG);

    // Output mode: --es (direct ES `_bulk`) takes precedence over --out (NDJSON); with neither set
    // we just drain the channel (pure-decode benchmark). Only build the NDJSON writer when ES is
    // NOT in play, so `--es` cleanly ignores `--out`.
    let es_cfg = args.es.as_ref().map(|url| EsCfg {
        url: format!("{}/_bulk", url.trim_end_matches('/')),
        chain: args.chain.clone(),
        version: args.index_version.clone(),
        partition_size: args.partition_size.max(1),
        batch: args.es_batch.max(1),
    });
    let es_workers = args.es_workers.max(1);
    let es_stats = Arc::new(EsStats::default());
    let create = |path: &Option<String>| -> Result<Option<Box<dyn Write + Send>>> {
        match (es_cfg.is_some(), path) {
            (false, Some(p)) => Ok(Some(Box::new(BufWriter::new(
                File::create(p).with_context(|| format!("create {p}"))?,
            )))),
            _ => Ok(None),
        }
    };
    let mut out = create(&args.out)?;
    let mut blocks_out = create(&args.blocks_out)?;
    // Workers build documents whenever there is a consumer (ES posters or an NDJSON writer).
    let emit = es_cfg.is_some() || out.is_some() || blocks_out.is_some();
    if let Some(c) = &es_cfg {
        eprintln!(
            "[action-proto] direct-ES sink -> {} | index={}-action-{}-<part> partition_size={} batch={} posters={}",
            c.url, c.chain, c.version, c.partition_size, c.batch, es_workers
        );
    }
    let t0 = Instant::now();

    let producer = Producer {
        abi_index: &abi_index,
        index_transfer_memo: args.index_transfer_memo,
        metadata_only: args.metadata_only,
        timestamps,
        docs: &docs,
        stats: &stats,
        failures: &failures,
    };
    let walk_blocks = wire && block_file.is_some();
    let run = std::thread::scope(|s| {
        // Consumer side: either N ES `_bulk` posters sharing the Receiver, or the single NDJSON
        // writer thread (actions to --out, blocks to --blocks-out), or a lone drain thread.
        let rx_shared = Arc::new(std::sync::Mutex::new(rx));
        let mut consumers = Vec::new();
        if let Some(cfg) = es_cfg.clone() {
            for _ in 0..es_workers {
                let (rxc, cfgc, est) = (rx_shared.clone(), cfg.clone(), es_stats.clone());
                consumers.push(s.spawn(move || {
                    es_poster(&rxc, &cfgc, &est);
                }));
            }
        } else {
            consumers.push(s.spawn(move || {
                for msg in rx_shared.lock().unwrap().iter() {
                    let target = match msg.kind {
                        DocKind::Action(_) => out.as_mut(),
                        DocKind::Block => blocks_out.as_mut(),
                    };
                    if let Some(w) = target {
                        let _ = writeln!(w, "{}", msg.doc);
                    }
                }
                for w in [out.as_mut(), blocks_out.as_mut()].into_iter().flatten() {
                    let _ = w.flush();
                }
            }));
        }

        let mut previous: Option<Segment> = None;
        let result = ordered_chunks(
            Chunks {
                from: range_start,
                to: end,
                size: CHUNK,
                threads,
                window: threads + 2,
                limit: None,
            },
            || {
                Ok(Worker {
                    decoder: BlockWorker::new(producer),
                    headers: match (&block_file, walk_blocks) {
                        (Some(_), false) => {
                            Some(BlockLog::open(args.blocks_dir.as_deref().unwrap())?)
                        }
                        _ => None,
                    },
                })
            },
            |w, cs, ce| -> Result<(Vec<DocMsg>, Option<Segment>)> {
                let mut out = Vec::new();
                let mut segment = None;
                let blocks = match (&block_file, walk_blocks) {
                    (Some(file), true) => Some(file.slab(cs, ce)?),
                    _ => None,
                };
                let traced_from = cs.max(first_block);
                for block_num in cs..traced_from.min(ce + 1) {
                    w.decoder.untraced(
                        block_num,
                        blocks.as_ref().and_then(|b| b.block(block_num)),
                        &mut segment,
                        emit.then_some(&mut out),
                    )?;
                }
                if traced_from <= ce {
                    let slab = trace.slab(traced_from, ce)?;
                    stats.stale.fetch_add(slab.skipped(), Relaxed);
                    for block_num in traced_from..=ce {
                        let entry = slab.entry(block_num)?;
                        let input = match (&blocks, &mut w.headers) {
                            (Some(b), _) => match b.block(block_num) {
                                Some(bytes) => BlockInput::Bytes(bytes),
                                None => BlockInput::Missing,
                            },
                            (None, Some(log)) => BlockInput::Header(log.header(block_num)),
                            (None, None) => BlockInput::None,
                        };
                        w.decoder.traced(
                            block_num,
                            entry.id(),
                            entry.payload,
                            input,
                            &mut segment,
                            emit.then_some(&mut out),
                        )?;
                    }
                }
                // A walked chunk must cover its whole range, one chained run of blocks.
                if walk_blocks {
                    match segment {
                        Some(run) if run.first == cs && run.last == ce => {}
                        other => bail!(
                            "chunk [{cs}..{ce}] walked {}",
                            other.map_or("no blocks".to_string(), |run| format!(
                                "only [{}..{}]",
                                run.first, run.last
                            ))
                        ),
                    }
                }
                Ok((out, segment))
            },
            |_, _, (chunk, segment)| {
                // Chunks chain into one run: each chunk's first header names the previous chunk's
                // last block as its parent.
                if let (Some(before), Some(after)) = (previous, segment)
                    && (after.first != before.last + 1 || after.first_previous != before.last_id)
                {
                    bail!(
                        "block {} does not follow block {}: its header names {} as its parent, but block {} is {}",
                        after.first,
                        before.last,
                        hex::encode(after.first_previous),
                        before.last,
                        hex::encode(before.last_id)
                    );
                }
                if segment.is_some() {
                    previous = segment;
                }
                for msg in chunk {
                    tx.send(msg).context("the document sink stopped")?;
                }
                Ok(())
            },
        );
        drop(tx);
        for c in consumers {
            let _ = c.join();
        }
        result
    });

    let secs = t0.elapsed().as_secs_f64();
    let b = stats.blocks.load(Relaxed);
    let txs = stats.txs.load(Relaxed);
    let actions = stats.actions.load(Relaxed);
    let decoded = stats.decoded.load(Relaxed);
    let raw = stats.raw.load(Relaxed);
    eprintln!(
        "[action-proto] done: {b} blocks in {secs:.1}s ({:.0} blk/s) | txs={txs} actions={actions} -> docs={} | act.data decoded={decoded} ({:.2}%) + raw={raw} recovered_via_block-1={} no_abi={}",
        b as f64 / secs.max(1e-9),
        stats.docs.load(Relaxed),
        if actions > 0 {
            100.0 * decoded as f64 / actions as f64
        } else {
            0.0
        },
        stats.recovered.load(Relaxed),
        stats.no_abi.load(Relaxed),
    );
    if args.blocks {
        eprintln!(
            "[action-proto] block docs={}",
            stats.block_docs.load(Relaxed)
        );
    }
    let stale = stats.stale.load(Relaxed);
    if stale > 0 {
        eprintln!(
            "[action-proto] skipped {stale} bytes of superseded log entries (forked blocks the index no longer points to)"
        );
    }
    let unreadable = stats.unreadable.load(Relaxed);
    if unreadable > 0 {
        eprintln!(
            "[action-proto] WARNING: {unreadable} trace entries could not be read, so their actions are missing. First blocks: {:?}",
            stats.unreadable_blocks.lock().unwrap()
        );
    }
    let mut failed = Vec::new();
    if let Err(e) = run {
        failed.push(format!("{e:#}"));
    }
    if es_cfg.is_some() {
        let posted = es_stats.docs.load(Relaxed);
        let bytes = es_stats.bytes.load(Relaxed);
        let requests = es_stats.requests.load(Relaxed);
        let errors = es_stats.errors.load(Relaxed);
        let mb = bytes as f64 / 1e6;
        eprintln!(
            "[action-proto] ES sink: posted {posted} docs in {secs:.1}s ({:.0} docs/s) | {mb:.1} MB ({:.1} MB/s) | {requests} bulk reqs | errors={errors}",
            posted as f64 / secs.max(1e-9),
            mb / secs.max(1e-9),
        );
        if errors > 0 {
            failed.push(format!(
                "{errors} bulk request(s) reported errors — inspect ES response / mapping conflicts"
            ));
        }
    }
    let f = failures.lock().unwrap();
    if !f.is_empty() {
        let mut by_reason: std::collections::BTreeMap<&str, u64> =
            std::collections::BTreeMap::new();
        for ((reason, _, _), (cnt, _)) in f.iter() {
            *by_reason.entry(reason).or_default() += cnt;
        }
        eprintln!("[action-proto] raw (undecodable) act.data by reason: {by_reason:?}");
        let mut top: Vec<_> = f.iter().collect();
        top.sort_by_key(|e| std::cmp::Reverse(e.1.0));
        eprintln!("[action-proto] top undecoded contract/action:");
        for ((reason, code, action), (cnt, sample)) in top.into_iter().take(25) {
            eprintln!("  {cnt:>6}  {reason:<8} {code}/{action}  {sample}");
        }
    }
    if !failed.is_empty() {
        for failure in &failed {
            eprintln!("[action-proto] FAILED: {failure}");
        }
        bail!("{} failure(s); the output is incomplete", failed.len());
    }
    Ok(())
}
