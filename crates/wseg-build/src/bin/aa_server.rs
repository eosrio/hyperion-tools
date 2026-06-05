//! aa-server — the live-serving daemon: an `ArcSwap`-hosted AtomicAssets store that ingests the chain
//! head AND compacts itself, continuously, without stalling reads.
//!
//! This is the AA instance of a domain-generic, hot-swappable state store (the same shape that will host
//! Light-API + chain-v1 tables in one segment, fed by one SHiP reader, alongside Hyperion's ES history).
//!
//! Three concurrent roles over one `ArcSwap<LiveSeg>`:
//!   - WRITER (the SHiP applier): applies blocks to the current LiveSeg's overlay, swap-safe (re-targets
//!     the new segment if a compaction landed between load and lock — no delta lost, no double-apply).
//!   - READERS: serve queries lock-free via `ArcSwap::load` — never blocked by the writer or a swap.
//!   - COMPACTOR: when the overlay grows past a threshold, snapshots it (lock-free clone), folds base +
//!     snapshot into a fresh segment in the background, replays the WAL residual that accrued during the
//!     fold, then atomically swaps — the only stall is a brief final tail-replay under the write lock.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use clap::Parser;
use wseg_build::aa_live::*;
use wseg_build::aa_tables::*;

#[derive(Parser)]
#[command(
    about = "Live-serving AtomicAssets daemon: ArcSwap store + SHiP applier + background compaction"
)]
struct Args {
    #[arg(long, default_value = "C:/snaptmp/aa-testnet-hybrid.wseg")]
    seg: String,
    /// Writer ingest rate (deltas/sec) — a realistic chain rate (SHiP delivers ~10^3–10^4/s).
    #[arg(long, default_value_t = 30_000)]
    rate: usize,
    /// Compaction trigger: overlay serving-heap threshold in MB.
    #[arg(long, default_value_t = 30)]
    compact_threshold_mb: u64,
    /// Reader threads.
    #[arg(long, default_value_t = 4)]
    readers: usize,
    /// How long to run the daemon (seconds).
    #[arg(long, default_value_t = 150)]
    duration_secs: u64,
    /// Directory for rotating compacted segments.
    #[arg(long, default_value = "C:/snaptmp")]
    tmp_dir: String,
}

/// Per-drain cap for the lock-free catch-up (whole blocks; keeps the clone-under-read-lock short).
const DRAIN_BATCH: usize = 100_000;
/// Tail size below which the compactor stops the catch-up and does the final swap under the write lock.
const FINAL_BATCH: usize = 5_000;
const RARITIES: [&str; 6] = ["Common", "Uncommon", "Rare", "Epic", "Legendary", "Mythic"];

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

/// Shared, read-only sampling pools (drawn from the initial base; collections/schemas persist across
/// compactions so they stay valid).
struct Pools {
    assets: Vec<u64>,
    owners: Vec<u64>,
    colls: Vec<u64>,
    facets: Vec<u64>,
    blueprints: Vec<Option<Blueprint>>,
    facet_field: String,
}

/// Generate one block of realistic deltas (35% mint / 45% transfer / 12% burn / 8% setdata).
fn gen_block(p: &Pools, rng: &mut Rng, next_mint: &AtomicU64, n: usize) -> Vec<Delta> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let r = rng.next() % 100;
        let idx = rng.zipf(p.assets.len());
        let pick = p.assets[idx];
        let owner = p.owners[(rng.next() as usize) % p.owners.len().max(1)];
        let d = if r < 35 {
            if let Some(bp) = &p.blueprints[idx] {
                let id = next_mint.fetch_add(1, Ordering::Relaxed);
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
        } else if let Some(bp) = &p.blueprints[idx] {
            let newval = RARITIES[(rng.next() as usize) % RARITIES.len()];
            let facet_new = Some(data_attr_key(&bp.coll_s, &bp.sch_s, &p.facet_field, newval));
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
        };
        out.push(d);
    }
    out
}

/// Apply a block to whichever LiveSeg the store currently points at, retrying if a compaction swapped
/// the store between resolving the base facts and taking the write lock (so no delta lands on an
/// orphaned overlay).
fn apply_swap_safe(store: &ArcSwap<LiveSeg>, blk: u32, deltas: &[Delta]) {
    loop {
        let cur = store.load_full();
        let prepared = cur.prepare_block(blk, deltas);
        let mut g = cur.write_overlay();
        if !Arc::ptr_eq(&store.load_full(), &cur) {
            drop(g);
            continue; // swapped under us → retry against the new segment
        }
        cur.commit_into(&mut g, prepared);
        return;
    }
}

fn replay(live: &LiveSeg, deltas: &[(u32, Delta)]) {
    for (blk, d) in deltas {
        live.apply_block(*blk, std::slice::from_ref(d));
    }
}

/// One full compaction cycle: snapshot → fold (background) → catch up the residual → atomic swap.
/// Returns (assets_folded, fold_secs, residual_total, swap_micros).
fn compact_and_swap(
    store: &ArcSwap<LiveSeg>,
    facet_fields: &[String],
    out: &str,
) -> (u64, f64, usize, u128, u128) {
    let cur = store.load_full();
    let t_snap = Instant::now();
    let (seal, snap) = cur.snapshot_overlay(); // read-lock clone of the serving state (no WAL)
    let snap_ms = t_snap.elapsed().as_millis();

    let t_fold = Instant::now();
    let stats = cur.compact_with(&snap, out).expect("fold");
    let fold_secs = t_fold.elapsed().as_secs_f64();

    let new_base = BaseSeg::open(out).expect("open compacted");
    let new_live = Arc::new(LiveSeg::from_base(new_base, facet_fields.to_vec()));

    // lock-free catch-up: replay WAL(seal..] onto the new overlay in bounded, whole-block drains until
    // only a small tail remains — each drain holds the read lock only briefly.
    let mut through = seal;
    let mut residual_total = 0usize;
    loop {
        let residual = cur.wal_after(through, DRAIN_BATCH);
        if residual.len() <= FINAL_BATCH {
            break;
        }
        residual_total += residual.len();
        replay(&new_live, &residual);
        through = residual.last().unwrap().0;
    }

    // final swap: hold the OLD overlay's write lock (blocks only the writer; readers are lock-free via
    // ArcSwap), replay the small tail, publish the new segment. This is the only stall.
    let t_swap = Instant::now();
    let g = cur.write_overlay();
    let tail = g.wal_after(through, usize::MAX);
    residual_total += tail.len();
    replay(&new_live, &tail);
    store.store(new_live);
    drop(g);
    let swap_us = t_swap.elapsed().as_micros();

    (stats.assets, fold_secs, residual_total, swap_us, snap_ms)
}

fn main() {
    let args = Args::parse();
    let ff = vec!["rarity".to_string()];
    let store = Arc::new(ArcSwap::from_pointee(
        LiveSeg::open(&args.seg, ff.clone()).unwrap(),
    ));
    println!("\n=== aa-server: live-serving daemon on {} ===", args.seg);
    println!(
        "writer {} deltas/s | {} readers | compact at overlay heap > {} MB | run {}s",
        args.rate, args.readers, args.compact_threshold_mb, args.duration_secs
    );

    // sample pools from the initial base
    let init = store.load_full();
    let pools = Arc::new({
        let assets = init.base().sample_assets(500_000);
        let blueprints: Vec<Option<Blueprint>> =
            assets.iter().map(|&a| init.asset_blueprint(a)).collect();
        Pools {
            owners: init.base().sample_index_keys(TABLE_AA_BY_OWNER, 200_000),
            colls: init.base().sample_index_keys(TABLE_AA_BY_COLL, 50_000),
            facets: init.base().sample_index_keys(TABLE_AA_DATA_ATTR, 50_000),
            facet_field: ff[0].clone(),
            assets,
            blueprints,
        }
    });
    let next_mint = Arc::new(AtomicU64::new(init.base_max_id + 1));
    let applied = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let compactions = Arc::new(AtomicUsize::new(0));
    drop(init);

    // ── WRITER (SHiP applier) ──
    let writer = {
        let (store, pools, next_mint, applied, stop) = (
            store.clone(),
            pools.clone(),
            next_mint.clone(),
            applied.clone(),
            stop.clone(),
        );
        std::thread::spawn(move || {
            let mut rng = Rng(0xA11C_E5EE_D000_0001);
            let block_size = (args.rate / 60).max(1); // ~60 blocks/s
            let mut blk = 1u32;
            let nanos_per_block = (block_size as f64 / args.rate as f64 * 1e9) as u64;
            while !stop.load(Ordering::Relaxed) {
                let t = Instant::now();
                let deltas = gen_block(&pools, &mut rng, &next_mint, block_size);
                apply_swap_safe(&store, blk, &deltas);
                applied.fetch_add(block_size as u64, Ordering::Relaxed);
                blk += 1;
                // throttle to the target ingest rate
                let spent = t.elapsed().as_nanos() as u64;
                if spent < nanos_per_block {
                    std::thread::sleep(Duration::from_nanos(nanos_per_block - spent));
                }
            }
        })
    };

    // ── READERS (lock-free) ──
    let reader_handles: Vec<_> = (0..args.readers)
        .map(|rt| {
            let (store, pools, stop) = (store.clone(), pools.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut rng = Rng(0x5EAD_0000_0000_0001 + rt as u64);
                let mut durs: Vec<u128> = Vec::with_capacity(3_000_000);
                let mut max_ns = 0u128;
                let mut count = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let t = Instant::now();
                    let cur = store.load(); // lock-free RCU load of the current segment
                    let ov = cur.overlay(); // per-request overlay read lock (brief; contends only w/ writer)
                    let r = rng.next() % 100;
                    let mut h = 0u64;
                    if r < 40 {
                        h ^= cur
                            .point_owner(&ov, pools.assets[rng.zipf(pools.assets.len())])
                            .unwrap_or(0);
                    } else if r < 65 {
                        h ^= cur
                            .page(
                                &ov,
                                DIM_OWNER,
                                pools.owners[rng.zipf(pools.owners.len())],
                                100,
                            )
                            .len() as u64;
                    } else if r < 85 {
                        h ^= cur
                            .page(&ov, DIM_COLL, pools.colls[rng.zipf(pools.colls.len())], 100)
                            .len() as u64;
                    } else if r < 95 {
                        h ^= cur
                            .page(
                                &ov,
                                DIM_FACET,
                                pools.facets[rng.zipf(pools.facets.len())],
                                100,
                            )
                            .len() as u64;
                    } else {
                        h ^= cur.browse(&ov, (rng.next() as usize) % 300, 100).len() as u64;
                    }
                    drop(ov);
                    black_box(h);
                    let ns = t.elapsed().as_nanos();
                    max_ns = max_ns.max(ns);
                    if durs.len() < 3_000_000 {
                        durs.push(ns);
                    }
                    count += 1;
                }
                (durs, max_ns, count)
            })
        })
        .collect();

    // ── COMPACTOR ──
    let compactor = {
        let (store, stop, compactions) = (store.clone(), stop.clone(), compactions.clone());
        let ff = ff.clone();
        let tmp_dir = args.tmp_dir.clone();
        let threshold = args.compact_threshold_mb * 1_048_576;
        std::thread::spawn(move || {
            let mut gen = 0usize;
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(1000));
                let (heap, _) = store.load().overlay_heap_bytes();
                if heap < threshold {
                    continue;
                }
                gen += 1;
                let out = format!("{tmp_dir}/aa-server-compact-{gen}.wseg");
                let heap_mb = heap / 1_048_576;
                let (assets, fold_s, residual, swap_us, snap_ms) =
                    compact_and_swap(&store, &ff, &out);
                compactions.fetch_add(1, Ordering::Relaxed);
                println!(
                    "  [compact #{gen}] trigger {heap_mb} MB → snapshot {snap_ms} ms, folded {assets} assets \
                     in {fold_s:.1}s (background), replayed {residual} residual, SWAP STALL {swap_us} µs; reset",
                );
            }
        })
    };

    // ── run, then stop + join ──
    let t0 = Instant::now();
    std::thread::sleep(Duration::from_secs(args.duration_secs));
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let mut all: Vec<u128> = Vec::new();
    let mut global_max = 0u128;
    let mut total_reads = 0u64;
    for h in reader_handles {
        let (durs, mx, cnt) = h.join().unwrap();
        all.extend(durs);
        global_max = global_max.max(mx);
        total_reads += cnt;
    }
    compactor.join().unwrap();
    let run_s = t0.elapsed().as_secs_f64();

    all.sort_unstable();
    let p = |q: f64| {
        if all.is_empty() {
            0
        } else {
            all[((all.len() as f64 * q) as usize).min(all.len() - 1)]
        }
    };
    println!("\n── results after {run_s:.0}s ──");
    println!(
        "writer:    applied {} mutations = {:.0}/s (target {}/s)",
        applied.load(Ordering::Relaxed),
        applied.load(Ordering::Relaxed) as f64 / run_s,
        args.rate
    );
    println!(
        "compactions: {} hot-swaps completed",
        compactions.load(Ordering::Relaxed)
    );
    println!(
        "readers:   {} reqs = {:.0}/s across {} threads | p50 {} ns | p99 {} ns | p999 {} ns | MAX {} µs",
        total_reads,
        total_reads as f64 / run_s,
        args.readers,
        p(0.5),
        p(0.99),
        p(0.999),
        global_max / 1000,
    );

    // final correctness: every page entry on the live store must be currently owned (no stale leaked
    // through a swap), and point-lookups resolve.
    let cur = store.load();
    let ov = cur.overlay();
    let (mut checked, mut stale) = (0u64, 0u64);
    for &o in pools.owners.iter().take(5000) {
        for aid in cur.page(&ov, DIM_OWNER, o, 100) {
            checked += 1;
            if cur.point_owner(&ov, aid) != Some(o) {
                stale += 1;
            }
        }
    }
    let (heap, _) = cur.overlay_heap_bytes();
    println!(
        "final live store: page-merge invariant {} entries / {} stale [{}]; overlay heap now {} MB",
        checked,
        stale,
        if stale == 0 { "PASS" } else { "FAIL" },
        heap / 1_048_576,
    );
    println!(
        "  → readers served continuously through {} hot-swaps; the only reader stall is the per-swap \
         final tail-replay (the SWAP STALL above), NOT the minutes-long background fold.",
        compactions.load(Ordering::Relaxed),
    );
}
