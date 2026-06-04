//! aa-probe — mmap an AtomicAssets `.wseg` and benchmark the faceted query primitives + the resident
//! working set. This is the efficiency proof: only the touched blobs fault in, so a handful of queries
//! stay tens-of-MiB resident regardless of the on-disk size.

use std::collections::HashMap;
use std::fs::File;
use std::hint::black_box;
use std::time::Instant;

use clap::Parser;
use memmap2::Mmap;
use wseg_build::aa_binfmt::{decode_asset, Posting};
use wseg_build::aa_tables::*;

#[derive(Parser)]
#[command(about = "Benchmark an AtomicAssets .wseg segment (query latency + resident working set)")]
struct Args {
    #[arg(long, default_value = "aa-testnet.wseg")]
    seg: String,
    #[arg(long, default_value_t = 2000)]
    iters: usize,
    /// Run a varied-key mixed workload of this many requests (0 = skip; tests RSS under realistic spread).
    #[arg(long, default_value_t = 0)]
    workload: usize,
    /// Model history hydration: add this many ms (one ES batch round-trip) to the fraction of workload
    /// requests that display history (uncached). 0 = state-only.
    #[arg(long, default_value_t = 0)]
    mock_es_ms: u64,
    /// Fraction of requests that display a history field (and so pay one ES round-trip, once, then cached).
    #[arg(long, default_value_t = 0.30)]
    es_display_frac: f64,
}

/// Deterministic xorshift64 PRNG (no dep, reproducible).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Zipf-ish index into a heaviness-sorted pool: ~80% of picks land in the hot first fifth.
    fn pick(&mut self, len: usize) -> usize {
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

struct TableLoc {
    key_count: usize,
    index_off: usize,
    index_len: usize,
    blob_off: usize,
    blob_len: usize,
}

struct Seg {
    mmap: Mmap,
    tables: HashMap<u32, TableLoc>,
    order: Vec<u32>,
}

fn rdu32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rdu64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl Seg {
    fn open(path: &str) -> std::io::Result<Seg> {
        let f = File::open(path)?;
        let mmap = unsafe { Mmap::map(&f)? };
        assert_eq!(&mmap[0..8], b"WSEG0001", "bad magic");
        let table_count = rdu32(&mmap, 16) as usize;
        let mut tables = HashMap::new();
        let mut order = Vec::new();
        let mut p = 40usize;
        for _ in 0..table_count {
            let table_id = rdu32(&mmap, p);
            let key_count = rdu64(&mmap, p + 8) as usize;
            let index_off = rdu64(&mmap, p + 16) as usize;
            let index_len = rdu64(&mmap, p + 24) as usize;
            let blob_off = rdu64(&mmap, p + 32) as usize;
            let blob_len = rdu64(&mmap, p + 40) as usize;
            tables.insert(
                table_id,
                TableLoc {
                    key_count,
                    index_off,
                    index_len,
                    blob_off,
                    blob_len,
                },
            );
            order.push(table_id);
            p += 48;
        }
        Ok(Seg {
            mmap,
            tables,
            order,
        })
    }

    fn entry(&self, t: &TableLoc, i: usize) -> (u64, u64, u32) {
        let o = t.index_off + i * 20;
        (
            rdu64(&self.mmap, o),
            rdu64(&self.mmap, o + 8),
            rdu32(&self.mmap, o + 16),
        )
    }
    fn blob(&self, t: &TableLoc, off: u64, len: u32) -> &[u8] {
        let s = t.blob_off + off as usize;
        &self.mmap[s..s + len as usize]
    }
    fn blob_at(&self, table_id: u32, off: u64, len: u32) -> Option<&[u8]> {
        self.tables.get(&table_id).map(|t| self.blob(t, off, len))
    }
    fn lookup(&self, table_id: u32, key: u64) -> Option<&[u8]> {
        let t = self.tables.get(&table_id)?;
        let (mut lo, mut hi) = (0usize, t.key_count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let (k, off, len) = self.entry(t, mid);
            match k.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Some(self.blob(t, off, len)),
            }
        }
        None
    }
    /// Sample ~`samples` entries across a table to find a heavy (large-posting) key, cheaply.
    fn heaviest(&self, table_id: u32, samples: usize) -> Option<(u64, u64, u32)> {
        let t = self.tables.get(&table_id)?;
        if t.key_count == 0 {
            return None;
        }
        let step = (t.key_count / samples).max(1);
        let mut best = self.entry(t, 0);
        let mut i = 0;
        while i < t.key_count {
            let e = self.entry(t, i);
            if e.2 > best.2 {
                best = e;
            }
            i += step;
        }
        Some(best)
    }
    /// Sample up to `n` keys across a table, returned heaviest-first (so a zipf pick hits hot keys).
    fn sample_pool(&self, table_id: u32, n: usize) -> Vec<(u64, u64, u32)> {
        let Some(t) = self.tables.get(&table_id) else {
            return Vec::new();
        };
        let step = (t.key_count / n.max(1)).max(1);
        let mut v: Vec<(u64, u64, u32)> = (0..t.key_count)
            .step_by(step)
            .map(|i| self.entry(t, i))
            .collect();
        v.sort_unstable_by_key(|e| std::cmp::Reverse(e.2)); // by posting/blob size desc
        v
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

fn bench(iters: usize, mut f: impl FnMut() -> u64) -> (u128, u128) {
    let mut durs = Vec::with_capacity(iters);
    let mut acc = 0u64;
    for _ in 0..iters {
        let t = Instant::now();
        acc ^= f();
        durs.push(t.elapsed().as_nanos());
    }
    black_box(acc);
    (pctl(&mut durs, 0.5), pctl(&mut durs, 0.99))
}

fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let file_sz = std::fs::metadata(&args.seg)?.len();
    let seg = Seg::open(&args.seg)?;

    // ── size breakdown ──
    println!(
        "\n=== segment {} ({} MiB on disk) ===",
        args.seg,
        file_sz >> 20
    );
    let label = |id: u32| -> &'static str {
        match id {
            TABLE_AA_FWD => "asset_fwd",
            TABLE_AA_TMPL_FWD => "template_fwd",
            TABLE_AA_SCHEMAS => "schemas",
            TABLE_AA_BY_OWNER => "by_owner",
            TABLE_AA_BY_COLL => "by_collection",
            TABLE_AA_BY_SCHEMA => "by_schema",
            TABLE_AA_BY_TMPL => "by_template",
            TABLE_AA_DATA_ATTR => "by_data_attr",
            TABLE_AA_SORTED_ID => "sorted_id",
            TABLE_AA_SORTED_TMPL => "sorted_tmpl",
            _ => "?",
        }
    };
    println!(
        "{:<14} {:>10} {:>12} {:>12}",
        "table", "keys", "index", "blobs"
    );
    for id in &seg.order {
        let t = &seg.tables[id];
        println!(
            "{:<14} {:>10} {:>9} MB {:>9} MB",
            label(*id),
            t.key_count,
            t.index_len >> 20,
            t.blob_len >> 20
        );
    }

    let base_rss = rss_mb();
    println!("\nRSS after mmap+parse (cold): {base_rss:.1} MB");

    // ── pick representative keys from the segment itself ──
    let sorted = seg.tables.get(&TABLE_AA_SORTED_ID).map(|t| {
        let (_, off, len) = seg.entry(t, 0);
        seg.blob(t, off, len)
    });
    let pick_asset = |k: usize| -> u64 {
        sorted
            .map(|b| {
                let n = rdu32(b, 0) as usize;
                rdu64(b, 4 + k.min(n.saturating_sub(1)) * 8)
            })
            .unwrap_or(0)
    };
    let asset_id = pick_asset(1000);
    let (owner_key, _, _) = seg.heaviest(TABLE_AA_BY_OWNER, 4096).unwrap_or((0, 0, 0));
    let (coll_key, _, _) = seg.heaviest(TABLE_AA_BY_COLL, 4096).unwrap_or((0, 0, 0));
    let (data_key, _, _) = seg.heaviest(TABLE_AA_DATA_ATTR, 4096).unwrap_or((0, 0, 0));

    // ── Q1: point lookup by asset_id (+ template join) ──
    let q1 = bench(args.iters, || {
        let mut h = 0u64;
        if let Some(b) = seg.lookup(TABLE_AA_FWD, asset_id) {
            let a = decode_asset(b);
            h ^= a.owner;
            if a.template_id >= 0 {
                if let Some(tb) = seg.lookup(TABLE_AA_TMPL_FWD, template_key(a.template_id as i64))
                {
                    h ^= tb.len() as u64;
                }
            }
        }
        h
    });
    // ── Q2: owner's assets, page 1 of 100 (top/newest = the posting head) ──
    let q2 = bench(args.iters, || {
        seg.lookup(TABLE_AA_BY_OWNER, owner_key)
            .map(|b| Posting::parse(b).head_xor(100))
            .unwrap_or(0)
    });
    // ── Q3: faceted filter `collection,data:rarity=X` — a SINGLE collection+schema-scoped posting
    // (data_attr_key already includes the collection), so this is one lookup + page slice, not an
    // intersect.
    let q3 = bench(args.iters, || {
        seg.lookup(TABLE_AA_DATA_ATTR, data_key)
            .map(|b| Posting::parse(b).head_xor(100))
            .unwrap_or(0)
    });
    // ── Q4: collection page 1 of 100 ──
    let q4 = bench(args.iters, || {
        seg.lookup(TABLE_AA_BY_COLL, coll_key)
            .map(|b| Posting::parse(b).head_xor(100))
            .unwrap_or(0)
    });
    // ── Q5: browse all sorted by asset_id desc, page slice ──
    let q5 = bench(args.iters, || {
        let mut h = 0u64;
        if let Some(t) = seg.tables.get(&TABLE_AA_SORTED_ID) {
            let (_, off, len) = seg.entry(t, 0);
            let b = seg.blob(t, off, len);
            let n = rdu32(b, 0) as usize;
            for k in 1000..1100.min(n) {
                h ^= rdu64(b, 4 + k * 8);
            }
        }
        h
    });
    // ── Q6: SORT BY MINT — the materialized template_mint ordering + forward-blob hydration. This is
    // a "history-looking" sort served entirely from the segment (no Elasticsearch). (mint u32, aid u64)
    let q6 = bench(args.iters, || {
        let mut h = 0u64;
        if let Some(t) = seg.tables.get(&TABLE_AA_SORTED_TMPL) {
            let (_, off, len) = seg.entry(t, 0);
            let b = seg.blob(t, off, len);
            let n = rdu32(b, 0) as usize;
            for k in 1000..1100.min(n) {
                let aid = rdu64(b, 4 + k * 12 + 4); // skip the u32 mint
                if let Some(fb) = seg.lookup(TABLE_AA_FWD, aid) {
                    h ^= decode_asset(fb).template_mint as u64;
                }
            }
        }
        h
    });

    let hot_rss = rss_mb();
    println!(
        "RSS after {} iters × 5 queries (hot): {hot_rss:.1} MB",
        args.iters
    );
    println!(
        "query working set (hot − cold): {:.1} MB",
        hot_rss - base_rss
    );

    let owner_n = seg
        .lookup(TABLE_AA_BY_OWNER, owner_key)
        .map(|b| Posting::parse(b).len())
        .unwrap_or(0);
    let coll_n = seg
        .lookup(TABLE_AA_BY_COLL, coll_key)
        .map(|b| Posting::parse(b).len())
        .unwrap_or(0);
    let data_n = seg
        .lookup(TABLE_AA_DATA_ATTR, data_key)
        .map(|b| Posting::parse(b).len())
        .unwrap_or(0);
    println!(
        "\n=== query latency (P50 / P99 over {} iters) ===",
        args.iters
    );
    let row = |n: &str, q: (u128, u128), note: String| {
        println!("{n:<40} {:>7} ns / {:>8} ns   {note}", q.0, q.1)
    };
    row("Q1 point lookup (asset + template join)", q1, String::new());
    row(
        "Q2 owner page (sort id desc, 100)",
        q2,
        format!("owner has {owner_n} assets"),
    );
    row(
        "Q3 facet data:rarity page (single posting)",
        q3,
        format!("collection-scoped rarity posting {data_n}"),
    );
    row(
        "Q4 collection page (100)",
        q4,
        format!("collection has {coll_n} assets"),
    );
    row("Q5 browse sorted_id slice (100)", q5, String::new());
    row(
        "Q6 SORT BY MINT + hydrate (materialized)",
        q6,
        "history-looking sort, no ES".to_string(),
    );

    // ── Multi-filter intersect microbench (hybrid postings): a genuine two-large-posting AND. Most
    // faceted queries are single collection-scoped postings (Q3); this is the rarer multi-attr case. ──
    if let (Some(cb), Some(db)) = (
        seg.lookup(TABLE_AA_BY_COLL, coll_key),
        seg.lookup(TABLE_AA_DATA_ATTR, data_key),
    ) {
        let (pc, pd) = (Posting::parse(cb), Posting::parse(db));
        let (nc, nd) = (pc.len(), pd.len());
        // real per-query path: materialize a roaring from each posting (deserialize / build) + AND.
        let r_full = bench(args.iters, || (pc.to_roaring() & pd.to_roaring()).len());
        // AND-only lower bound (bitmaps already resident / cached).
        let (rc, rd) = (pc.to_roaring(), pd.to_roaring());
        let r_and = bench(args.iters, || (&rc & &rd).len());

        let stored = cb.len() + db.len();
        let raw_equiv = (nc + nd) * 8 + 10;
        println!("\n=== multi-filter intersect (hybrid postings): {nc} ∩ {nd} ===");
        println!(
            "on-disk hybrid: {} KB   vs raw-u64 would be {} KB  ({:.1}× smaller in the segment)",
            stored >> 10,
            raw_equiv >> 10,
            raw_equiv as f64 / stored.max(1) as f64,
        );
        println!(
            "intersect to_roaring + AND : {:>9} ns / {:>9} ns",
            r_full.0, r_full.1
        );
        println!(
            "intersect AND only         : {:>9} ns / {:>9} ns",
            r_and.0, r_and.1
        );
    }

    if args.workload > 0 {
        run_workload(
            &seg,
            args.workload,
            base_rss,
            args.mock_es_ms,
            args.es_display_frac,
        );
    }
    Ok(())
}

/// Varied-key mixed workload: zipf-weighted keys across a realistic query mix. Every page result is
/// HYDRATED (its 100 forward blobs are read, as a real server would), so the RSS reflects real serving.
/// Optionally overlays a mock Elasticsearch history hydration (one ~`mock_es_ms` round-trip on the
/// `es_display_frac` of requests that show a history field, once per item then cached).
fn run_workload(seg: &Seg, n: usize, base_rss: f64, mock_es_ms: u64, es_display_frac: f64) {
    let owners = seg.sample_pool(TABLE_AA_BY_OWNER, 50_000);
    let colls = seg.sample_pool(TABLE_AA_BY_COLL, 50_000);
    let datas = seg.sample_pool(TABLE_AA_DATA_ATTR, 50_000);
    let (sorted_off, sorted_cnt) = seg
        .tables
        .get(&TABLE_AA_SORTED_ID)
        .map(|t| {
            let (_, off, _) = seg.entry(t, 0);
            (off, rdu32(&seg.mmap, t.blob_off + off as usize) as usize)
        })
        .unwrap_or((0, 0));
    let assets: Vec<u64> = if let Some(t) = seg.tables.get(&TABLE_AA_SORTED_ID) {
        let b = seg.blob(t, sorted_off, (4 + sorted_cnt * 8) as u32);
        let step = (sorted_cnt / 200_000).max(1);
        (0..sorted_cnt)
            .step_by(step)
            .map(|k| rdu64(b, 4 + k * 8))
            .collect()
    } else {
        Vec::new()
    };

    println!(
        "\n=== varied-key workload: {} reqs over {} owners / {} collections / {} facets / {} assets ===",
        n,
        owners.len(),
        colls.len(),
        datas.len(),
        assets.len()
    );
    // a page = posting head (top 100 ids) + HYDRATE each result's forward blob (as a real server does)
    let page = |seg: &Seg, tid: u32, off: u64, len: u32| -> u64 {
        let mut h = 0u64;
        if let Some(b) = seg.blob_at(tid, off, len) {
            Posting::parse(b).head_for_each(100, |aid| {
                if let Some(fb) = seg.lookup(TABLE_AA_FWD, aid) {
                    h ^= decode_asset(fb).owner;
                }
            });
        }
        h
    };

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut durs: Vec<u128> = Vec::with_capacity(n);
    let t0 = Instant::now();
    for _ in 0..n {
        let r = rng.next() % 100;
        let t = Instant::now();
        let mut h = 0u64;
        if r < 40 && !assets.is_empty() {
            // zipf toward the front of the pool (= newest asset_ids = the hot, recently-minted ones)
            let aid = assets[rng.pick(assets.len())];
            if let Some(b) = seg.lookup(TABLE_AA_FWD, aid) {
                let a = decode_asset(b);
                h ^= a.owner;
                if a.template_id >= 0 {
                    if let Some(tb) =
                        seg.lookup(TABLE_AA_TMPL_FWD, template_key(a.template_id as i64))
                    {
                        h ^= tb.len() as u64;
                    }
                }
            }
        } else if r < 65 && !owners.is_empty() {
            let (_, off, len) = owners[rng.pick(owners.len())];
            h ^= page(seg, TABLE_AA_BY_OWNER, off, len);
        } else if r < 85 && !colls.is_empty() {
            let (_, off, len) = colls[rng.pick(colls.len())];
            h ^= page(seg, TABLE_AA_BY_COLL, off, len);
        } else if r < 95 && !datas.is_empty() {
            let (_, off, len) = datas[rng.pick(datas.len())];
            h ^= page(seg, TABLE_AA_DATA_ATTR, off, len);
        } else if sorted_cnt > 100 {
            let k = rng.next() as usize % (sorted_cnt - 100);
            if let Some(tt) = seg.tables.get(&TABLE_AA_SORTED_ID) {
                let b = seg.blob(tt, sorted_off, (4 + sorted_cnt * 8) as u32);
                for j in k..k + 100 {
                    h ^= rdu64(b, 4 + j * 8);
                }
            }
        }
        black_box(h);
        durs.push(t.elapsed().as_nanos());
    }
    let secs = t0.elapsed().as_secs_f64();
    let rss1 = rss_mb();
    durs.sort_unstable();
    let p = |q: f64| durs[((durs.len() as f64 * q) as usize).min(durs.len() - 1)];
    println!(
        "throughput: {:.0} req/s ({:.1}s, single thread) | latency p50 {} ns | p99 {} ns | p999 {} ns | max {} µs",
        n as f64 / secs,
        secs,
        p(0.5),
        p(0.99),
        p(0.999),
        durs[durs.len() - 1] / 1000
    );
    println!(
        "RSS: {:.1} MB → {:.1} MB  (working set under varied keys: {:.1} MB)",
        base_rss,
        rss1,
        rss1 - base_rss
    );

    // History-hydration overlay: the `es_display_frac` of requests that show a history field pay ONE
    // ES batch round-trip (~mock_es_ms) the first time, then it's cached. The state path is untouched.
    if mock_es_ms > 0 {
        let add = (mock_es_ms as u128) * 1_000_000; // ms → ns
        let pay_every = (1.0 / es_display_frac.max(0.001)).round().max(1.0) as usize;
        let mut blended = durs.clone();
        for (i, d) in blended.iter_mut().enumerate() {
            if i % pay_every == 0 {
                *d += add; // this request displayed history and missed the cache
            }
        }
        blended.sort_unstable();
        let pb = |q: f64| blended[((blended.len() as f64 * q) as usize).min(blended.len() - 1)];
        let us = |ns: u128| ns as f64 / 1000.0;
        println!(
            "history overlay (display {:.0}%, ES {} ms, cached after 1st hit): p50 {:.1} µs | p99 {:.0} µs | p999 {:.0} µs",
            es_display_frac * 100.0,
            mock_es_ms,
            us(pb(0.5)),
            us(pb(0.99)),
            us(pb(0.999)),
        );
        println!(
            "  → state-only stays p50 {} ns; only the displayed-history fraction pays the one ES round-trip.",
            durs[durs.len() / 2]
        );
    }
}
