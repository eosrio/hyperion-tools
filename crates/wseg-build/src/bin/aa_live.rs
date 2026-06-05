//! aa-live — prove the AtomicAssets WormDB store can serve the CHAIN HEAD, not just a frozen snapshot.
//!
//! Opens the immutable base `.wseg`, then drives a realistic SHiP-shaped mutation stream
//! (mint / transfer / burn / setdata, sampled from REAL owners / collections / facets in the segment)
//! through the in-RAM freshness overlay and measures:
//!   1. apply throughput (must clear SHiP burst rates ~10^4/s),
//!   2. merged-read latency Q1–Q6 with the overlay active (must stay sub-µs / low-µs),
//!   3. correctness at scale (a transfer/burn/mint/setdata is reflected immediately),
//!   4. concurrent freshness (a writer applies blocks while readers serve; the lag = block apply time),
//!   5. overlay RAM vs mutation count (the compaction trigger).

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use wseg_build::aa_live::*;
use wseg_build::aa_tables::*;
use wseg_build::name;

#[derive(Parser)]
#[command(about = "Drive a live mutation stream through the AA freshness overlay and benchmark it")]
struct Args {
    #[arg(long, default_value = "C:/snaptmp/aa-5m.wseg")]
    seg: String,
    /// Mutations to pre-generate + apply for the apply-throughput + latency phases.
    #[arg(long, default_value_t = 2_000_000)]
    mutations: usize,
    /// Deltas per block (the SHiP applier batches a block under one write lock).
    #[arg(long, default_value_t = 250)]
    block_size: usize,
    /// Read requests for the merged-read latency microbench (per query).
    #[arg(long, default_value_t = 20_000)]
    iters: usize,
    /// Reader requests for the concurrent freshness phase (0 = skip).
    #[arg(long, default_value_t = 2_000_000)]
    concurrent: usize,
    /// Reader threads in the concurrent phase.
    #[arg(long, default_value_t = 4)]
    readers: usize,
    /// Compact (fold overlay → fresh segment) at the end and verify equivalence (0 = skip).
    #[arg(long, default_value_t = true)]
    compact: bool,
    /// Output path for the compacted segment.
    #[arg(long, default_value = "C:/snaptmp/aa-compacted.wseg")]
    compact_out: String,
}

struct Rng(u64);
impl Rng {
    #[inline]
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Zipf-ish pick: ~80% land in the hot first fifth (newest / most-popular).
    #[inline]
    fn zipf(&mut self, len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        if self.next() % 100 < 80 {
            (self.next() as usize) % (len / 5).max(1)
        } else {
            (self.next() as usize) % len
        }
    }
}

fn rss_mb() -> f64 {
    memory_stats::memory_stats()
        .map(|m| m.physical_mem as f64 / 1_048_576.0)
        .unwrap_or(0.0)
}

fn pctl(d: &mut [u128], p: f64) -> u128 {
    d.sort_unstable();
    d[((d.len() as f64 * p) as usize).min(d.len() - 1)]
}

const RARITIES: [&str; 6] = ["Common", "Uncommon", "Rare", "Epic", "Legendary", "Mythic"];

/// Pre-generate a realistic stream. Weighted mix (mints + transfers dominate, as on WAX):
/// 35% mint, 45% transfer, 12% burn, 8% setdata. Keys are sampled from REAL base postings.
fn generate(
    live: &LiveSeg,
    assets: &[u64],
    owners: &[u64],
    bps: &[Option<Blueprint>],
    n: usize,
    mint_ctr: &mut u64,
    seed: u64,
) -> Vec<Delta> {
    let mut rng = Rng(seed);
    let mut out = Vec::with_capacity(n);
    let facet_field = live.facet_fields().first().cloned().unwrap_or_default();
    for _ in 0..n {
        let r = rng.next() % 100;
        let idx = rng.zipf(assets.len());
        let pick = assets[idx];
        let owner = owners[(rng.next() as usize) % owners.len().max(1)];
        let d = if r < 35 {
            // MINT into a real template/collection (reuse the blueprint of a real asset)
            if let Some(bp) = &bps[idx] {
                *mint_ctr += 1;
                let id = live.base_max_id + *mint_ctr;
                Delta::Mint(MintD {
                    asset_id: id,
                    owner,
                    collection: bp.collection,
                    schema: bp.schema,
                    schema_key: bp.schema_key,
                    template_id: bp.template_id,
                    facet_key: bp.facet_key,
                    immutable: vec![],
                    mutable: vec![],
                })
            } else {
                Delta::Transfer(TransferD {
                    asset_id: pick,
                    new_owner: owner,
                })
            }
        } else if r < 80 {
            Delta::Transfer(TransferD {
                asset_id: pick,
                new_owner: owner,
            })
        } else if r < 92 {
            Delta::Burn(BurnD { asset_id: pick })
        } else if std::env::var("AA_NO_SETDATA").is_ok() {
            Delta::Transfer(TransferD {
                asset_id: pick,
                new_owner: owner,
            })
        } else {
            // SETDATA: move the asset's facet (rarity) to a new value within its own collection/schema
            if let Some(bp) = &bps[idx] {
                let newval = RARITIES[(rng.next() as usize) % RARITIES.len()];
                let facet_new = Some(data_attr_key(&bp.coll_s, &bp.sch_s, &facet_field, newval));
                Delta::SetData(SetDataD {
                    asset_id: pick,
                    mutable: vec![],
                    facet_old: bp.facet_key,
                    facet_new,
                })
            } else {
                Delta::Transfer(TransferD {
                    asset_id: pick,
                    new_owner: owner,
                })
            }
        };
        out.push(d);
    }
    out
}

fn main() {
    let args = Args::parse();
    println!("\n=== aa-live: freshness overlay on {} ===", args.seg);
    let live = Arc::new(LiveSeg::open(&args.seg, vec!["rarity".to_string()]).unwrap());
    let base_rss = rss_mb();
    println!(
        "base mmap opened; base_max_id = {}; RSS {:.0} MB",
        live.base_max_id, base_rss
    );

    // ── sample real keys to drive the stream ──
    let assets = live.base().sample_assets(500_000);
    let owners = live.base().sample_index_keys(TABLE_AA_BY_OWNER, 200_000);
    let colls = live.base().sample_index_keys(TABLE_AA_BY_COLL, 50_000);
    let facets = live.base().sample_index_keys(TABLE_AA_DATA_ATTR, 50_000);
    println!(
        "sampled {} assets / {} owners / {} collections / {} facets",
        assets.len(),
        owners.len(),
        colls.len(),
        facets.len()
    );
    // precompute blueprints for the asset pool (mints/setdata reuse them — keeps apply timing pure)
    let bps: Vec<Option<Blueprint>> = assets.iter().map(|&a| live.asset_blueprint(a)).collect();
    let with_facet = bps
        .iter()
        .filter(|b| b.as_ref().and_then(|x| x.facet_key).is_some())
        .count();
    println!(
        "blueprints: {}/{} sampled assets carry an indexed facet (rarity)",
        with_facet,
        bps.len()
    );

    // ── Phase A — apply throughput (pre-generated stream, pure apply timing) ──
    let n = args.mutations;
    live.reserve(n + args.concurrent / 2 + 1024); // avoid realloc spikes inside timed commits
    let mut mint_ctr = 0u64;
    println!("\n[A] generating {} mutations…", n);
    let t_gen = Instant::now();
    let stream = generate(
        &live,
        &assets,
        &owners,
        &bps,
        n,
        &mut mint_ctr,
        0x1234_5678_9abc_def0,
    );
    println!("    generated in {:.1}s", t_gen.elapsed().as_secs_f64());

    println!(
        "[A] applying in blocks of {} (lock-free prepare + brief commit)…",
        args.block_size
    );
    let mut block_durs: Vec<u128> = Vec::new();
    let t_apply = Instant::now();
    let mut block = 1u32;
    for chunk in stream.chunks(args.block_size) {
        let prepared = live.prepare_block(block, chunk); // base mmap reads, no lock
        let tb = Instant::now();
        live.commit_block(prepared); // pure in-RAM, brief write lock
        block_durs.push(tb.elapsed().as_nanos());
        block += 1;
    }
    let apply_secs = t_apply.elapsed().as_secs_f64();
    let (fwd, add_k, rem_k, applied, tomb) = live.overlay_stats();
    let (heap, wal) = live.overlay_heap_bytes();
    let post_apply_rss = rss_mb();
    println!(
        "    applied {} mutations in {:.1}s = {:.0} mutations/s (single thread)",
        applied,
        apply_secs,
        applied as f64 / apply_secs
    );
    println!(
        "    block COMMIT (write-lock hold = freshness lag): p50 {} µs / p99 {} µs / max {} µs  (pure in-RAM; base faults happen lock-free in prepare)",
        pctl(&mut block_durs.clone(), 0.5) / 1000,
        pctl(&mut block_durs.clone(), 0.99) / 1000,
        block_durs.iter().max().unwrap_or(&0) / 1000
    );
    println!(
        "    overlay: {} forward records, {} add-keys, {} rem-keys, {} tombstones",
        fwd, add_k, rem_k, tomb
    );
    println!(
        "    overlay HEAP (serving structures): {:.0} MB = {:.0} B/mutation  | WAL (→disk in prod): {:.0} MB",
        heap as f64 / 1_048_576.0,
        heap as f64 / applied as f64,
        wal as f64 / 1_048_576.0,
    );
    println!(
        "    process RSS {:.0} MB (incl. base mmap pages faulted by the apply path — evictable page cache, not overlay heap)",
        post_apply_rss,
    );

    // ── Phase B — merged-read latency Q1–Q6 (overlay active, read guard held) ──
    println!(
        "\n[B] merged-read latency with overlay active (P50 / P99 over {} iters):",
        args.iters
    );
    let ov = live.overlay();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);

    let bench = |label: &str, iters: usize, mut f: Box<dyn FnMut() -> u64 + '_>| {
        let mut d = Vec::with_capacity(iters);
        let mut acc = 0u64;
        for _ in 0..iters {
            let t = Instant::now();
            acc ^= f();
            d.push(t.elapsed().as_nanos());
        }
        black_box(acc);
        println!(
            "    {:<42} {:>7} ns / {:>8} ns",
            label,
            pctl(&mut d.clone(), 0.5),
            pctl(&mut d, 0.99)
        );
    };

    bench(
        "Q1 point lookup (overlay-first)",
        args.iters,
        Box::new(|| {
            let a = assets[rng.zipf(assets.len())];
            live.point_owner(&ov, a).unwrap_or(0)
        }),
    );
    bench(
        "Q2 owner page-1 (merged, validated, 100)",
        args.iters,
        Box::new(|| {
            let o = owners[rng.zipf(owners.len())];
            let p = live.page(&ov, DIM_OWNER, o, 100);
            p.iter().fold(p.len() as u64, |h, &x| h ^ x)
        }),
    );
    bench(
        "Q3 facet page-1 (merged, 100)",
        args.iters,
        Box::new(|| {
            let f = facets[rng.zipf(facets.len())];
            let p = live.page(&ov, DIM_FACET, f, 100);
            p.iter().fold(p.len() as u64, |h, &x| h ^ x)
        }),
    );
    bench(
        "Q4 collection page-1 (merged, 100)",
        args.iters,
        Box::new(|| {
            let c = colls[rng.zipf(colls.len())];
            let p = live.page(&ov, DIM_COLL, c, 100);
            p.iter().fold(p.len() as u64, |h, &x| h ^ x)
        }),
    );
    bench(
        "Q5 browse newest page-1 (overlay mints first)",
        args.iters,
        Box::new(|| {
            // page 1..a few: offset pagination is O(skip); a real server uses cursor pagination (O(1)).
            let skip = (rng.next() as usize) % 300;
            let p = live.browse(&ov, skip, 100);
            p.iter().fold(p.len() as u64, |h, &x| h ^ x)
        }),
    );
    // Q6 sort-by-mint slice + overlay-aware hydrate (tombstones skipped)
    let stmpl = live
        .base()
        .sentinel_blob(TABLE_AA_SORTED_TMPL)
        .map(|b| b.to_vec());
    bench(
        "Q6 sort-by-mint slice + hydrate (100)",
        args.iters,
        Box::new(|| {
            let mut h = 0u64;
            if let Some(b) = &stmpl {
                let cnt = u32::from_le_bytes(b[0..4].try_into().unwrap()) as usize;
                if cnt > 100 {
                    // hot window (page-1-ish): isolates the overlay merge+hydrate from cold mmap faults
                    // (a random deep slice would just measure 100 scattered cold page faults, same cost
                    // for ANY store — the frozen probe's 4.8µs likewise used a fixed warm slice).
                    let start = (rng.next() as usize) % 2000.min(cnt - 100);
                    for k in start..start + 100 {
                        let aid = u64::from_le_bytes(
                            b[4 + k * 12 + 4..4 + k * 12 + 12].try_into().unwrap(),
                        );
                        if let Some(o) = live.point_owner(&ov, aid) {
                            h ^= o;
                        }
                    }
                }
            }
            h
        }),
    );
    drop(ov);

    // ── Phase C — correctness at scale (a fresh mutation is reflected immediately) ──
    println!("\n[C] correctness at scale:");
    {
        // pick a live base asset, record its owner, transfer it to a sentinel, verify, then burn it.
        let sentinel = name::encode("zzzzzzzzzzzz");
        let probe_asset = assets
            .iter()
            .copied()
            .find(|&a| live.exists(&live.overlay(), a))
            .unwrap();
        let before = live.point_owner(&live.overlay(), probe_asset);
        live.apply_block(
            block,
            &[Delta::Transfer(TransferD {
                asset_id: probe_asset,
                new_owner: sentinel,
            })],
        );
        block += 1;
        let after = live.point_owner(&live.overlay(), probe_asset);
        let in_sent_page = live
            .page(&live.overlay(), DIM_OWNER, sentinel, 100)
            .contains(&probe_asset);
        println!(
            "    transfer asset {} : owner {:?} → {:?}; in sentinel page = {}  [{}]",
            probe_asset,
            before,
            after,
            in_sent_page,
            if after == Some(sentinel) && in_sent_page {
                "PASS"
            } else {
                "FAIL"
            }
        );
        live.apply_block(
            block,
            &[Delta::Burn(BurnD {
                asset_id: probe_asset,
            })],
        );
        block += 1;
        let gone = !live.exists(&live.overlay(), probe_asset);
        println!(
            "    burn asset {} : exists = {}  [{}]",
            probe_asset,
            !gone,
            if gone { "PASS" } else { "FAIL" }
        );

        // merge invariant at scale: EVERY asset a page returns must be currently owned by that key and
        // not tombstoned (no stale candidate ever leaks through validation). Sample 5000 owners.
        let ov = live.overlay();
        let mut checked = 0u64;
        let mut stale = 0u64;
        for &o in owners.iter().take(5000) {
            for aid in live.page(&ov, DIM_OWNER, o, 100) {
                checked += 1;
                if live.point_owner(&ov, aid) != Some(o) {
                    stale += 1;
                }
            }
        }
        println!(
            "    page merge invariant: {} page entries across 5000 owners, {} stale  [{}]",
            checked,
            stale,
            if stale == 0 { "PASS" } else { "FAIL" }
        );
    }

    // ── Phase D — concurrent freshness: writer applies blocks while readers serve ──
    if args.concurrent > 0 {
        println!(
            "\n[D] concurrent: 1 writer applying blocks + {} readers serving {} reqs…",
            args.readers, args.concurrent
        );
        // a fresh stream for the writer to apply during the read storm
        let mut mc = mint_ctr;
        let wstream = Arc::new(generate(
            &live,
            &assets,
            &owners,
            &bps,
            args.concurrent / 2,
            &mut mc,
            0xDEAD_BEEF_CAFE_0001,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let applied_ct = Arc::new(AtomicU64::new(0));
        let start_block = block;

        // writer thread
        let w_live = live.clone();
        let w_stream = wstream.clone();
        let w_stop = stop.clone();
        let w_applied = applied_ct.clone();
        let bs = args.block_size;
        let writer = std::thread::spawn(move || {
            let t = Instant::now();
            let mut maxlock = 0u128;
            for (i, chunk) in w_stream.chunks(bs).enumerate() {
                let blk = start_block + i as u32;
                let prepared = w_live.prepare_block(blk, chunk); // lock-free base reads
                let tb = Instant::now();
                w_live.commit_block(prepared); // brief in-RAM write lock
                maxlock = maxlock.max(tb.elapsed().as_nanos());
                w_applied.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            }
            w_stop.store(true, Ordering::Relaxed);
            (t.elapsed().as_secs_f64(), maxlock)
        });

        // reader threads
        let per = args.concurrent / args.readers.max(1);
        let mut handles = Vec::new();
        for rt in 0..args.readers {
            let r_live = live.clone();
            let r_assets = assets.clone();
            let r_owners = owners.clone();
            let r_colls = colls.clone();
            let r_facets = facets.clone();
            handles.push(std::thread::spawn(move || {
                let mut rng = Rng(0x51ED_2718_0000_0001 + rt as u64);
                let mut durs: Vec<u128> = Vec::with_capacity(per);
                for _ in 0..per {
                    let t = Instant::now();
                    let ov = r_live.overlay(); // acquire read lock per request (real concurrency)
                    let r = rng.next() % 100;
                    let mut h = 0u64;
                    if r < 40 {
                        h ^= r_live
                            .point_owner(&ov, r_assets[rng.zipf(r_assets.len())])
                            .unwrap_or(0);
                    } else if r < 65 {
                        h ^= r_live
                            .page(&ov, DIM_OWNER, r_owners[rng.zipf(r_owners.len())], 100)
                            .len() as u64;
                    } else if r < 85 {
                        h ^= r_live
                            .page(&ov, DIM_COLL, r_colls[rng.zipf(r_colls.len())], 100)
                            .len() as u64;
                    } else if r < 95 {
                        h ^= r_live
                            .page(&ov, DIM_FACET, r_facets[rng.zipf(r_facets.len())], 100)
                            .len() as u64;
                    } else {
                        h ^= r_live
                            .browse(&ov, (rng.next() as usize) % 10_000, 100)
                            .len() as u64;
                    }
                    drop(ov);
                    black_box(h);
                    durs.push(t.elapsed().as_nanos());
                }
                durs
            }));
        }

        let t_conc = Instant::now();
        let mut all: Vec<u128> = Vec::new();
        for h in handles {
            all.extend(h.join().unwrap());
        }
        let (w_secs, w_maxlock) = writer.join().unwrap();
        let _ = stop.load(Ordering::Relaxed);
        let conc_secs = t_conc.elapsed().as_secs_f64();
        all.sort_unstable();
        let p = |q: f64| all[((all.len() as f64 * q) as usize).min(all.len() - 1)];
        println!(
            "    writer: applied {} mutations in {:.1}s = {:.0}/s (max block lock {} µs)",
            applied_ct.load(Ordering::Relaxed),
            w_secs,
            applied_ct.load(Ordering::Relaxed) as f64 / w_secs,
            w_maxlock / 1000
        );
        println!(
            "    readers: {} reqs across {} threads in {:.1}s = {:.0} req/s | p50 {} ns | p99 {} ns | p999 {} ns",
            all.len(),
            args.readers,
            conc_secs,
            all.len() as f64 / conc_secs,
            p(0.5),
            p(0.99),
            p(0.999)
        );
        println!(
            "    → freshness lag is bounded by one block's write-lock hold ({} µs max above): a read after that sees the block.",
            w_maxlock / 1000
        );
    }

    // ── Phase E — compaction: fold base + overlay → fresh segment, verify equivalence + heap reclaim ──
    if args.compact {
        println!("\n[E] compaction: fold base + overlay → a fresh segment…");
        let (h_before, _) = live.overlay_heap_bytes();
        let t = Instant::now();
        let stats = live.compact(&args.compact_out).expect("compact");
        let secs = t.elapsed().as_secs_f64();
        let sz = std::fs::metadata(&args.compact_out)
            .map(|m| m.len())
            .unwrap_or(0);
        println!(
            "    folded {} live assets in {:.1}s → {} ({} MiB on disk)",
            stats.assets,
            secs,
            args.compact_out,
            sz >> 20
        );
        // equivalence: the new base ALONE (empty overlay) must answer identically to old (base+overlay).
        let fresh = LiveSeg::open(&args.compact_out, vec!["rarity".to_string()]).unwrap();
        let ov_old = live.overlay();
        let ov_new = fresh.overlay();
        let mut rng = Rng(0x00C0_FFEE_1234_5678);
        let (mut checks, mut point_m, mut owner_m, mut facet_m) = (0u64, 0u64, 0u64, 0u64);
        let mut examples = 0;
        for _ in 0..50_000 {
            let aid = assets[rng.zipf(assets.len())];
            if live.point_owner(&ov_old, aid) != fresh.point_owner(&ov_new, aid) {
                point_m += 1;
            }
            checks += 1;
        }
        for &o in owners.iter().take(5000) {
            if live.count(&ov_old, DIM_OWNER, o) != fresh.count(&ov_new, DIM_OWNER, o) {
                owner_m += 1;
            }
            checks += 1;
        }
        for &f in facets.iter().take(5000) {
            let (a, b) = (
                live.count(&ov_old, DIM_FACET, f),
                fresh.count(&ov_new, DIM_FACET, f),
            );
            if a != b {
                facet_m += 1;
                if examples < 5 {
                    println!("      facet mismatch: key {f} live={a} fresh={b}");
                    examples += 1;
                }
            }
            checks += 1;
        }
        let mism = point_m + owner_m + facet_m;
        let (h_after, _) = fresh.overlay_heap_bytes();
        println!(
            "    equivalence: {} checks, {} mismatches (point {} / owner-count {} / facet-count {})  [{}]",
            checks,
            mism,
            point_m,
            owner_m,
            facet_m,
            if mism == 0 { "PASS" } else { "FAIL" }
        );
        println!(
            "    overlay heap reclaimed: {} MB (live overlay) → {} MB (fresh base, overlay empty)",
            h_before / 1_048_576,
            h_after / 1_048_576
        );
    }

    let (fwd2, _, _, applied2, tomb2) = live.overlay_stats();
    println!(
        "\nfinal overlay: {} forward records, {} tombstones, {} mutations applied; RSS {:.0} MB (base {:.0} MB)",
        fwd2,
        tomb2,
        applied2,
        rss_mb(),
        base_rss
    );
}
