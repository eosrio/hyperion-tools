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
    // ── Q3: collection ∩ data:rarity=X (multi-filter intersect) ──
    let q3 = bench(args.iters, || {
        let mut out = Vec::new();
        if let (Some(a), Some(b)) = (
            seg.lookup(TABLE_AA_BY_COLL, q3_coll),
            seg.lookup(TABLE_AA_DATA_ATTR, data_key),
        ) {
            PostingList::intersect(&PostingList::new(a), &PostingList::new(b), &mut out);
        }
        out.len() as u64 ^ out.first().copied().unwrap_or(0)
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
        "Q3 collection ∩ data:rarity (intersect)",
        q3,
        format!("rarity posting {data_n}"),
    );
    row(
        "Q4 collection page (100)",
        q4,
        format!("collection has {coll_n} assets"),
    );
    row("Q5 browse sorted_id slice (100)", q5, String::new());
    Ok(())
}
