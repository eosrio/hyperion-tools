//! `AtomicBuilder` — accumulates AtomicAssets state (schemas → templates → assets, in that order)
//! and writes the faceted `.wseg` segment: forward stores + per-dimension inverted posting lists.
//!
//! Push order matters: `schemas` must be pushed before the `templates`/`assets` that reference them,
//! so a row's `(field name → schema index)` map is available when encoding its data attributes.

use std::collections::HashMap;

use mongodb::bson::{Bson, Document};

use crate::aa_binfmt::{
    encode_asset, encode_posting_list, encode_schema_format, encode_template, Attr,
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
            stats: AaStats::default(),
        }
    }

    /// Dispatch one Mongo doc by collection. Unknown collections are ignored.
    pub fn push(&mut self, coll: &str, d: &Document) {
        match coll {
            "atomicassets-schemas" => self.push_schema(d),
            "atomicassets-templates" => self.push_template(d),
            "atomicassets-assets" => self.push_asset(d),
            _ => {}
        }
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
        let blob = encode_posting_list(&mut ids);
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
