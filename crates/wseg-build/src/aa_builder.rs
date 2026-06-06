//! `AtomicBuilder` — accumulates AtomicAssets state (schemas → templates → assets, in that order)
//! and writes the faceted `.wseg` segment: forward stores + per-dimension inverted posting lists.
//!
//! Push order matters: `schemas` must be pushed before the `templates`/`assets` that reference them,
//! so a row's `(field name → schema index)` map is available when encoding its data attributes.

use std::collections::HashMap;

use mongodb::bson::{Bson, Document};

use crate::aa_binfmt::{
    encode_asset, encode_config, encode_posting_hybrid, encode_schema_format, encode_template, Attr,
};
use crate::aa_tables::*;
use crate::name;
use crate::wseg::{write_segment, IndexEntry, Table};

/// Build stats returned by [`AtomicBuilder::finish`].
#[derive(Debug, Default)]
pub struct AaStats {
    pub schemas: u64,
    pub templates: u64,
    pub assets: u64,
    pub bytes: u64,
}

/// A forward store accumulated as one contiguous arena + an index, so per-row blobs don't each get
/// their own heap allocation (at 88M+ assets that allocation churn dominates).
#[derive(Default)]
struct Fwd {
    arena: Vec<u8>,
    index: Vec<IndexEntry>,
}
impl Fwd {
    fn push(&mut self, key: u64, blob: &[u8]) {
        let off = self.arena.len() as u64;
        self.arena.extend_from_slice(blob);
        self.index.push(IndexEntry {
            key,
            off,
            len: blob.len() as u32,
        });
    }
    fn into_table(self, table_id: u32) -> Table {
        Table {
            table_id,
            index: self.index,
            arena: self.arena,
        }
    }
}

/// Per-schema info: the ordered `(name, type)` fields + a field-name → index map.
type SchemaInfo = (Vec<(String, String)>, HashMap<String, u8>);

pub struct AtomicBuilder {
    /// coll_schema_key → schema fields + name→index.
    schemas: HashMap<u64, SchemaInfo>,
    asset_fwd: Fwd,
    tmpl_fwd: Fwd,
    schema_fwd: Fwd,
    by_owner: HashMap<u64, Vec<u64>>,
    by_coll: HashMap<u64, Vec<u64>>,
    by_schema: HashMap<u64, Vec<u64>>,
    by_tmpl: HashMap<u64, Vec<u64>>,
    by_data: HashMap<u64, Vec<u64>>,
    all_ids: Vec<u64>,
    /// Data-attribute fields to inverted-index (low-cardinality facets; default `["rarity"]`).
    data_fields: Vec<String>,
    /// The config singleton blob (`/v1/config`), set by the `atomicassets-config` doc.
    config: Option<Vec<u8>>,
    stats: AaStats,
}

impl Default for AtomicBuilder {
    fn default() -> Self {
        Self::new(vec!["rarity".to_string()])
    }
}

impl AtomicBuilder {
    pub fn new(data_fields: Vec<String>) -> Self {
        AtomicBuilder {
            schemas: HashMap::new(),
            asset_fwd: Fwd::default(),
            tmpl_fwd: Fwd::default(),
            schema_fwd: Fwd::default(),
            by_owner: HashMap::new(),
            by_coll: HashMap::new(),
            by_schema: HashMap::new(),
            by_tmpl: HashMap::new(),
            by_data: HashMap::new(),
            all_ids: Vec::new(),
            data_fields,
            config: None,
            stats: AaStats::default(),
        }
    }

    /// Dispatch one Mongo doc by collection. Unknown collections are ignored.
    pub fn push(&mut self, coll: &str, d: &Document) {
        match coll {
            "atomicassets-config" => self.push_config(d),
            "atomicassets-schemas" => self.push_schema(d),
            "atomicassets-templates" => self.push_template(d),
            "atomicassets-assets" => self.push_asset(d),
            _ => {}
        }
    }

    /// `atomicassets-config` (singleton) → the config blob served at `/v1/config`.
    fn push_config(&mut self, d: &Document) {
        let contract = doc_str(d, "contract").map(name::encode).unwrap_or(0);
        let version = doc_str(d, "version").unwrap_or("").to_string();
        let mut fmt: Vec<(String, String)> = Vec::new();
        if let Ok(arr) = d.get_array("collection_format") {
            for f in arr {
                if let Bson::Document(fd) = f {
                    if let (Some(n), Some(t)) = (doc_str(fd, "name"), doc_str(fd, "type")) {
                        fmt.push((n.to_string(), t.to_string()));
                    }
                }
            }
        }
        let mut tokens: Vec<(u64, String, i64)> = Vec::new();
        if let Ok(arr) = d.get_array("supported_tokens") {
            for t in arr {
                if let Bson::Document(td) = t {
                    let tc = doc_str(td, "token_contract").map(name::encode).unwrap_or(0);
                    let sym = doc_str(td, "token_symbol").unwrap_or("").to_string();
                    let prec = doc_i64(td, "token_precision").unwrap_or(0);
                    tokens.push((tc, sym, prec));
                }
            }
        }
        self.config = Some(encode_config(contract, &version, &fmt, &tokens));
    }

    fn push_schema(&mut self, d: &Document) {
        let (Some(coll), Some(sch)) = (doc_str(d, "collection_name"), doc_str(d, "schema_name"))
        else {
            return;
        };
        let mut fields: Vec<(String, String)> = Vec::new();
        if let Ok(arr) = d.get_array("format") {
            for f in arr {
                if let Bson::Document(fd) = f {
                    if let (Some(n), Some(t)) = (doc_str(fd, "name"), doc_str(fd, "type")) {
                        fields.push((n.to_string(), t.to_string()));
                    }
                }
            }
        }
        let idx: HashMap<String, u8> = fields
            .iter()
            .enumerate()
            .map(|(i, (n, _))| (n.clone(), i as u8))
            .collect();
        let key = coll_schema_key(coll, sch);
        self.schema_fwd.push(key, &encode_schema_format(&fields));
        self.schemas.insert(key, (fields, idx));
        self.stats.schemas += 1;
    }

    fn push_template(&mut self, d: &Document) {
        let (Some(coll), Some(sch)) = (doc_str(d, "collection_name"), doc_str(d, "schema_name"))
        else {
            return;
        };
        let Some(tid) = doc_i32(d, "template_id") else {
            return;
        };
        let skey = coll_schema_key(coll, sch);
        let immutable = self.attrs(skey, d.get_document("immutable_data").ok());
        let blob = encode_template(tid, name::encode(sch), &immutable);
        self.tmpl_fwd.push(template_key(tid as i64), &blob);
        self.stats.templates += 1;
    }

    fn push_asset(&mut self, d: &Document) {
        let (Some(coll), Some(sch), Some(owner)) = (
            doc_str(d, "collection_name"),
            doc_str(d, "schema_name"),
            doc_str(d, "owner"),
        ) else {
            return;
        };
        let Some(asset_id) = doc_str(d, "asset_id").and_then(|s| s.parse::<u64>().ok()) else {
            return;
        };
        let tid = doc_i32(d, "template_id").unwrap_or(-1);
        let skey = coll_schema_key(coll, sch);
        let immutable = self.attrs(skey, d.get_document("immutable_data").ok());
        let mutable = self.attrs(skey, d.get_document("mutable_data").ok());

        let (owner_u, coll_u, schema_u) =
            (name::encode(owner), name::encode(coll), name::encode(sch));
        let blob = encode_asset(
            owner_u,
            coll_u,
            schema_u,
            tid,
            doc_u32(d, "block_num"),
            0, // template_mint — patched in finish() (needs all of a template's assets to rank)
            &immutable,
            &mutable,
        );
        self.asset_fwd.push(asset_id, &blob);

        self.by_owner.entry(owner_u).or_default().push(asset_id);
        self.by_coll.entry(coll_u).or_default().push(asset_id);
        self.by_schema.entry(skey).or_default().push(asset_id);
        if tid >= 0 {
            self.by_tmpl
                .entry(template_key(tid as i64))
                .or_default()
                .push(asset_id);
        }
        // data-attribute inverted index for the configured facet fields (scalar, short values).
        for fld in &self.data_fields {
            let val = d
                .get_document("immutable_data")
                .ok()
                .and_then(|m| m.get(fld))
                .or_else(|| d.get_document("mutable_data").ok().and_then(|m| m.get(fld)));
            if let Some(b) = val {
                let v = bson_canon(b);
                if !v.is_empty() && v.len() <= 64 {
                    self.by_data
                        .entry(data_attr_key(coll, sch, fld, &v))
                        .or_default()
                        .push(asset_id);
                }
            }
        }
        self.all_ids.push(asset_id);
        self.stats.assets += 1;
    }

    // ── raw (already-decoded) push API — used by COMPACTION to fold the live overlay + immutable base
    //    back into a fresh segment, reusing all of the posting / sorted / template_mint logic in
    //    finish() instead of reimplementing it. Inputs are the on-segment forms (name-u64 keys,
    //    (field_idx, value) attrs, precomputed facet keys), so no bson round-trip. ────────────────────
    /// Register a schema format (its blob), keyed by `coll_schema_key`.
    pub fn push_schema_raw(&mut self, schema_key: u64, fields: &[(String, String)]) {
        self.schema_fwd
            .push(schema_key, &encode_schema_format(fields));
        self.stats.schemas += 1;
    }

    /// Register a template forward record (its immutable attrs, stored once).
    pub fn push_template_raw(&mut self, template_id: i32, schema_u: u64, immutable: &[Attr]) {
        self.tmpl_fwd.push(
            template_key(template_id as i64),
            &encode_template(template_id, schema_u, immutable),
        );
        self.stats.templates += 1;
    }

    /// Add one already-decoded current asset to the forward store + every inverted index. `facet_keys`
    /// are the precomputed `data_attr_key`s the asset participates in (0 or 1 per configured facet).
    #[allow(clippy::too_many_arguments)]
    pub fn push_asset_raw(
        &mut self,
        asset_id: u64,
        owner_u: u64,
        coll_u: u64,
        schema_u: u64,
        schema_key: u64,
        template_id: i32,
        block_num: u32,
        immutable: &[Attr],
        mutable: &[Attr],
        facet_keys: &[u64],
    ) {
        let blob = encode_asset(
            owner_u,
            coll_u,
            schema_u,
            template_id,
            block_num,
            0,
            immutable,
            mutable,
        );
        self.asset_fwd.push(asset_id, &blob);
        self.by_owner.entry(owner_u).or_default().push(asset_id);
        self.by_coll.entry(coll_u).or_default().push(asset_id);
        self.by_schema.entry(schema_key).or_default().push(asset_id);
        if template_id >= 0 {
            self.by_tmpl
                .entry(template_key(template_id as i64))
                .or_default()
                .push(asset_id);
        }
        for &fk in facet_keys {
            self.by_data.entry(fk).or_default().push(asset_id);
        }
        self.all_ids.push(asset_id);
        self.stats.assets += 1;
    }

    /// Decode a data subdocument into `(field_idx, canonical_value)` attrs, dropping fields not in the
    /// schema. Empty/absent → no attrs.
    fn attrs(&self, schema_key: u64, doc: Option<&Document>) -> Vec<Attr> {
        let Some(doc) = doc else { return Vec::new() };
        let Some((_, idx)) = self.schemas.get(&schema_key) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(doc.len());
        for (k, v) in doc {
            if let Some(&i) = idx.get(k) {
                out.push((i, bson_canon(v)));
            }
        }
        out
    }

    pub fn finish(mut self, out: &str) -> std::io::Result<AaStats> {
        // Materialize template_mint = 1-based rank within each template (asset_id asc), reconstructed
        // from the by_tmpl postings — so "sort by mint" (a history-looking sort) stays sub-µs without ES.
        let mut tmpl_mint: HashMap<u64, u32> = HashMap::with_capacity(self.stats.assets as usize);
        for ids in self.by_tmpl.values() {
            let mut s = ids.clone();
            s.sort_unstable();
            for (rank, &aid) in s.iter().enumerate() {
                tmpl_mint.insert(aid, (rank + 1) as u32);
            }
        }
        // patch each asset's forward blob (template_mint is the 4 bytes at blob offset 33)
        let idx = std::mem::take(&mut self.asset_fwd.index);
        for e in &idx {
            if let Some(&m) = tmpl_mint.get(&e.key) {
                let o = e.off as usize + 33;
                self.asset_fwd.arena[o..o + 4].copy_from_slice(&m.to_le_bytes());
            }
        }
        self.asset_fwd.index = idx;
        // presorted (template_mint, asset_id) ordering for the sort-by-mint browse
        let mut pairs: Vec<(u32, u64)> = tmpl_mint.iter().map(|(&a, &m)| (m, a)).collect();
        pairs.sort_unstable();
        let mut ta = Vec::with_capacity(4 + pairs.len() * 12);
        ta.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
        for (m, a) in &pairs {
            ta.extend_from_slice(&m.to_le_bytes());
            ta.extend_from_slice(&a.to_le_bytes());
        }
        let tmpl_sorted = Table {
            table_id: TABLE_AA_SORTED_TMPL,
            index: vec![IndexEntry {
                key: SENTINEL_KEY,
                off: 0,
                len: ta.len() as u32,
            }],
            arena: ta,
        };

        let mut tables: Vec<Table> = vec![
            std::mem::take(&mut self.asset_fwd).into_table(TABLE_AA_FWD),
            std::mem::take(&mut self.tmpl_fwd).into_table(TABLE_AA_TMPL_FWD),
            std::mem::take(&mut self.schema_fwd).into_table(TABLE_AA_SCHEMAS),
            posting_table(TABLE_AA_BY_OWNER, std::mem::take(&mut self.by_owner)),
            posting_table(TABLE_AA_BY_COLL, std::mem::take(&mut self.by_coll)),
            posting_table(TABLE_AA_BY_SCHEMA, std::mem::take(&mut self.by_schema)),
            posting_table(TABLE_AA_BY_TMPL, std::mem::take(&mut self.by_tmpl)),
            posting_table(TABLE_AA_DATA_ATTR, std::mem::take(&mut self.by_data)),
        ];

        // presorted browse: all asset_ids descending, in a single sentinel-keyed blob.
        let mut ids = std::mem::take(&mut self.all_ids);
        ids.sort_unstable();
        ids.dedup();
        let mut arena = Vec::with_capacity(4 + ids.len() * 8);
        arena.extend_from_slice(&(ids.len() as u32).to_le_bytes());
        for &id in ids.iter().rev() {
            arena.extend_from_slice(&id.to_le_bytes());
        }
        let sorted_len = arena.len() as u32;
        tables.push(Table {
            table_id: TABLE_AA_SORTED_ID,
            index: vec![IndexEntry {
                key: SENTINEL_KEY,
                off: 0,
                len: sorted_len,
            }],
            arena,
        });
        tables.push(tmpl_sorted);

        // config singleton (one sentinel-keyed entry), if the `atomicassets-config` doc was seen.
        if let Some(cfg) = self.config.take() {
            tables.push(Table {
                table_id: TABLE_AA_CONFIG,
                index: vec![IndexEntry {
                    key: SENTINEL_KEY,
                    off: 0,
                    len: cfg.len() as u32,
                }],
                arena: cfg,
            });
        }

        self.stats.bytes = tables.iter().map(|t| t.arena.len() as u64).sum();
        write_segment(out, tables)?;
        Ok(self.stats)
    }
}

/// Assemble an inverted-index Table: one posting-list blob per key.
fn posting_table(table_id: u32, map: HashMap<u64, Vec<u64>>) -> Table {
    let mut arena = Vec::new();
    let mut index = Vec::with_capacity(map.len());
    for (key, mut ids) in map {
        let blob = encode_posting_hybrid(&mut ids);
        let off = arena.len() as u64;
        arena.extend_from_slice(&blob);
        index.push(IndexEntry {
            key,
            off,
            len: blob.len() as u32,
        });
    }
    Table {
        table_id,
        index,
        arena,
    }
}

// ── bson helpers ─────────────────────────────────────────────────────────────────────────────────
fn doc_str<'a>(d: &'a Document, k: &str) -> Option<&'a str> {
    d.get_str(k).ok()
}
fn doc_i32(d: &Document, k: &str) -> Option<i32> {
    match d.get(k) {
        Some(Bson::Int32(i)) => Some(*i),
        Some(Bson::Int64(i)) => Some(*i as i32),
        Some(Bson::Double(f)) => Some(*f as i32),
        _ => None,
    }
}
fn doc_u32(d: &Document, k: &str) -> u32 {
    match d.get(k) {
        Some(Bson::Int32(i)) => *i as u32,
        Some(Bson::Int64(i)) => *i as u32,
        Some(Bson::Double(f)) => *f as u32,
        _ => 0,
    }
}
fn doc_i64(d: &Document, k: &str) -> Option<i64> {
    match d.get(k) {
        Some(Bson::Int32(i)) => Some(*i as i64),
        Some(Bson::Int64(i)) => Some(*i),
        Some(Bson::Double(f)) => Some(*f as i64),
        _ => None,
    }
}

/// Canonical string form of a bson value, matching what the API filters on.
fn bson_canon(b: &Bson) -> String {
    match b {
        Bson::String(s) => s.clone(),
        Bson::Int32(i) => i.to_string(),
        Bson::Int64(i) => i.to_string(),
        Bson::Double(f) => f.to_string(),
        Bson::Boolean(v) => v.to_string(),
        Bson::Array(a) => {
            let parts: Vec<String> = a.iter().map(bson_canon).collect();
            format!("[{}]", parts.join(","))
        }
        Bson::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn builds_a_tiny_segment() {
        let mut b = AtomicBuilder::new(vec!["rarity".to_string()]);
        // config singleton flows through the builder (push_config → finish() emit) without panicking.
        b.push(
            "atomicassets-config",
            &doc! { "contract": "atomicassets",
            "collection_format": [ {"name":"name","type":"string"} ],
            "supported_tokens": [ {"token_contract":"eosio.token","token_symbol":"WAX","token_precision":8i32} ] },
        );
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
        b.push(
            "atomicassets-assets",
            &doc! { "collection_name": "col", "schema_name": "sch", "owner": "alice",
            "asset_id": "1099511627776", "template_id": 7i32, "block_num": 100i64,
            "immutable_data": {}, "mutable_data": { "rarity": "Mythic" } },
        );
        let dir = std::env::temp_dir().join("aa_builder_test.wseg");
        let stats = b.finish(dir.to_str().unwrap()).unwrap();
        assert_eq!(stats.assets, 1);
        assert_eq!(stats.templates, 1);
        assert_eq!(stats.schemas, 1);
        assert!(stats.bytes > 0);
        assert!(std::fs::metadata(&dir).unwrap().len() > 0);
        let _ = std::fs::remove_file(&dir);
    }
}
