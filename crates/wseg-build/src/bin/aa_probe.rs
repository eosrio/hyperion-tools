//! aa-probe — mmap an AtomicAssets `.wseg` and benchmark the faceted query primitives + the resident
//! working set. This is the efficiency proof: only the touched blobs fault in, so a handful of queries
//! stay tens-of-MiB resident regardless of the on-disk size.

use std::collections::HashMap;
use std::fs::File;
use std::hint::black_box;
use std::time::Instant;

use clap::Parser;
use memmap2::Mmap;
use wseg_build::aa_binfmt::{decode_asset, PostingList};
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
    let (data_key, d_off, d_len) = seg.heaviest(TABLE_AA_DATA_ATTR, 4096).unwrap_or((0, 0, 0));
    // For Q3, intersect that rarity posting with its first asset's collection posting (a real overlap).
    let q3_coll = seg
        .tables
        .get(&TABLE_AA_DATA_ATTR)
        .map(|t| {
            let pl = PostingList::new(seg.blob(t, d_off, d_len));
            let first = if pl.len > 0 { pl.get(0) } else { asset_id };
            seg.lookup(TABLE_AA_FWD, first)
                .map(|b| decode_asset(b).collection)
                .unwrap_or(coll_key)
        })
        .unwrap_or(coll_key);

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
    // ── Q2: owner's assets, last 100 (sort=asset_id desc) ──
    let q2 = bench(args.iters, || {
        let mut h = 0u64;
        if let Some(b) = seg.lookup(TABLE_AA_BY_OWNER, owner_key) {
            let pl = PostingList::new(b);
            for i in pl.len.saturating_sub(100)..pl.len {
                h ^= pl.get(i);
            }
        }
        h
    });
    // ── Q3: faceted filter `collection,data:rarity=X` — a SINGLE collection+schema-scoped posting
    // (data_attr_key already includes the collection), so this is one lookup + page slice, not an
    // intersect. (q3_coll kept only for the intersect microbench below.)
    let _ = q3_coll;
    let q3 = bench(args.iters, || {
        let mut h = 0u64;
        if let Some(b) = seg.lookup(TABLE_AA_DATA_ATTR, data_key) {
            let pl = PostingList::new(b);
            for i in pl.len.saturating_sub(100)..pl.len {
                h ^= pl.get(i);
            }
        }
        h
    });
    // ── Q4: collection page, last 100 ──
    let q4 = bench(args.iters, || {
        let mut h = 0u64;
        if let Some(b) = seg.lookup(TABLE_AA_BY_COLL, coll_key) {
            let pl = PostingList::new(b);
            for i in pl.len.saturating_sub(100)..pl.len {
                h ^= pl.get(i);
            }
        }
        h
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
        .map(|b| PostingList::new(b).len)
        .unwrap_or(0);
    let coll_n = seg
        .lookup(TABLE_AA_BY_COLL, coll_key)
        .map(|b| PostingList::new(b).len)
        .unwrap_or(0);
    let data_n = PostingList::new(
        seg.lookup(TABLE_AA_DATA_ATTR, data_key)
            .unwrap_or(&[0, 0, 0, 0]),
    )
    .len;
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

    // ── Multi-filter intersect microbench: a GENUINE two-large-posting AND (the worst case — most
    // faceted queries are single collection-scoped postings, see Q3). Raw sorted-merge vs roaring. ──
    if let (Some(craw), Some(draw)) = (
        seg.lookup(TABLE_AA_BY_COLL, coll_key),
        seg.lookup(TABLE_AA_DATA_ATTR, data_key),
    ) {
        use roaring::RoaringTreemap;
        let (plc, pld) = (PostingList::new(craw), PostingList::new(draw));
        let raw = bench(args.iters, || {
            let mut out = Vec::new();
            PostingList::intersect(&plc, &pld, &mut out);
            out.len() as u64
        });
        let rt_c: RoaringTreemap = (0..plc.len).map(|i| plc.get(i)).collect();
        let rt_d: RoaringTreemap = (0..pld.len).map(|i| pld.get(i)).collect();
        let (mut bc, mut bd) = (Vec::new(), Vec::new());
        rt_c.serialize_into(&mut bc).unwrap();
        rt_d.serialize_into(&mut bd).unwrap();
        let r_full = bench(args.iters, || {
            let a = RoaringTreemap::deserialize_from(&bc[..]).unwrap();
            let b = RoaringTreemap::deserialize_from(&bd[..]).unwrap();
            (a & b).len()
        });
        let r_and = bench(args.iters, || (&rt_c & &rt_d).len());

        println!(
            "\n=== multi-filter intersect: {} ∩ {} postings (worst case) ===",
            plc.len, pld.len
        );
        println!(
            "posting storage:  raw {} KB   roaring {} KB  ({:.1}× smaller)",
            (craw.len() + draw.len()) >> 10,
            (bc.len() + bd.len()) >> 10,
            (craw.len() + draw.len()) as f64 / (bc.len() + bd.len()).max(1) as f64,
        );
        println!(
            "intersect raw sorted-merge  : {:>9} ns / {:>9} ns",
            raw.0, raw.1
        );
        println!(
            "intersect roaring deser+AND : {:>9} ns / {:>9} ns",
            r_full.0, r_full.1
        );
        println!(
            "intersect roaring AND only  : {:>9} ns / {:>9} ns",
            r_and.0, r_and.1
        );
    }

    if args.workload > 0 {
        run_workload(&seg, args.workload, base_rss);
    }
    Ok(())
}

/// Varied-key mixed workload: random keys (zipf-weighted toward hot/heavy keys) across a realistic
/// query mix — the test of whether the resident set stays bounded under real spread (vs. the fixed-key
/// bench where the same blobs are re-touched). Point lookups use UNIFORM-random asset_ids (worst case
/// for the forward-store working set); page queries use a zipf hot-set.
fn run_workload(seg: &Seg, n: usize, base_rss: f64) {
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
    let page = |seg: &Seg, tid: u32, off: u64, len: u32| -> u64 {
        let mut h = 0u64;
        if let Some(b) = seg.blob_at(tid, off, len) {
            let pl = PostingList::new(b);
            for i in pl.len.saturating_sub(100)..pl.len {
                h ^= pl.get(i);
            }
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
}
