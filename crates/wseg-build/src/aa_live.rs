//! Live freshness overlay for the AtomicAssets faceted store — serve the CHAIN HEAD, not just a frozen
//! snapshot. The base `.wseg` stays immutable + mmap'd; an in-RAM `Overlay` applies the SHiP delta
//! stream (mint / transfer / burn / setdata) and reads MERGE the two so answers are current + correct
//! while staying sub-µs.
//!
//! Design + the adversarial pass that chose it: `benchmark/atomicassets/FRESHNESS_OVERLAY_DESIGN.md`.
//!
//! The one invariant (the "re-validation spine"): the FORWARD record is the sole arbiter of an asset's
//! current owner/collection/schema/facet. Base inverted postings only PROPOSE candidate asset_ids; a
//! candidate is yielded only after the live forward view confirms it still matches the key and isn't
//! tombstoned. So stale entries in the immutable base postings are harmless, and transfer/burn need no
//! base-posting surgery. Per-key `add`/`rem` roaring sets keep the base head dense and make counts exact.

use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::sync::Arc;

use memmap2::Mmap;
use roaring::RoaringTreemap;

use crate::aa_binfmt::{
    decode_asset, decode_collection, decode_schema_format, decode_template, Attr, Posting,
};
use crate::aa_builder::{AaStats, AtomicBuilder};
use crate::aa_tables::*;
use crate::name;

// ── inverted dimensions (index into add/rem; maps to a base table) ────────────────────────────────
pub const DIM_OWNER: u8 = 0;
pub const DIM_COLL: u8 = 1;
pub const DIM_SCHEMA: u8 = 2;
pub const DIM_TMPL: u8 = 3;
pub const DIM_FACET: u8 = 4;

fn table_for_dim(dim: u8) -> u32 {
    match dim {
        DIM_OWNER => TABLE_AA_BY_OWNER,
        DIM_COLL => TABLE_AA_BY_COLL,
        DIM_SCHEMA => TABLE_AA_BY_SCHEMA,
        DIM_TMPL => TABLE_AA_BY_TMPL,
        _ => TABLE_AA_DATA_ATTR,
    }
}

// ── base segment reader (mmap; same format the probe reads) ────────────────────────────────────────
struct TableLoc {
    key_count: usize,
    index_off: usize,
    blob_off: usize,
}

/// Read-only mmap over a frozen `.wseg` base segment.
pub struct BaseSeg {
    mmap: Mmap,
    tables: HashMap<u32, TableLoc>,
}

fn rdu32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn rdu64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl BaseSeg {
    pub fn open(path: &str) -> std::io::Result<BaseSeg> {
        let f = File::open(path)?;
        let mmap = unsafe { Mmap::map(&f)? };
        assert_eq!(&mmap[0..8], b"WSEG0001", "bad magic");
        let table_count = rdu32(&mmap, 16) as usize;
        let mut tables = HashMap::new();
        let mut p = 40usize;
        for _ in 0..table_count {
            let table_id = rdu32(&mmap, p);
            let key_count = rdu64(&mmap, p + 8) as usize;
            let index_off = rdu64(&mmap, p + 16) as usize;
            let blob_off = rdu64(&mmap, p + 32) as usize;
            tables.insert(
                table_id,
                TableLoc {
                    key_count,
                    index_off,
                    blob_off,
                },
            );
            p += 48;
        }
        Ok(BaseSeg { mmap, tables })
    }

    /// Binary-search a table's index for `key`, returning the blob slice (zero-copy into the mmap).
    pub fn lookup(&self, table_id: u32, key: u64) -> Option<&[u8]> {
        let t = self.tables.get(&table_id)?;
        let (mut lo, mut hi) = (0usize, t.key_count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let o = t.index_off + mid * 20;
            let k = rdu64(&self.mmap, o);
            match k.cmp(&key) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => {
                    let off = rdu64(&self.mmap, o + 8) as usize;
                    let len = rdu32(&self.mmap, o + 16) as usize;
                    return Some(&self.mmap[t.blob_off + off..t.blob_off + off + len]);
                }
            }
        }
        None
    }

    /// The single sentinel-keyed blob of a presorted ordering table (SORTED_ID / SORTED_TMPL).
    pub fn sentinel_blob(&self, table_id: u32) -> Option<&[u8]> {
        let t = self.tables.get(&table_id)?;
        if t.key_count == 0 {
            return None;
        }
        let off = rdu64(&self.mmap, t.index_off + 8) as usize;
        let len = rdu32(&self.mmap, t.index_off + 16) as usize;
        Some(&self.mmap[t.blob_off + off..t.blob_off + off + len])
    }

    pub fn base_len(&self, table_id: u32, key: u64) -> usize {
        self.lookup(table_id, key)
            .map(|b| Posting::parse(b).len())
            .unwrap_or(0)
    }

    /// Sample up to `n` keys evenly across a table's (key-sorted) index — for driving a realistic
    /// workload/mutation stream against real owners / collections / facets.
    pub fn sample_index_keys(&self, table_id: u32, n: usize) -> Vec<u64> {
        let Some(t) = self.tables.get(&table_id) else {
            return Vec::new();
        };
        let step = (t.key_count / n.max(1)).max(1);
        (0..t.key_count)
            .step_by(step)
            .map(|i| rdu64(&self.mmap, t.index_off + i * 20))
            .collect()
    }

    /// Sample up to `n` real asset_ids from the SORTED_ID ordering.
    pub fn sample_assets(&self, n: usize) -> Vec<u64> {
        let Some(b) = self.sentinel_blob(TABLE_AA_SORTED_ID) else {
            return Vec::new();
        };
        let cnt = rdu32(b, 0) as usize;
        let step = (cnt / n.max(1)).max(1);
        (0..cnt)
            .step_by(step)
            .map(|k| rdu64(b, 4 + k * 8))
            .collect()
    }

    /// Number of keys in a table (0 if absent).
    pub fn key_count(&self, table_id: u32) -> usize {
        self.tables.get(&table_id).map(|t| t.key_count).unwrap_or(0)
    }

    /// Iterate every `(key, blob)` of a table in key order — used by compaction to walk all base
    /// assets / schemas / templates.
    pub fn for_each_entry(&self, table_id: u32, mut f: impl FnMut(u64, &[u8])) {
        let Some(t) = self.tables.get(&table_id) else {
            return;
        };
        for i in 0..t.key_count {
            let o = t.index_off + i * 20;
            let key = rdu64(&self.mmap, o);
            let off = rdu64(&self.mmap, o + 8) as usize;
            let len = rdu32(&self.mmap, o + 16) as usize;
            f(key, &self.mmap[t.blob_off + off..t.blob_off + off + len]);
        }
    }
}

/// Everything needed to synthesize a realistic mint into an existing template/collection (or a setdata
/// facet move), sampled from a real base asset.
pub struct Blueprint {
    pub collection: u64,
    pub schema: u64,
    pub schema_key: u64,
    pub template_id: i32,
    pub facet_key: Option<u64>,
    pub coll_s: String,
    pub sch_s: String,
}

// ── the per-asset current state held in the overlay ───────────────────────────────────────────────
#[derive(Clone)]
pub struct AssetLive {
    pub owner: u64,
    pub collection: u64,
    pub schema: u64,
    pub template_id: i32,
    pub block_num: u32,
    pub template_mint: u32,
    /// Cached current facet (e.g. rarity) key so reads/burn don't re-hash. None if the asset has no
    /// indexed facet value.
    pub facet_key: Option<u64>,
    pub immutable: Vec<Attr>,
    pub mutable: Vec<Attr>,
}

#[derive(Clone)]
pub enum FwdState {
    Live(Box<AssetLive>),
    Tomb,
}

/// The dimension keys an asset maps to (for membership tests + leave/join).
#[derive(Clone, Copy)]
pub struct AssetKeys {
    pub owner: u64,
    pub coll: u64,
    pub schema_key: u64,
    pub tmpl_key: Option<u64>,
    pub facet_key: Option<u64>,
}
impl AssetKeys {
    fn for_dim(&self, dim: u8) -> Option<u64> {
        match dim {
            DIM_OWNER => Some(self.owner),
            DIM_COLL => Some(self.coll),
            DIM_SCHEMA => Some(self.schema_key),
            DIM_TMPL => self.tmpl_key,
            _ => self.facet_key,
        }
    }
}

// ── one applied chain delta (resolved keys; the SHiP decoder supplies the strings → hashes) ────────
#[derive(Clone)]
pub struct MintD {
    pub asset_id: u64,
    pub owner: u64,
    pub collection: u64,
    pub schema: u64,
    pub schema_key: u64,
    pub template_id: i32,
    pub facet_key: Option<u64>,
    pub immutable: Vec<Attr>,
    pub mutable: Vec<Attr>,
}
#[derive(Clone)]
pub struct TransferD {
    pub asset_id: u64,
    pub new_owner: u64,
}
#[derive(Clone)]
pub struct BurnD {
    pub asset_id: u64,
}
#[derive(Clone)]
pub struct SetDataD {
    pub asset_id: u64,
    pub mutable: Vec<Attr>,
    pub facet_old: Option<u64>,
    pub facet_new: Option<u64>,
}
#[derive(Clone)]
pub enum Delta {
    Mint(MintD),
    Transfer(TransferD),
    Burn(BurnD),
    SetData(SetDataD),
}

/// Base facts a delta needs, resolved LOCK-FREE in `prepare` so `commit` touches no mmap.
#[derive(Default)]
pub struct BaseFacts {
    live: Option<AssetLive>,
    keys: Option<AssetKeys>,
    tmpl_len: u32,
}

/// A block resolved against the base and ready to commit under a brief write lock.
pub struct Prepared {
    block: u32,
    items: Vec<(Delta, BaseFacts)>,
}

// ── the in-RAM overlay ─────────────────────────────────────────────────────────────────────────────
#[derive(Default, Clone)]
pub struct Overlay {
    pub fwd: HashMap<u64, FwdState>,
    add: HashMap<(u8, u64), RoaringTreemap>,
    rem: HashMap<(u8, u64), RoaringTreemap>,
    tomb: RoaringTreemap,
    /// template_key → number of overlay mints so far (the monotonic seq added to base_len).
    mint_seq: HashMap<u64, u32>,
    /// new mints (and never-removed): the desc browse concatenates these before the base SORTED_ID.
    sorted_id_adds: BTreeSet<u64>,
    /// (template_mint, asset_id) for overlay mints — merged into the base SORTED_TMPL slice.
    sorted_tmpl_adds: BTreeSet<(u32, u64)>,
    pub hwm_block: u32,
    pub applied: u64,
    /// Block-indexed write-ahead log; rollback/recovery replay it. (In-RAM here; one fwrite to persist.)
    wal: Vec<(u32, Delta)>,
}

impl Overlay {
    fn add_set(&mut self, dim: u8, key: u64) -> &mut RoaringTreemap {
        self.add.entry((dim, key)).or_default()
    }
    fn rem_set(&mut self, dim: u8, key: u64) -> &mut RoaringTreemap {
        self.rem.entry((dim, key)).or_default()
    }
    fn add_len(&self, dim: u8, key: u64) -> u64 {
        self.add.get(&(dim, key)).map(|s| s.len()).unwrap_or(0)
    }
    fn rem_len(&self, dim: u8, key: u64) -> u64 {
        self.rem.get(&(dim, key)).map(|s| s.len()).unwrap_or(0)
    }
    pub fn hwm_block(&self) -> u32 {
        self.hwm_block
    }
    /// Clone the serving state WITHOUT the WAL (the fold needs the state, not the log) — so a compaction
    /// snapshot is cheap and doesn't copy hundreds of MB of deltas.
    fn clone_state(&self) -> Overlay {
        Overlay {
            fwd: self.fwd.clone(),
            add: self.add.clone(),
            rem: self.rem.clone(),
            tomb: self.tomb.clone(),
            mint_seq: self.mint_seq.clone(),
            sorted_id_adds: self.sorted_id_adds.clone(),
            sorted_tmpl_adds: self.sorted_tmpl_adds.clone(),
            hwm_block: self.hwm_block,
            applied: self.applied,
            wal: Vec::new(),
        }
    }
    /// The compaction residual: WAL deltas with `block > after`, returning at most ~`max` entries but
    /// always stopping on a BLOCK boundary (so a block is never split across two drains — which would
    /// drop the tail of that block). Capping keeps the clone-under-read-lock short so a waiting writer
    /// (and the readers queued behind it) aren't stalled by a multi-million-entry copy.
    pub fn wal_after(&self, after: u32, max: usize) -> Vec<(u32, Delta)> {
        let mut out: Vec<(u32, Delta)> = Vec::new();
        let mut last: Option<u32> = None;
        for (b, d) in self.wal.iter() {
            if *b <= after {
                continue;
            }
            if out.len() >= max && last != Some(*b) {
                break; // past the cap and at a fresh block → stop on the boundary
            }
            out.push((*b, d.clone()));
            last = Some(*b);
        }
        out
    }
}

// ── the live store = immutable base + mutable overlay ──────────────────────────────────────────────
pub struct LiveSeg {
    base: BaseSeg,
    ov: RwLock<Overlay>,
    /// Configured facet field names (only "rarity" by default) — used to recompute base facet keys.
    facet_fields: Vec<String>,
    /// the largest asset_id observed (base max); new synthetic mints continue from here.
    pub base_max_id: u64,
    /// Deserialized base ROARING postings, cached per (table, key) for this segment's life (the base is
    /// immutable) — so deep cursor pages on a heavy key deserialize the bitmap once, not per page. A fresh
    /// LiveSeg (post-compaction swap) starts with an empty cache. (POC: unbounded; production = LRU-capped.)
    post_cache: Mutex<HashMap<(u32, u64), Arc<RoaringTreemap>>>,
}

impl LiveSeg {
    pub fn open(path: &str, facet_fields: Vec<String>) -> std::io::Result<LiveSeg> {
        let base = BaseSeg::open(path)?;
        // base max asset_id = first entry of SORTED_ID (descending).
        let base_max_id = base
            .sentinel_blob(TABLE_AA_SORTED_ID)
            .map(|b| if rdu32(b, 0) > 0 { rdu64(b, 4) } else { 0 })
            .unwrap_or(0);
        Ok(LiveSeg {
            base,
            ov: RwLock::new(Overlay::default()),
            facet_fields,
            base_max_id,
            post_cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn base(&self) -> &BaseSeg {
        &self.base
    }
    pub fn overlay(&self) -> parking_lot::RwLockReadGuard<'_, Overlay> {
        self.ov.read()
    }
    pub fn facet_fields(&self) -> &[String] {
        &self.facet_fields
    }

    /// Sample a base asset's template/collection/schema/facet so a synthetic mint or setdata reuses
    /// real keys (real postings, real facet hashing).
    pub fn asset_blueprint(&self, asset_id: u64) -> Option<Blueprint> {
        let b = self.base.lookup(TABLE_AA_FWD, asset_id)?;
        let a = decode_asset(b);
        let coll_s = name::decode(a.collection);
        let sch_s = name::decode(a.schema);
        let facet_key = self.facet_key_of(&coll_s, &sch_s, &a.immutable, &a.mutable);
        Some(Blueprint {
            collection: a.collection,
            schema: a.schema,
            schema_key: coll_schema_key(&coll_s, &sch_s),
            template_id: a.template_id,
            facet_key,
            coll_s,
            sch_s,
        })
    }

    /// A Live overlay asset's key for a dimension — computed entirely in RAM (no mmap touch), so read
    /// validation of changed assets is cheap.
    fn live_key(&self, a: &AssetLive, dim: u8) -> Option<u64> {
        match dim {
            DIM_OWNER => Some(a.owner),
            DIM_COLL => Some(a.collection),
            DIM_TMPL => {
                if a.template_id >= 0 {
                    Some(template_key(a.template_id as i64))
                } else {
                    None
                }
            }
            DIM_SCHEMA => Some(coll_schema_key(
                &name::decode(a.collection),
                &name::decode(a.schema),
            )),
            _ => a.facet_key,
        }
    }

    /// Decode the base forward record's dimension keys for `asset_id` (owner/coll/schema/tmpl/facet),
    /// or None if the asset isn't in the base. Used for base-membership tests + burn.
    fn base_keys(&self, asset_id: u64) -> Option<AssetKeys> {
        let b = self.base.lookup(TABLE_AA_FWD, asset_id)?;
        let a = decode_asset(b);
        Some(self.keys_from(
            a.owner,
            a.collection,
            a.schema,
            a.template_id,
            &a.immutable,
            &a.mutable,
        ))
    }

    /// Build the dimension keys from an asset's fields. schema_key + facet_key need the name strings,
    /// recovered by decoding the name-packed u64s (antelope names round-trip through name::decode).
    fn keys_from(
        &self,
        owner: u64,
        collection: u64,
        schema: u64,
        template_id: i32,
        immutable: &[Attr],
        mutable: &[Attr],
    ) -> AssetKeys {
        let coll_s = name::decode(collection);
        let sch_s = name::decode(schema);
        let _ = schema;
        let schema_key = coll_schema_key(&coll_s, &sch_s);
        let tmpl_key = if template_id >= 0 {
            Some(template_key(template_id as i64))
        } else {
            None
        };
        let facet_key = self.facet_key_of(&coll_s, &sch_s, immutable, mutable);
        AssetKeys {
            owner,
            coll: collection,
            schema_key,
            tmpl_key,
            facet_key,
        }
    }

    /// Compute the data-attr key for the first configured facet field present on the asset, by reading
    /// the schema format (to map field name → index) and the asset's stored attr value at that index.
    fn facet_key_of(
        &self,
        coll_s: &str,
        sch_s: &str,
        immutable: &[Attr],
        mutable: &[Attr],
    ) -> Option<u64> {
        let sfmt = self
            .base
            .lookup(TABLE_AA_SCHEMAS, coll_schema_key(coll_s, sch_s))?;
        let fields = decode_schema_format(sfmt);
        for fld in &self.facet_fields {
            let Some(idx) = fields.iter().position(|(n, _)| n == fld) else {
                continue;
            };
            let idx = idx as u8;
            // immutable-first, matching the base builder's by_data (aa_builder.rs push_asset) exactly —
            // so the overlay's facet key for an asset equals the one the frozen base indexed it under.
            let val = immutable
                .iter()
                .find(|(i, _)| *i == idx)
                .or_else(|| mutable.iter().find(|(i, _)| *i == idx));
            if let Some((_, v)) = val {
                if !v.is_empty() && v.len() <= 64 {
                    return Some(data_attr_key(coll_s, sch_s, fld, v));
                }
            }
        }
        None
    }

    // ── APPLY: two-phase. prepare() does ALL base mmap reads LOCK-FREE; commit() mutates the overlay
    //    under a brief write lock (pure in-RAM), so the write-lock hold — and thus the freshness lag —
    //    is microseconds and never blocks readers on a cold page fault. ───────────────────────────
    /// Resolve a block's base facts (the only mmap-touching step) without taking any lock.
    pub fn prepare_block(&self, block: u32, deltas: &[Delta]) -> Prepared {
        let items = deltas
            .iter()
            .map(|d| (d.clone(), self.resolve(d)))
            .collect();
        Prepared { block, items }
    }

    fn resolve(&self, d: &Delta) -> BaseFacts {
        let mut f = BaseFacts::default();
        match d {
            Delta::Mint(m) => {
                if m.template_id >= 0 {
                    f.tmpl_len = self
                        .base
                        .base_len(TABLE_AA_BY_TMPL, template_key(m.template_id as i64))
                        as u32;
                }
            }
            Delta::Transfer(t) => f.live = self.decode_base_live(t.asset_id),
            Delta::Burn(b) => f.keys = self.base_keys(b.asset_id),
            Delta::SetData(s) => {
                f.live = self.decode_base_live(s.asset_id);
                f.keys = self.base_keys(s.asset_id);
            }
        }
        f
    }

    /// Pre-size the overlay's forward map + WAL so growth reallocs don't land inside a timed commit.
    /// (In production the WAL is a disk append — no giant in-RAM realloc — and the forward map is sized
    /// to the expected post-LIB working set; this mirrors that.)
    pub fn reserve(&self, mutations: usize) {
        let mut ov = self.ov.write();
        ov.fwd.reserve(mutations);
        ov.wal.reserve(mutations);
    }

    /// Apply a prepared block under a brief write lock (pure in-RAM — no mmap).
    pub fn commit_block(&self, prepared: Prepared) {
        let mut ov = self.ov.write();
        self.commit_into(&mut ov, prepared);
    }

    /// Apply a prepared block into an already-held overlay guard (lets the server swap-coordinate the
    /// commit: take the write lock, verify the store still points here, then commit — all atomically).
    pub fn commit_into(&self, ov: &mut Overlay, prepared: Prepared) {
        let block = prepared.block;
        for (d, facts) in prepared.items {
            self.apply_resolved(ov, block, d, facts);
        }
        if block > ov.hwm_block {
            ov.hwm_block = block;
        }
    }

    /// Acquire the overlay write lock directly (the server holds it across a ptr-eq swap check).
    pub fn write_overlay(&self) -> parking_lot::RwLockWriteGuard<'_, Overlay> {
        self.ov.write()
    }

    /// Construct a fresh LiveSeg on an already-open base (the compacted segment) with an empty overlay.
    pub fn from_base(base: BaseSeg, facet_fields: Vec<String>) -> LiveSeg {
        let base_max_id = base
            .sentinel_blob(TABLE_AA_SORTED_ID)
            .map(|b| if rdu32(b, 0) > 0 { rdu64(b, 4) } else { 0 })
            .unwrap_or(0);
        LiveSeg {
            base,
            ov: RwLock::new(Overlay::default()),
            facet_fields,
            base_max_id,
            post_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot the overlay's serving state (WAL excluded) + its high-water block, for a lock-free fold.
    pub fn snapshot_overlay(&self) -> (u32, Overlay) {
        let ov = self.ov.read();
        (ov.hwm_block, ov.clone_state())
    }

    /// The compaction residual: up to ~`max` WAL deltas with `block > after` (clone under the read lock).
    pub fn wal_after(&self, after: u32, max: usize) -> Vec<(u32, Delta)> {
        self.ov.read().wal_after(after, max)
    }

    /// Convenience: prepare (lock-free) + commit (locked) in one call. Used by the single-thread
    /// phases + tests; the base reads still happen before the lock is acquired.
    pub fn apply_block(&self, block: u32, deltas: &[Delta]) {
        self.commit_block(self.prepare_block(block, deltas));
    }

    fn apply_resolved(&self, ov: &mut Overlay, block: u32, d: Delta, facts: BaseFacts) {
        ov.wal.push((block, d.clone()));
        ov.applied += 1;
        match d {
            Delta::Mint(m) => self.do_mint(ov, block, m, facts.tmpl_len),
            Delta::Transfer(t) => self.do_transfer(ov, block, t, facts.live),
            Delta::Burn(b) => self.do_burn(ov, b, facts.keys),
            Delta::SetData(s) => {
                self.do_setdata(ov, s, facts.live, facts.keys.and_then(|k| k.facet_key))
            }
        }
    }

    fn do_mint(&self, ov: &mut Overlay, block: u32, m: MintD, base_tmpl_len: u32) {
        let tmpl_key = if m.template_id >= 0 {
            Some(template_key(m.template_id as i64))
        } else {
            None
        };
        // template_mint = dense base max (immutable full_count, never shrinks) + monotonic overlay seq.
        let template_mint = if let Some(tk) = tmpl_key {
            let seq = ov.mint_seq.entry(tk).or_insert(0);
            *seq += 1;
            base_tmpl_len + *seq
        } else {
            0
        };
        ov.fwd.insert(
            m.asset_id,
            FwdState::Live(Box::new(AssetLive {
                owner: m.owner,
                collection: m.collection,
                schema: m.schema,
                template_id: m.template_id,
                block_num: block,
                template_mint,
                facet_key: m.facet_key,
                immutable: m.immutable,
                mutable: m.mutable,
            })),
        );
        // additions to every dimension (a new id is never a base member → always `add`)
        ov.add_set(DIM_OWNER, m.owner).insert(m.asset_id);
        ov.add_set(DIM_COLL, m.collection).insert(m.asset_id);
        ov.add_set(DIM_SCHEMA, m.schema_key).insert(m.asset_id);
        if let Some(tk) = tmpl_key {
            ov.add_set(DIM_TMPL, tk).insert(m.asset_id);
            ov.sorted_tmpl_adds.insert((template_mint, m.asset_id));
        }
        if let Some(fk) = m.facet_key {
            ov.add_set(DIM_FACET, fk).insert(m.asset_id);
        }
        ov.sorted_id_adds.insert(m.asset_id);
    }

    fn do_transfer(
        &self,
        ov: &mut Overlay,
        block: u32,
        t: TransferD,
        base_live: Option<AssetLive>,
    ) {
        let from = match ov.fwd.get(&t.asset_id) {
            Some(FwdState::Tomb) => return,
            Some(FwdState::Live(a)) => a.owner,
            None => match &base_live {
                Some(a) => a.owner,
                None => return, // unknown asset
            },
        };
        if from == t.new_owner {
            return;
        }
        let base_owner = base_live.as_ref().map(|a| a.owner);
        self.leave(ov, DIM_OWNER, from, t.asset_id, base_owner == Some(from));
        self.join(
            ov,
            DIM_OWNER,
            t.new_owner,
            t.asset_id,
            base_owner == Some(t.new_owner),
        );
        self.promote(ov, t.asset_id, base_live);
        if let Some(FwdState::Live(a)) = ov.fwd.get_mut(&t.asset_id) {
            a.owner = t.new_owner;
            a.block_num = block;
        }
    }

    fn do_burn(&self, ov: &mut Overlay, b: BurnD, base_keys: Option<AssetKeys>) {
        let id = b.asset_id;
        // current dimension keys: the overlay record (in-RAM) if present, else the resolved base keys.
        let keys = match ov.fwd.get(&id) {
            Some(FwdState::Tomb) => None,
            Some(FwdState::Live(a)) => Some(self.live_keys(a)),
            None => base_keys,
        };
        if let Some(k) = keys {
            for dim in [DIM_OWNER, DIM_COLL, DIM_SCHEMA, DIM_TMPL, DIM_FACET] {
                if let Some(key) = k.for_dim(dim) {
                    let was_base = base_keys.and_then(|b| b.for_dim(dim)) == Some(key);
                    self.leave(ov, dim, key, id, was_base);
                }
            }
        }
        ov.sorted_id_adds.remove(&id);
        if let Some(FwdState::Live(a)) = ov.fwd.get(&id) {
            ov.sorted_tmpl_adds.remove(&(a.template_mint, id));
        }
        ov.tomb.insert(id);
        ov.fwd.insert(id, FwdState::Tomb);
    }

    fn do_setdata(
        &self,
        ov: &mut Overlay,
        s: SetDataD,
        base_live: Option<AssetLive>,
        base_facet: Option<u64>,
    ) {
        self.promote(ov, s.asset_id, base_live);
        // Derive the OLD facet from our own current forward record, not the delta's `facet_old` (which a
        // real SHiP stream — or a second setdata on the same asset — can render stale). This keeps the
        // overlay's facet membership exactly consistent with what compaction folds (a.facet_key).
        // A setdata for a tombstoned/unknown asset is a no-op (else it would add a phantom to a facet).
        let cur_facet = match ov.fwd.get(&s.asset_id) {
            Some(FwdState::Live(a)) => a.facet_key,
            _ => return,
        };
        if cur_facet != s.facet_new {
            if let Some(old) = cur_facet {
                self.leave(ov, DIM_FACET, old, s.asset_id, base_facet == Some(old));
            }
            if let Some(new) = s.facet_new {
                self.join(ov, DIM_FACET, new, s.asset_id, base_facet == Some(new));
            }
        }
        if let Some(FwdState::Live(a)) = ov.fwd.get_mut(&s.asset_id) {
            a.mutable = s.mutable;
            a.facet_key = s.facet_new;
        }
    }

    /// A Live overlay asset's full dimension keys — entirely in RAM (schema_key via name::decode, facet
    /// from the cached `facet_key`); no mmap, so it's safe to call under the commit write lock.
    fn live_keys(&self, a: &AssetLive) -> AssetKeys {
        AssetKeys {
            owner: a.owner,
            coll: a.collection,
            schema_key: coll_schema_key(&name::decode(a.collection), &name::decode(a.schema)),
            tmpl_key: if a.template_id >= 0 {
                Some(template_key(a.template_id as i64))
            } else {
                None
            },
            facet_key: a.facet_key,
        }
    }

    /// Decode a base forward record into an owned AssetLive (lock-free; used by resolve()).
    fn decode_base_live(&self, asset_id: u64) -> Option<AssetLive> {
        let b = self.base.lookup(TABLE_AA_FWD, asset_id)?;
        let a = decode_asset(b);
        let coll_s = name::decode(a.collection);
        let sch_s = name::decode(a.schema);
        let facet_key = self.facet_key_of(&coll_s, &sch_s, &a.immutable, &a.mutable);
        Some(AssetLive {
            owner: a.owner,
            collection: a.collection,
            schema: a.schema,
            template_id: a.template_id,
            block_num: a.block_num,
            template_mint: a.template_mint,
            facet_key,
            immutable: a.immutable,
            mutable: a.mutable,
        })
    }

    /// Ensure `asset_id` has a Live forward record (promote the already-resolved base record — no mmap).
    fn promote(&self, ov: &mut Overlay, asset_id: u64, base_live: Option<AssetLive>) {
        if ov.fwd.contains_key(&asset_id) {
            return;
        }
        if let Some(a) = base_live {
            ov.fwd.insert(asset_id, FwdState::Live(Box::new(a)));
        }
    }

    fn leave(&self, ov: &mut Overlay, dim: u8, key: u64, id: u64, was_base_member: bool) {
        if was_base_member {
            ov.rem_set(dim, key).insert(id);
        } else {
            ov.add_set(dim, key).remove(id);
        }
    }
    fn join(&self, ov: &mut Overlay, dim: u8, key: u64, id: u64, is_base_member: bool) {
        if is_base_member {
            ov.rem_set(dim, key).remove(id);
        } else {
            ov.add_set(dim, key).insert(id);
        }
    }

    // ── READ (merge base + overlay) ─────────────────────────────────────────────────────────────
    /// Point lookup: returns the current owner (the bench sink), or None if tombstoned/missing.
    pub fn point_owner(&self, ov: &Overlay, asset_id: u64) -> Option<u64> {
        match ov.fwd.get(&asset_id) {
            Some(FwdState::Tomb) => None,
            Some(FwdState::Live(a)) => Some(a.owner),
            None => self
                .base
                .lookup(TABLE_AA_FWD, asset_id)
                .map(|b| decode_asset(b).owner),
        }
    }

    /// True if `asset_id` is live (not burned, exists in base or overlay).
    pub fn exists(&self, ov: &Overlay, asset_id: u64) -> bool {
        match ov.fwd.get(&asset_id) {
            Some(FwdState::Tomb) => false,
            Some(FwdState::Live(_)) => true,
            None => self.base.lookup(TABLE_AA_FWD, asset_id).is_some(),
        }
    }

    /// Page 1 (newest-first, up to `n`) of a dimension key, merging overlay adds with the base head and
    /// re-validating every candidate against the live forward view. Falls back to a full materialize
    /// only when the dense head can't fill the page (rare: a key whose newest 256 mostly moved away).
    pub fn page(&self, ov: &Overlay, dim: u8, key: u64, n: usize) -> Vec<u64> {
        let table = table_for_dim(dim);
        let mut cand: Vec<u64> = Vec::with_capacity(512);
        if let Some(s) = ov.add.get(&(dim, key)) {
            cand.extend(s.iter());
        }
        if let Some(b) = self.base.lookup(table, key) {
            Posting::parse(b).head_for_each(256, |id| cand.push(id));
        }
        cand.sort_unstable_by(|a, b| b.cmp(a)); // desc
        cand.dedup();
        let mut out = Vec::with_capacity(n);
        for &id in &cand {
            if out.len() >= n {
                break;
            }
            // Validation is IN-RAM only: a base-head candidate absent from the overlay is, by the very
            // meaning of the base posting, still a member of `key` (unchanged) → valid with one HashMap
            // probe. Only overlay-touched assets need a field compare (also in RAM). No base decode.
            match ov.fwd.get(&id) {
                None => out.push(id),
                Some(FwdState::Tomb) => {}
                Some(FwdState::Live(a)) => {
                    if self.live_key(a, dim) == Some(key) {
                        out.push(id);
                    }
                }
            }
        }
        if out.len() < n {
            let base_total = self.base.base_len(table, key);
            let add_n = ov.add_len(dim, key) as usize;
            let rem_n = ov.rem_len(dim, key) as usize;
            // members beyond the 256 head exist → the page genuinely needs a deeper scan.
            if base_total + add_n > 256 + rem_n {
                return self.materialize(ov, dim, key, n);
            }
        }
        out
    }

    /// Correctness fallback / deep-page path: materialize the full effective set once, validated + desc.
    fn materialize(&self, ov: &Overlay, dim: u8, key: u64, n: usize) -> Vec<u64> {
        let table = table_for_dim(dim);
        let mut set: RoaringTreemap = self
            .base
            .lookup(table, key)
            .map(|b| Posting::parse(b).to_roaring())
            .unwrap_or_default();
        if let Some(a) = ov.add.get(&(dim, key)) {
            set |= a;
        }
        if let Some(r) = ov.rem.get(&(dim, key)) {
            set -= r;
        }
        set -= &ov.tomb;
        // (base ∪ add) − rem − tomb is already the EXACT current membership of `key` (the apply path
        // keeps add/rem precise), so no per-id validation is needed here — just take the newest n.
        let mut ids: Vec<u64> = set.iter().collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids.truncate(n);
        ids
    }

    /// In-RAM membership test for a candidate id under `key` (no base decode): a candidate absent from
    /// the overlay is still a member of its base key; an overlay-touched one needs a field compare.
    fn is_member(&self, ov: &Overlay, dim: u8, key: u64, id: u64) -> bool {
        match ov.fwd.get(&id) {
            None => true,
            Some(FwdState::Tomb) => false,
            Some(FwdState::Live(a)) => self.live_key(a, dim) == Some(key),
        }
    }

    /// The deserialized base ROARING posting for (table, key), cached for this segment's life (the base
    /// is immutable). Deserializes outside the lock; a racing thread's copy is discarded.
    fn base_roaring(&self, table: u32, key: u64) -> Arc<RoaringTreemap> {
        if let Some(rt) = self.post_cache.lock().get(&(table, key)) {
            return rt.clone();
        }
        let rt = Arc::new(
            self.base
                .lookup(table, key)
                .map(|b| Posting::parse(b).to_roaring())
                .unwrap_or_default(),
        );
        self.post_cache
            .lock()
            .entry((table, key))
            .or_insert(rt)
            .clone()
    }

    /// The largest `want` BASE ids strictly less than `cursor`, descending. RAW = zero-copy binary-search
    /// + walk-back; ROARING = rank/select on the cached bitmap — bounded, not a per-page full scan.
    fn base_below(&self, table: u32, key: u64, cursor: u64, want: usize) -> Vec<u64> {
        let Some(blob) = self.base.lookup(table, key) else {
            return Vec::new();
        };
        match Posting::parse(blob) {
            Posting::Raw(pl) => {
                // lower_bound: first index whose value >= cursor; everything before it is < cursor.
                let (mut lo, mut hi) = (0usize, pl.len);
                while lo < hi {
                    let m = (lo + hi) / 2;
                    if pl.get(m) < cursor {
                        lo = m + 1;
                    } else {
                        hi = m;
                    }
                }
                let mut out = Vec::with_capacity(want.min(lo));
                let mut i = lo;
                while i > 0 && out.len() < want {
                    i -= 1;
                    out.push(pl.get(i)); // strictly < cursor, descending
                }
                out
            }
            Posting::Roaring { .. } => {
                let rt = self.base_roaring(table, key);
                // r = #values < cursor; select(r-1) is the largest < cursor, walking down = descending.
                let r = rt.rank(cursor.saturating_sub(1));
                let mut out = Vec::with_capacity(want);
                let mut k = r;
                while k > 0 && out.len() < want {
                    k -= 1;
                    if let Some(v) = rt.select(k) {
                        out.push(v);
                    }
                }
                out
            }
        }
    }

    /// Candidate ids (base ∪ overlay-add) strictly below `cursor`, descending + deduped, up to `want`.
    fn candidates_below(
        &self,
        ov: &Overlay,
        dim: u8,
        table: u32,
        key: u64,
        cursor: u64,
        want: usize,
    ) -> Vec<u64> {
        let mut cand = self.base_below(table, key, cursor, want);
        if let Some(s) = ov.add.get(&(dim, key)) {
            for v in s.iter() {
                if v < cursor {
                    cand.push(v);
                }
            }
        }
        cand.sort_unstable_by(|a, b| b.cmp(a));
        cand.dedup();
        cand.truncate(want);
        cand
    }

    /// CURSOR PAGINATION — the next `n` live members of `key`, newest-first, with asset_id < `after`
    /// (`after = None` → the newest page). Stable under overlay growth (asset_ids never move), and
    /// O(page) on a hot key — RAW walk-back / ROARING rank+select on the cached bitmap — instead of the
    /// per-page full materialize. The returned slice's last id is the cursor for the next page.
    pub fn page_after(
        &self,
        ov: &Overlay,
        dim: u8,
        key: u64,
        after: Option<u64>,
        n: usize,
    ) -> Vec<u64> {
        let table = table_for_dim(dim);
        let mut out = Vec::with_capacity(n);
        let mut cursor = after.unwrap_or(u64::MAX);
        // pull desc candidates below the cursor in bounded chunks, validating, until n live or exhausted.
        loop {
            let want = (n - out.len()) * 2 + 64;
            let chunk = self.candidates_below(ov, dim, table, key, cursor, want);
            if chunk.is_empty() {
                break;
            }
            let exhausted = chunk.len() < want;
            for &id in &chunk {
                if out.len() >= n {
                    break;
                }
                if self.is_member(ov, dim, key, id) {
                    out.push(id);
                }
            }
            if out.len() >= n || exhausted {
                break;
            }
            cursor = *chunk.last().unwrap(); // continue strictly below the smallest candidate seen
        }
        out
    }

    /// Live count of a dimension key: immutable base count + overlay add − overlay rem.
    pub fn count(&self, ov: &Overlay, dim: u8, key: u64) -> i64 {
        self.base.base_len(table_for_dim(dim), key) as i64 + ov.add_len(dim, key) as i64
            - ov.rem_len(dim, key) as i64
    }

    /// Browse page (newest asset_ids desc): overlay mints first (strictly larger ids), then the base
    /// SORTED_ID slice, skipping tombstones. Returns up to `n` starting at logical offset `skip`.
    pub fn browse(&self, ov: &Overlay, skip: usize, n: usize) -> Vec<u64> {
        let mut out = Vec::with_capacity(n);
        let mut seen = 0usize;
        for &id in ov.sorted_id_adds.iter().rev() {
            if ov.tomb.contains(id) {
                continue;
            }
            if seen >= skip {
                out.push(id);
                if out.len() >= n {
                    return out;
                }
            }
            seen += 1;
        }
        if let Some(b) = self.base.sentinel_blob(TABLE_AA_SORTED_ID) {
            let cnt = rdu32(b, 0) as usize;
            for k in 0..cnt {
                let id = rdu64(b, 4 + k * 8);
                if ov.tomb.contains(id) {
                    continue;
                }
                if seen >= skip {
                    out.push(id);
                    if out.len() >= n {
                        break;
                    }
                }
                seen += 1;
            }
        }
        out
    }

    // ── COMPACTION: fold the immutable base + the live overlay into a fresh segment, so the overlay's
    //    RAM is reclaimed and the new base reflects every applied mutation. Reuses AtomicBuilder.finish()
    //    (posting hybrid selection, template_mint re-rank, sorted_id/sorted_tmpl) over the merged current
    //    state. Reads keep serving from the OLD (base + overlay) until the caller atomically swaps in a
    //    fresh LiveSeg on the new segment (ArcSwap in a server). ───────────────────────────────────────
    pub fn compact(&self, out: &str) -> std::io::Result<AaStats> {
        let ov = self.ov.read();
        self.compact_with(&ov, out)
    }

    /// Fold `self.base` + a GIVEN overlay snapshot into `out`. The server folds from a lock-free clone
    /// (`snapshot_overlay`) so the SHiP applier keeps writing to the live overlay during the minutes-long
    /// fold; reads keep serving from the current LiveSeg until the atomic ArcSwap.
    pub fn compact_with(&self, ov: &Overlay, out: &str) -> std::io::Result<AaStats> {
        let facet_field = self.facet_fields.first().cloned().unwrap_or_default();

        // rarity field index per schema (so the asset loop doesn't re-decode the schema 232M times).
        let mut facet_idx: HashMap<u64, u8> = HashMap::new();
        self.base.for_each_entry(TABLE_AA_SCHEMAS, |key, blob| {
            let fields = decode_schema_format(blob);
            if let Some(p) = fields.iter().position(|(n, _)| *n == facet_field) {
                facet_idx.insert(key, p as u8);
            }
        });

        let mut b = AtomicBuilder::new(self.facet_fields.clone());
        // schemas + templates carry straight over from the base (overlay add/extend would also fold here)
        self.base.for_each_entry(TABLE_AA_SCHEMAS, |key, blob| {
            b.push_schema_raw(key, &decode_schema_format(blob));
        });
        self.base.for_each_entry(TABLE_AA_TMPL_FWD, |_k, blob| {
            let t = decode_template(blob);
            b.push_template_raw(
                t.template_id,
                t.schema,
                t.transferable,
                t.burnable,
                t.max_supply,
                t.issued_supply,
                &t.immutable,
            );
        });
        // collection forward records carry over too (v2 TABLE_AA_COLL_FWD) — else compaction drops them.
        self.base.for_each_entry(TABLE_AA_COLL_FWD, |_k, blob| {
            let c = decode_collection(blob);
            b.push_collection_raw(
                c.collection,
                c.author,
                c.allow_notify,
                &c.authorized,
                &c.notify,
                c.market_fee,
                &c.data,
            );
        });

        // every CURRENT asset, exactly once: base assets (overlay-current, tombstones dropped) …
        self.base
            .for_each_entry(TABLE_AA_FWD, |aid, blob| match ov.fwd.get(&aid) {
                Some(FwdState::Tomb) => {} // burned → drop from the new base
                // overlay-current asset → use its cached facet key (what the overlay indexed it under)
                Some(FwdState::Live(a)) => {
                    fold_emit(&mut b, &facet_idx, &facet_field, aid, a, a.facet_key)
                }
                None => {
                    let a = decode_asset(blob);
                    let live = AssetLive {
                        owner: a.owner,
                        collection: a.collection,
                        schema: a.schema,
                        template_id: a.template_id,
                        block_num: a.block_num,
                        template_mint: a.template_mint,
                        facet_key: None,
                        immutable: a.immutable,
                        mutable: a.mutable,
                    };
                    fold_emit(&mut b, &facet_idx, &facet_field, aid, &live, None);
                }
            });
        // … then the overlay mints (Live records with no base row).
        for (&aid, st) in ov.fwd.iter() {
            if let FwdState::Live(a) = st {
                if self.base.lookup(TABLE_AA_FWD, aid).is_none() {
                    fold_emit(&mut b, &facet_idx, &facet_field, aid, a, a.facet_key);
                }
            }
        }
        b.finish(out)
    }

    // ── fork rollback / recovery: replay the WAL up to `block` (clears derived state, re-applies) ──
    pub fn rollback_to(&self, block: u32) {
        let mut ov = self.ov.write();
        let wal = std::mem::take(&mut ov.wal);
        *ov = Overlay::default();
        for (b, d) in wal {
            if b <= block {
                // rollback is rare/offline → resolving base facts under the lock is fine here.
                let facts = self.resolve(&d);
                self.apply_resolved(&mut ov, b, d, facts);
            }
        }
        ov.hwm_block = block;
    }

    /// Overlay size accounting (for the RAM-vs-mutations measurement).
    pub fn overlay_stats(&self) -> (usize, usize, usize, u64, u64) {
        let ov = self.ov.read();
        let add_keys = ov.add.len();
        let rem_keys = ov.rem.len();
        let fwd = ov.fwd.len();
        let tomb = ov.tomb.len();
        (fwd, add_keys, rem_keys, ov.applied, tomb)
    }

    /// Analytic overlay HEAP bytes (the actual serving structures), separated from the WAL (which is
    /// disk-resident in production). This is the real RAM cost — independent of the base mmap page
    /// cache that process RSS also counts.
    pub fn overlay_heap_bytes(&self) -> (u64, u64) {
        let ov = self.ov.read();
        let mut serving: u64 = 0;
        for st in ov.fwd.values() {
            serving += 24; // HashMap<u64, FwdState> entry (key + enum tag + ptr), approx
            if let FwdState::Live(a) = st {
                serving += std::mem::size_of::<AssetLive>() as u64 + 16; // struct + Box
                for (_, v) in a.immutable.iter().chain(a.mutable.iter()) {
                    serving += v.len() as u64 + 4;
                }
            }
        }
        for s in ov.add.values().chain(ov.rem.values()) {
            serving += s.serialized_size() as u64 + 48; // bitmap bytes + HashMap entry overhead
        }
        serving += ov.tomb.serialized_size() as u64;
        serving += (ov.sorted_id_adds.len() * 40 + ov.sorted_tmpl_adds.len() * 48) as u64;
        serving += (ov.mint_seq.len() * 24) as u64;
        let mut wal: u64 = 0;
        for (_, d) in &ov.wal {
            wal += match d {
                Delta::Mint(m) => {
                    64 + m
                        .immutable
                        .iter()
                        .chain(m.mutable.iter())
                        .map(|(_, v)| v.len() as u64 + 4)
                        .sum::<u64>()
                }
                Delta::SetData(s) => {
                    48 + s
                        .mutable
                        .iter()
                        .map(|(_, v)| v.len() as u64 + 4)
                        .sum::<u64>()
                }
                _ => 24,
            };
        }
        (serving, wal)
    }
}

/// Emit one current asset into the compaction builder. The facet key is the overlay's cached one when
/// available (overlay-touched assets), else recomputed from the asset's attrs via the schema's rarity
/// field index (base-unchanged assets).
fn fold_emit(
    b: &mut AtomicBuilder,
    facet_idx: &HashMap<u64, u8>,
    facet_field: &str,
    aid: u64,
    a: &AssetLive,
    cached_facet: Option<u64>,
) {
    let coll_s = name::decode(a.collection);
    let sch_s = name::decode(a.schema);
    let schema_key = coll_schema_key(&coll_s, &sch_s);
    let facet = cached_facet.or_else(|| {
        facet_idx.get(&schema_key).and_then(|&idx| {
            // immutable-first, matching the base builder's by_data (aa_builder.rs push_asset).
            a.immutable
                .iter()
                .find(|(i, _)| *i == idx)
                .or_else(|| a.mutable.iter().find(|(i, _)| *i == idx))
                .and_then(|(_, v)| {
                    (!v.is_empty() && v.len() <= 64)
                        .then(|| data_attr_key(&coll_s, &sch_s, facet_field, v))
                })
        })
    });
    let fk = facet.map(|k| [k]);
    let fks: &[u64] = fk.as_ref().map(|s| s.as_slice()).unwrap_or(&[]);
    b.push_asset_raw(
        aid,
        a.owner,
        a.collection,
        a.schema,
        schema_key,
        a.template_id,
        a.block_num,
        &a.immutable,
        &a.mutable,
        fks,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    /// Build a tiny base segment to a temp path and open it as a LiveSeg.
    fn tiny_base(path: &str) -> LiveSeg {
        let mut b = AtomicBuilder::new(vec!["rarity".to_string()]);
        b.push(
            "atomicassets-schemas",
            &doc! { "collection_name": "col", "schema_name": "sch",
            "format": [ {"name":"name","type":"string"}, {"name":"rarity","type":"string"} ] },
        );
        b.push(
            "atomicassets-templates",
            &doc! { "collection_name": "col", "schema_name": "sch", "template_id": 7i32,
            "immutable_data": { "name": "Hero" } },
        );
        // three base assets owned by alice, two Mythic one Common
        for (aid, rar) in [(1000u64, "Mythic"), (1001, "Mythic"), (1002, "Common")] {
            b.push(
                "atomicassets-assets",
                &doc! { "collection_name": "col", "schema_name": "sch", "owner": "alice",
                "asset_id": aid.to_string(), "template_id": 7i32, "block_num": 100i64,
                "immutable_data": {}, "mutable_data": { "rarity": rar } },
            );
        }
        b.finish(path).unwrap();
        LiveSeg::open(path, vec!["rarity".to_string()]).unwrap()
    }

    fn fkey(field: &str, val: &str) -> u64 {
        data_attr_key("col", "sch", field, val)
    }

    /// Base with `n` assets owned by `alice` in collection "col" (n>512 → a ROARING by_owner posting).
    fn big_base(path: &str, first: u64, n: u64) -> LiveSeg {
        let mut b = AtomicBuilder::new(vec!["rarity".to_string()]);
        b.push(
            "atomicassets-schemas",
            &doc! { "collection_name": "col", "schema_name": "sch",
            "format": [ {"name":"name","type":"string"}, {"name":"rarity","type":"string"} ] },
        );
        b.push(
            "atomicassets-templates",
            &doc! { "collection_name": "col", "schema_name": "sch", "template_id": 7i32,
            "immutable_data": { "name": "Hero" } },
        );
        for i in 0..n {
            b.push(
                "atomicassets-assets",
                &doc! { "collection_name": "col", "schema_name": "sch", "owner": "alice",
                "asset_id": (first + i).to_string(), "template_id": 7i32, "block_num": 100i64,
                "immutable_data": {}, "mutable_data": { "rarity": "Mythic" } },
            );
        }
        b.finish(path).unwrap();
        LiveSeg::open(path, vec!["rarity".to_string()]).unwrap()
    }

    /// Page through every page of a key via the cursor, asserting strict global descending order (which
    /// also proves no duplicates and no gaps), and return the full id list.
    fn drain_cursor(live: &LiveSeg, ov: &Overlay, dim: u8, key: u64, page: usize) -> Vec<u64> {
        let mut all = Vec::new();
        let mut cursor = None;
        loop {
            let pg = live.page_after(ov, dim, key, cursor, page);
            if pg.is_empty() {
                break;
            }
            assert!(pg.windows(2).all(|w| w[0] > w[1]), "page is strictly desc");
            all.extend_from_slice(&pg);
            cursor = Some(*pg.last().unwrap());
            if pg.len() < page {
                break;
            }
        }
        assert!(
            all.windows(2).all(|w| w[0] > w[1]),
            "globally desc → no dupes, no gaps"
        );
        all
    }

    #[test]
    fn cursor_pages_roaring_full_set_then_survives_mutations() {
        let path = std::env::temp_dir()
            .join("aa_live_cursor.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let first = 1_000_000u64;
        let n = 1000u64; // > RAW_MAX (512) → the by_owner posting is ROARING, exercising rank/select
        let live = big_base(&path, first, n);
        let alice = name::encode("alice");
        let bob = name::encode("bob");

        // 1) cursor walks the entire ROARING posting, newest-first, no gaps/dupes.
        {
            let ov = live.overlay();
            let all = drain_cursor(&live, &ov, DIM_OWNER, alice, 100);
            assert_eq!(all.len(), 1000);
            assert_eq!(all[0], first + 999, "newest first");
            assert_eq!(*all.last().unwrap(), first, "oldest last");
        }

        // 2) mutate: transfer one out, burn one, mint one in — then re-page.
        let minted = live.base_max_id + 1;
        live.apply_block(
            101,
            &[
                Delta::Transfer(TransferD {
                    asset_id: first + 500,
                    new_owner: bob,
                }),
                Delta::Burn(BurnD {
                    asset_id: first + 501,
                }),
                Delta::Mint(MintD {
                    asset_id: minted,
                    owner: alice,
                    collection: name::encode("col"),
                    schema: name::encode("sch"),
                    schema_key: coll_schema_key("col", "sch"),
                    template_id: 7,
                    facet_key: Some(fkey("rarity", "Mythic")),
                    immutable: vec![],
                    mutable: vec![(1u8, "Mythic".to_string())],
                }),
            ],
        );
        let ov = live.overlay();
        let all = drain_cursor(&live, &ov, DIM_OWNER, alice, 100);
        assert_eq!(all.len(), 1000 - 2 + 1, "−transfer −burn +mint");
        assert_eq!(
            all[0], minted,
            "the mint is newest → first page, first slot"
        );
        assert!(
            !all.contains(&(first + 500)),
            "transferred-out gone from alice"
        );
        assert!(!all.contains(&(first + 501)), "burned gone");
        assert_eq!(
            live.count(&ov, DIM_OWNER, alice) as usize,
            all.len(),
            "count == drained"
        );
        // the transferred asset shows up under bob via the cursor
        assert_eq!(
            live.page_after(&ov, DIM_OWNER, bob, None, 100),
            vec![first + 500]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn cursor_matches_offset_on_raw() {
        // small (RAW) posting: cursor pagination must agree with the head-based page() on page 1.
        let path = std::env::temp_dir()
            .join("aa_live_cursor_raw.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = big_base(&path, 2_000_000, 50); // 50 ≤ RAW_MAX → RAW posting
        let alice = name::encode("alice");
        let ov = live.overlay();
        let all = drain_cursor(&live, &ov, DIM_OWNER, alice, 10);
        assert_eq!(all.len(), 50);
        // page 1 of the cursor == the newest 10, matching page()'s head.
        let p1 = live.page_after(&ov, DIM_OWNER, alice, None, 10);
        let head = live.page(&ov, DIM_OWNER, alice, 10);
        let mut head_sorted = head.clone();
        head_sorted.sort_unstable_by(|a, b| b.cmp(a));
        assert_eq!(p1, head_sorted, "cursor page-1 == page() head (desc)");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn transfer_moves_owner_and_counts() {
        let path = std::env::temp_dir()
            .join("aa_live_transfer.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = tiny_base(&path);
        let alice = name::encode("alice");
        let bob = name::encode("bob");

        // before: alice owns 3, bob 0
        {
            let ov = live.overlay();
            assert_eq!(live.count(&ov, DIM_OWNER, alice), 3);
            assert_eq!(live.count(&ov, DIM_OWNER, bob), 0);
            assert!(live.page(&ov, DIM_OWNER, alice, 100).contains(&1000));
        }
        // transfer asset 1000 alice → bob
        live.apply_block(
            101,
            &[Delta::Transfer(TransferD {
                asset_id: 1000,
                new_owner: bob,
            })],
        );
        {
            let ov = live.overlay();
            assert_eq!(live.count(&ov, DIM_OWNER, alice), 2, "alice loses one");
            assert_eq!(live.count(&ov, DIM_OWNER, bob), 1, "bob gains one");
            assert!(
                !live.page(&ov, DIM_OWNER, alice, 100).contains(&1000),
                "gone from alice"
            );
            assert!(
                live.page(&ov, DIM_OWNER, bob, 100).contains(&1000),
                "now under bob"
            );
            assert_eq!(live.point_owner(&ov, 1000), Some(bob));
        }
        // round-trip bob → alice: counts return exactly (idempotent set-deltas)
        live.apply_block(
            102,
            &[Delta::Transfer(TransferD {
                asset_id: 1000,
                new_owner: alice,
            })],
        );
        {
            let ov = live.overlay();
            assert_eq!(
                live.count(&ov, DIM_OWNER, alice),
                3,
                "round-trip restores alice"
            );
            assert_eq!(
                live.count(&ov, DIM_OWNER, bob),
                0,
                "round-trip restores bob"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn burn_removes_everywhere() {
        let path = std::env::temp_dir()
            .join("aa_live_burn.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = tiny_base(&path);
        let alice = name::encode("alice");
        live.apply_block(101, &[Delta::Burn(BurnD { asset_id: 1001 })]);
        let ov = live.overlay();
        assert!(!live.exists(&ov, 1001), "burned asset gone");
        assert_eq!(live.point_owner(&ov, 1001), None);
        assert_eq!(live.count(&ov, DIM_OWNER, alice), 2, "owner count drops");
        assert!(!live.page(&ov, DIM_OWNER, alice, 100).contains(&1001));
        assert!(
            !live.browse(&ov, 0, 100).contains(&1001),
            "gone from browse"
        );
        // the Mythic facet had {1000,1001}; burning 1001 leaves {1000}
        assert_eq!(live.count(&ov, DIM_FACET, fkey("rarity", "Mythic")), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn mint_is_newest_and_lookupable() {
        let path = std::env::temp_dir()
            .join("aa_live_mint.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = tiny_base(&path);
        let carol = name::encode("carol");
        let new_id = live.base_max_id + 1;
        live.apply_block(
            101,
            &[Delta::Mint(MintD {
                asset_id: new_id,
                owner: carol,
                collection: name::encode("col"),
                schema: name::encode("sch"),
                schema_key: coll_schema_key("col", "sch"),
                template_id: 7,
                facet_key: Some(fkey("rarity", "Mythic")),
                immutable: vec![],
                mutable: vec![(1u8, "Mythic".to_string())],
            })],
        );
        let ov = live.overlay();
        assert!(live.exists(&ov, new_id));
        assert_eq!(live.point_owner(&ov, new_id), Some(carol));
        // newest id → first in browse
        assert_eq!(live.browse(&ov, 0, 1), vec![new_id]);
        assert!(live.page(&ov, DIM_OWNER, carol, 100).contains(&new_id));
        // template_mint continues past the base max (base had 3 assets on tmpl 7 → next is 4)
        if let Some(FwdState::Live(a)) = ov.fwd.get(&new_id) {
            assert_eq!(
                a.template_mint, 4,
                "mint ordinal continues from base dense max"
            );
        } else {
            panic!("mint not live");
        }
        // Mythic facet now {1000,1001,new_id}
        assert_eq!(live.count(&ov, DIM_FACET, fkey("rarity", "Mythic")), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn setdata_moves_between_facets() {
        let path = std::env::temp_dir()
            .join("aa_live_setdata.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = tiny_base(&path);
        // asset 1002 Common → Mythic
        live.apply_block(
            101,
            &[Delta::SetData(SetDataD {
                asset_id: 1002,
                mutable: vec![(1u8, "Mythic".to_string())],
                facet_old: Some(fkey("rarity", "Common")),
                facet_new: Some(fkey("rarity", "Mythic")),
            })],
        );
        let ov = live.overlay();
        assert_eq!(
            live.count(&ov, DIM_FACET, fkey("rarity", "Common")),
            0,
            "left Common"
        );
        assert_eq!(
            live.count(&ov, DIM_FACET, fkey("rarity", "Mythic")),
            3,
            "joined Mythic"
        );
        assert!(live
            .page(&ov, DIM_FACET, fkey("rarity", "Mythic"), 100)
            .contains(&1002));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn compaction_folds_overlay_into_new_base() {
        let path = std::env::temp_dir()
            .join("aa_live_compact_src.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let out = std::env::temp_dir()
            .join("aa_live_compact_dst.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = tiny_base(&path);
        let (alice, bob, carol) = (
            name::encode("alice"),
            name::encode("bob"),
            name::encode("carol"),
        );
        let new_id = live.base_max_id + 1;
        // transfer 1000 alice→bob, burn 1001, setdata 1002 Common→Mythic, mint a new carol Mythic asset
        live.apply_block(
            101,
            &[
                Delta::Transfer(TransferD {
                    asset_id: 1000,
                    new_owner: bob,
                }),
                Delta::Burn(BurnD { asset_id: 1001 }),
                Delta::SetData(SetDataD {
                    asset_id: 1002,
                    mutable: vec![(1u8, "Mythic".to_string())],
                    facet_old: Some(fkey("rarity", "Common")),
                    facet_new: Some(fkey("rarity", "Mythic")),
                }),
                Delta::Mint(MintD {
                    asset_id: new_id,
                    owner: carol,
                    collection: name::encode("col"),
                    schema: name::encode("sch"),
                    schema_key: coll_schema_key("col", "sch"),
                    template_id: 7,
                    facet_key: Some(fkey("rarity", "Mythic")),
                    immutable: vec![],
                    mutable: vec![(1u8, "Mythic".to_string())],
                }),
            ],
        );
        let stats = live.compact(&out).unwrap();
        assert_eq!(stats.assets, 3, "3 base − 1 burn + 1 mint");

        // the new base alone (empty overlay) must answer identically to the old base+overlay.
        let fresh = LiveSeg::open(&out, vec!["rarity".to_string()]).unwrap();
        let fo = fresh.overlay();
        assert_eq!(fresh.count(&fo, DIM_OWNER, bob), 1);
        assert_eq!(fresh.count(&fo, DIM_OWNER, alice), 1); // only 1002 remains
        assert_eq!(fresh.count(&fo, DIM_OWNER, carol), 1);
        assert!(fresh.page(&fo, DIM_OWNER, bob, 100).contains(&1000));
        assert!(!fresh.exists(&fo, 1001), "burned asset not in the new base");
        assert!(fresh.exists(&fo, new_id), "mint folded into the new base");
        // Mythic facet: base {1000,1001}; 1001 burned, 1002 joined, new minted → {1000,1002,new_id}
        assert_eq!(fresh.count(&fo, DIM_FACET, fkey("rarity", "Mythic")), 3);
        assert_eq!(fresh.count(&fo, DIM_FACET, fkey("rarity", "Common")), 0);
        // overlay reclaimed: the fresh store carries no overlay heap.
        let (heap, _) = fresh.overlay_heap_bytes();
        assert!(heap < 64, "fresh overlay heap ~0, got {heap}");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn fork_rollback_reverts() {
        let path = std::env::temp_dir()
            .join("aa_live_fork.wseg")
            .to_str()
            .unwrap()
            .to_string();
        let live = tiny_base(&path);
        let alice = name::encode("alice");
        let bob = name::encode("bob");
        live.apply_block(
            101,
            &[Delta::Transfer(TransferD {
                asset_id: 1000,
                new_owner: bob,
            })],
        );
        assert_eq!(live.point_owner(&live.overlay(), 1000), Some(bob));
        // a fork reverts block 101
        live.rollback_to(100);
        let ov = live.overlay();
        assert_eq!(
            live.point_owner(&ov, 1000),
            Some(alice),
            "transfer reverted by fork"
        );
        assert_eq!(live.count(&ov, DIM_OWNER, alice), 3);
        assert_eq!(live.count(&ov, DIM_OWNER, bob), 0);
        let _ = std::fs::remove_file(&path);
    }
}
