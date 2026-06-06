//! AtomicAssets state → eosio-contract-api-shaped MongoDB docs (the `atomicassets` contract tables).
//!
//! This is the AtomicAssets half of the `--tables atomicassets`/`atomic` preset. It decodes the live
//! contract-table *state* a snapshot carries — `schemas`, `collections`, `templates`, `assets`,
//! `offers`, `config` — into the document shapes the AtomicAssets API (`pinknetworkx/eosio-contract-api`)
//! serves, so a Hyperion operator can serve that API from MongoDB without a separate Postgres stack.
//!
//! ## What a snapshot can and cannot give us (S / D / H)
//! - **S** — plain on-chain row fields (asset_id, owner, collection_name, …). Present verbatim.
//! - **D** — the `*_serialized_data` blobs, decoded via [`atomicdata`] using the schema/collection
//!   `format` (this is why we need the [`SchemaRegistry`] pre-pass).
//! - **H** — history-derived fields (mint/transfer/burn timestamps, `template_mint`, burned assets,
//!   terminal-state offers). These are **not** in any contract table; eosio-contract-api accumulates
//!   them from action traces. They are intentionally **omitted** here and left to the live feed / ES
//!   (see `benchmark/atomicassets/REQUIREMENTS.md`). Every doc carries `block_num` as state provenance.
//!
//! ## The serialized_data shape
//! In the `atomicassets` ABI the `*_serialized_data` fields are typed `uint8[]`, so abieos renders them
//! as a JSON **array of byte numbers** (not a hex string). [`extract_bytes`] tolerates both. Collection
//! data decodes against the **global** `config.collection_format`; template/asset data decodes against
//! the row's `(collection_name, schema_name)` schema `format`.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use rs_abieos::{AbiHandle, Abieos};
use serde_json::{json, Map, Value};

use crate::map::RowCtx;
use crate::model::{Filter, RawRow, Targets};
use crate::reader::{find, Section, Snap};
use crate::tables;

/// Mongo collection names (Hyperion's `${code}-${table}` dynamic convention, so they sit alongside
/// `eosio-userres` et al.). Named here so [`crate::mongo::build_indexes`] can give them AA-specific
/// indexes instead of the generic contract-state ones.
pub const COLL_AA_SCHEMAS: &str = "atomicassets-schemas";
pub const COLL_AA_COLLECTIONS: &str = "atomicassets-collections";
pub const COLL_AA_TEMPLATES: &str = "atomicassets-templates";
pub const COLL_AA_ASSETS: &str = "atomicassets-assets";
pub const COLL_AA_OFFERS: &str = "atomicassets-offers";
pub const COLL_AA_CONFIG: &str = "atomicassets-config";

/// Contract tables the `atomicassets` preset loads. `config` is loaded for its counters + supported
/// tokens (and its `collection_format` is also read by the schema-registry pre-pass).
pub const ATOMICASSETS_TABLES: &[&str] = &[
    "atomicassets:schemas",
    "atomicassets:collections",
    "atomicassets:templates",
    "atomicassets:assets",
    "atomicassets:offers",
    "atomicassets:config",
];

/// All AtomicAssets + AtomicMarket Mongo collections (for the preset's `--mongo-drop` set).
pub fn all_collections() -> Vec<&'static str> {
    let mut v = vec![
        COLL_AA_SCHEMAS,
        COLL_AA_COLLECTIONS,
        COLL_AA_TEMPLATES,
        COLL_AA_ASSETS,
        COLL_AA_OFFERS,
        COLL_AA_CONFIG,
    ];
    v.extend_from_slice(crate::atomicmarket::ALL_COLLECTIONS);
    v
}

// ── schema-format registry (the D-field prerequisite) ────────────────────────────────────────────

/// Decoded schema `format`s, keyed for the main pass: `collection -> schema_name -> format`, plus the
/// single global collection `format` from `config`. Built by [`build_schema_registry`] in a seek-path
/// pre-pass over the `schemas` + `config` tables, then shared (read-only) across the decode workers.
#[derive(Default)]
pub struct SchemaRegistry {
    schemas: HashMap<String, HashMap<String, Vec<atomicdata::Field>>>,
    collection_format: Vec<atomicdata::Field>,
    /// The contract `version` from the `tokenconfigs` table (v2). Empty on pre-v2 chains that lack it
    /// — `map_config` then omits the field rather than emitting an invalid `""`.
    version: String,
}

impl SchemaRegistry {
    /// Number of `(collection, schema)` formats loaded.
    pub fn schema_count(&self) -> usize {
        self.schemas.values().map(HashMap::len).sum()
    }
    /// Field count of the global collection format (`config.collection_format`).
    pub fn collection_format_len(&self) -> usize {
        self.collection_format.len()
    }
    /// The schema `format` for a `(collection, schema_name)` pair, if known (zero-alloc lookup).
    fn format_for(&self, collection: &str, schema: &str) -> Option<&[atomicdata::Field]> {
        self.schemas
            .get(collection)
            .and_then(|m| m.get(schema))
            .map(Vec::as_slice)
    }
}

/// Pre-pass (seek path only): walk just the `atomicassets` `schemas` + `config` rows and build the
/// [`SchemaRegistry`]. Runs before the main decode pass so every asset/template/collection blob can be
/// decoded against its format. Cheap — `schemas` are a few thousand rows and `config` is a singleton —
/// and leaves `s` re-seekable (the main pass seeks back to the section start).
pub fn build_schema_registry(
    s: &mut Snap,
    secs: &[Section],
    chain_version: u32,
    abi_raw: &HashMap<u64, Vec<u8>>,
    names: &Abieos,
) -> Result<SchemaRegistry> {
    let nm = |x: &str| {
        names
            .string_to_name(x)
            .map_err(|e| anyhow!("string_to_name({x}): {e:?}"))
    };
    let aa = nm("atomicassets")?;
    let schemas_t = nm("schemas")?;
    let config_t = nm("config")?;
    let tokenconfigs_t = nm("tokenconfigs")?;

    let mut reg = SchemaRegistry::default();

    // Decode the schemas/config rows with the atomicassets ABI alone (no full registry needed).
    let Some(abi_bytes) = abi_raw.get(&aa) else {
        eprintln!(
            "[snapshot-load] atomicassets: ABI not present in snapshot — schema registry empty (serialized_data will not decode)"
        );
        return Ok(reg);
    };
    let mut handle =
        AbiHandle::from_bin(abi_bytes).map_err(|e| anyhow!("parse atomicassets ABI: {e:?}"))?;

    let targets = Targets {
        filters: vec![
            Filter::CodeTable(aa, schemas_t),
            Filter::CodeTable(aa, config_t),
            Filter::CodeTable(aa, tokenconfigs_t),
        ],
    };

    let mut decoded = String::new();
    let mut sink = |row: RawRow| -> Result<()> {
        decoded.clear();
        if handle
            .decode_table_row_into(row.table, &row.value, &mut decoded)
            .is_err()
        {
            return Ok(()); // skip an undecodable row rather than abort the whole pre-pass
        }
        let Ok(data) = serde_json::from_str::<Value>(&decoded) else {
            return Ok(());
        };
        if row.table == schemas_t {
            // schemas scope = collection_name.
            let collection = names.name_to_string(row.scope).unwrap_or_default();
            if let Some(schema_name) = data.get("schema_name").and_then(Value::as_str) {
                if let Ok(fields) =
                    atomicdata::Field::from_format_json(data.get("format").unwrap_or(&Value::Null))
                {
                    reg.schemas
                        .entry(collection)
                        .or_default()
                        .insert(schema_name.to_string(), fields);
                }
            }
        } else if row.table == config_t {
            if let Ok(fields) = atomicdata::Field::from_format_json(
                data.get("collection_format").unwrap_or(&Value::Null),
            ) {
                reg.collection_format = fields;
            }
        } else if row.table == tokenconfigs_t {
            // tokenconfigs (v2) → the contract `version` the API's /config reports.
            if let Some(v) = data.get("version").and_then(Value::as_str) {
                reg.version = v.to_string();
            }
        }
        Ok(())
    };

    if chain_version < 7 {
        let ct = find(secs, "contract_tables")
            .ok_or_else(|| anyhow!("no contract_tables section (v6)"))?;
        tables::walk_v6(s, ct, &targets, None, &mut sink)?;
    } else {
        let tid = find(secs, "eosio::chain::table_id_object")
            .ok_or_else(|| anyhow!("no table_id_object section (v8)"))?;
        let kv = find(secs, "eosio::chain::key_value_object")
            .ok_or_else(|| anyhow!("no key_value_object section (v8)"))?;
        let interesting = tables::load_table_ids_v8(s, tid, &targets)?;
        tables::walk_v8(s, kv, &interesting, None, &mut sink)?;
    }
    Ok(reg)
}

// ── mappers (one per table) ──────────────────────────────────────────────────────────────────────

/// `schemas` row → schema doc. `format` is the on-chain `[{name,type}]` passed through verbatim.
pub fn map_schema(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = Map::new();
    doc.insert("contract".into(), json!(ctx.code));
    doc.insert("collection_name".into(), json!(ctx.scope)); // schemas scope = collection
    doc.insert(
        "schema_name".into(),
        data.get("schema_name").cloned().unwrap_or(Value::Null),
    );
    doc.insert(
        "format".into(),
        data.get("format")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![])),
    );
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `collections` row → collection doc. `serialized_data` (D) decodes against the global
/// `config.collection_format`.
pub fn map_collection(ctx: &RowCtx, data: Value, reg: &SchemaRegistry) -> Value {
    let data_obj = decode_blob(
        Some(&reg.collection_format),
        &extract_bytes(data.get("serialized_data")),
    );
    let mut doc = Map::new();
    doc.insert("contract".into(), json!(ctx.code));
    for f in [
        "collection_name",
        "author",
        "allow_notify",
        "authorized_accounts",
        "notify_accounts",
        "market_fee",
    ] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    doc.insert("data".into(), Value::Object(data_obj));
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `templates` row → template doc. `immutable_serialized_data` (D) decodes against the
/// `(collection, schema_name)` schema format. `collection_name` = the row scope.
pub fn map_template(ctx: &RowCtx, data: Value, reg: &SchemaRegistry) -> Value {
    let schema_name = data
        .get("schema_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fmt = reg.format_for(ctx.scope, schema_name);
    let immutable = decode_blob(fmt, &extract_bytes(data.get("immutable_serialized_data")));

    let mut doc = Map::new();
    doc.insert("contract".into(), json!(ctx.code));
    doc.insert("collection_name".into(), json!(ctx.scope));
    doc.insert(
        "template_id".into(),
        data.get("template_id").cloned().unwrap_or(Value::Null),
    );
    doc.insert("schema_name".into(), json!(schema_name));
    for f in ["transferable", "burnable", "max_supply", "issued_supply"] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    // Templates have only immutable data; expose it both as `immutable_data` and the merged `data`
    // (so a single `data.*` index serves attribute filters uniformly across templates and assets).
    doc.insert("immutable_data".into(), Value::Object(immutable.clone()));
    doc.insert("data".into(), Value::Object(immutable));
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `assets` row → asset doc. `owner` = the row scope. `immutable`/`mutable_serialized_data` (D) decode
/// against the `(collection_name, schema_name)` schema format; `data` is the merged view
/// (mutable overrides immutable), mirroring eosio-contract-api's combined-data attribute index.
pub fn map_asset(ctx: &RowCtx, data: Value, reg: &SchemaRegistry) -> Value {
    let collection = data
        .get("collection_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let schema_name = data
        .get("schema_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let fmt = reg.format_for(collection, schema_name);
    // float/double attributes are emitted as NUMBERS — the canonical schema decode (matching templates
    // and public eosio-contract-api). PARITY NOTE: the live API may render a float/double *asset* attr
    // as a STRING when that asset's `logmint` action passed the value through the atomicdata
    // attribute-map's *string* variant instead of float64 (e.g. WAX `cybauthority` doubles show as
    // `"3.97"`, while `pagangodsapp` doubles show as `1`). That is a mint-action detail; a snapshot only
    // carries the canonicalized `serialized_data` (always the schema-typed 8 bytes), so the number↔string
    // choice is NOT recoverable here — only from the action via the live feed. Numbers is the honest,
    // schema-true value and matches the asset's own `serialized_data`.
    let immutable = decode_blob(fmt, &extract_bytes(data.get("immutable_serialized_data")));
    let mutable = decode_blob(fmt, &extract_bytes(data.get("mutable_serialized_data")));
    let merged = merge_data(&immutable, &mutable);

    let mut doc = Map::new();
    doc.insert("contract".into(), json!(ctx.code));
    doc.insert(
        "asset_id".into(),
        data.get("asset_id").cloned().unwrap_or(Value::Null),
    );
    doc.insert("owner".into(), json!(ctx.scope));
    doc.insert("collection_name".into(), json!(collection));
    doc.insert("schema_name".into(), json!(schema_name));
    doc.insert(
        "template_id".into(),
        normalize_template_id(data.get("template_id")),
    );
    if let Some(v) = data.get("ram_payer") {
        doc.insert("ram_payer".into(), v.clone());
    }
    doc.insert(
        "backed_tokens".into(),
        Value::Array(parse_backed_tokens(data.get("backed_tokens"))),
    );
    doc.insert("immutable_data".into(), Value::Object(immutable));
    doc.insert("mutable_data".into(), Value::Object(mutable));
    doc.insert("data".into(), Value::Object(merged));
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `offers` row → offer doc. The asset-id arrays are kept inline (a multikey index serves "offers
/// containing asset X"). `state` is **H** (PENDING vs INVALID needs a join vs current owners; terminal
/// states are deleted on-chain) and is left to the feed.
pub fn map_offer(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = Map::new();
    doc.insert("contract".into(), json!(ctx.code));
    for f in [
        "offer_id",
        "sender",
        "recipient",
        "sender_asset_ids",
        "recipient_asset_ids",
        "memo",
        "ram_payer",
    ] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `config` singleton → config doc. Counters are live on-chain (S); `supported_tokens` is flattened to
/// `{token_contract, token_symbol, token_precision}`.
pub fn map_config(ctx: &RowCtx, data: Value, reg: &SchemaRegistry) -> Value {
    let mut doc = Map::new();
    doc.insert("contract".into(), json!(ctx.code));
    // `version` comes from the `tokenconfigs` table (read in the pre-pass), NOT the `config` row.
    // Omit it entirely when the chain has no tokenconfigs (pre-v2) rather than emit an invalid "".
    if !reg.version.is_empty() {
        doc.insert("version".into(), json!(reg.version));
    }
    for f in [
        "asset_counter",
        "template_counter",
        "offer_counter",
        "collection_format",
    ] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    doc.insert(
        "supported_tokens".into(),
        Value::Array(parse_extended_symbols(data.get("supported_tokens"))),
    );
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

// ── shared helpers (used by atomicmarket too) ────────────────────────────────────────────────────

/// Collect a `uint8[]` serialized-data field into raw bytes. abieos renders `uint8[]` as a JSON array
/// of byte numbers; tolerate a hex string too (in case a future ABI types the field `bytes`).
pub(crate) fn extract_bytes(v: Option<&Value>) -> Vec<u8> {
    match v {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_u64)
            .map(|n| n as u8)
            .collect(),
        Some(Value::String(s)) => hex::decode(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Decode an atomicdata blob to an object using `format`. Empty blob → empty object; a decode error or
/// a missing format → empty object (the row is still stored; the data is just not resolved).
pub(crate) fn decode_blob(
    format: Option<&[atomicdata::Field]>,
    bytes: &[u8],
) -> Map<String, Value> {
    if bytes.is_empty() {
        return Map::new();
    }
    match format {
        Some(fmt) => atomicdata::deserialize_to_object(bytes, fmt).unwrap_or_default(),
        None => Map::new(),
    }
}

/// Merge immutable + mutable attribute maps; mutable wins on key collision (matches the API's `data`).
fn merge_data(immutable: &Map<String, Value>, mutable: &Map<String, Value>) -> Map<String, Value> {
    let mut m = immutable.clone();
    for (k, v) in mutable {
        m.insert(k.clone(), v.clone());
    }
    m
}

/// atomicassets stores "no template" as `template_id == -1`; surface that as JSON `null` (the API's
/// nullable `template_id`). Non-negative ids pass through as numbers.
fn normalize_template_id(v: Option<&Value>) -> Value {
    match v {
        Some(Value::Number(n)) => match n.as_i64() {
            Some(i) if i >= 0 => json!(i),
            _ => Value::Null,
        },
        Some(Value::String(s)) => match s.parse::<i64>() {
            Ok(i) if i >= 0 => json!(i),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

/// Parse an `asset[]` field ("1.0000 WAX") into `[{token_symbol, token_precision, amount}]`, where
/// `amount` is the exact integer base-unit count as a string.
fn parse_backed_tokens(v: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(Value::Array(arr)) = v {
        for it in arr {
            if let Some((amount, symbol, precision)) = it.as_str().and_then(parse_asset) {
                out.push(json!({
                    "token_symbol": symbol,
                    "token_precision": precision,
                    "amount": amount,
                }));
            }
        }
    }
    out
}

/// Parse an `extended_symbol[]` field (`{sym:"8,WAX", contract:"eosio.token"}`) into
/// `[{token_contract, token_symbol, token_precision}]`.
fn parse_extended_symbols(v: Option<&Value>) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(Value::Array(arr)) = v {
        for it in arr {
            let contract = it.get("contract").and_then(Value::as_str).unwrap_or("");
            if let Some((symbol, precision)) = it
                .get("sym")
                .and_then(Value::as_str)
                .and_then(parse_symbol_spec)
            {
                out.push(json!({
                    "token_contract": contract,
                    "token_symbol": symbol,
                    "token_precision": precision,
                }));
            }
        }
    }
    out
}

/// Parse an asset string "12.3456 SYM" → (base_units_string, symbol, precision). Base units = the
/// amount with the decimal point removed (exact integer), as a decimal string.
pub(crate) fn parse_asset(s: &str) -> Option<(String, String, i64)> {
    let (amount, symbol) = s.split_once(' ')?;
    if symbol.is_empty() {
        return None;
    }
    let (int_part, frac) = amount.split_once('.').unwrap_or((amount, ""));
    let base = format!("{int_part}{frac}").parse::<i128>().ok()?;
    Some((base.to_string(), symbol.to_string(), frac.len() as i64))
}

/// Parse a symbol spec "8,WAX" → (symbol, precision).
pub(crate) fn parse_symbol_spec(s: &str) -> Option<(String, i64)> {
    let (prec, sym) = s.split_once(',')?;
    Some((sym.to_string(), prec.parse::<i64>().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_asset_splits_base_units_and_precision() {
        assert_eq!(
            parse_asset("1.00000000 WAX"),
            Some(("100000000".into(), "WAX".into(), 8))
        );
        assert_eq!(
            parse_asset("0.5000 EOS"),
            Some(("5000".into(), "EOS".into(), 4))
        );
        assert_eq!(
            parse_asset("100 WAX"),
            Some(("100".into(), "WAX".into(), 0))
        );
        assert_eq!(parse_asset("garbage"), None);
    }

    #[test]
    fn parse_symbol_spec_splits_precision_and_symbol() {
        assert_eq!(parse_symbol_spec("8,WAX"), Some(("WAX".into(), 8)));
        assert_eq!(parse_symbol_spec("4,EOS"), Some(("EOS".into(), 4)));
        assert_eq!(parse_symbol_spec("WAX"), None);
    }

    #[test]
    fn extract_bytes_handles_uint8_array_and_hex() {
        assert_eq!(
            extract_bytes(Some(&json!([4, 21, 87, 255]))),
            vec![4u8, 21, 87, 255]
        );
        assert_eq!(
            extract_bytes(Some(&json!("0a0bff"))),
            vec![0x0a, 0x0b, 0xff]
        );
        assert_eq!(extract_bytes(None), Vec::<u8>::new());
        assert_eq!(extract_bytes(Some(&json!([]))), Vec::<u8>::new());
    }

    #[test]
    fn normalize_template_id_maps_minus_one_to_null() {
        assert_eq!(normalize_template_id(Some(&json!(123))), json!(123));
        assert_eq!(normalize_template_id(Some(&json!(-1))), Value::Null);
        assert_eq!(normalize_template_id(None), Value::Null);
    }

    #[test]
    fn merge_data_lets_mutable_win() {
        let mut imm = Map::new();
        imm.insert("name".into(), json!("orig"));
        imm.insert("rarity".into(), json!("common"));
        let mut mutbl = Map::new();
        mutbl.insert("name".into(), json!("renamed"));
        let merged = merge_data(&imm, &mutbl);
        assert_eq!(merged.get("name"), Some(&json!("renamed")));
        assert_eq!(merged.get("rarity"), Some(&json!("common")));
    }

    /// End-to-end-ish: a known schema format + a hand-built atomicdata blob → the asset `data` object.
    /// Blob: id=4 (format[0]="name":string) "Hi", id=5 (format[1]="level":uint16) 7.
    #[test]
    fn map_asset_decodes_immutable_data_against_registry() {
        let mut reg = SchemaRegistry::default();
        let fmt = vec![
            atomicdata::Field::new("name", "string"),
            atomicdata::Field::new("level", "uint16"),
        ];
        reg.schemas
            .entry("mycollection".into())
            .or_default()
            .insert("myschema".into(), fmt);

        // serialized_data as the uint8[] number array abieos would render:
        //   04            id=4 -> format[0] "name":string
        //   02 48 69      varuint len=2, "Hi"
        //   05            id=5 -> format[1] "level":uint16
        //   07            uint16 LEB128 = 7
        let blob = json!([0x04, 0x02, 0x48, 0x69, 0x05, 0x07]);
        let data = json!({
            "asset_id": "1099512961542",
            "collection_name": "mycollection",
            "schema_name": "myschema",
            "template_id": -1,
            "ram_payer": "owner",
            "backed_tokens": ["1.00000000 WAX"],
            "immutable_serialized_data": blob,
            "mutable_serialized_data": [],
        });
        let ctx = RowCtx {
            code: "atomicassets",
            scope: "alice",
            table: "assets",
            primary_key: "1099512961542".into(),
            payer: "owner",
            block_num: 42,
            token_ok: false,
            lightapi_fields: false,
        };
        let doc = map_asset(&ctx, data, &reg);
        assert_eq!(doc["owner"], json!("alice"));
        assert_eq!(doc["template_id"], Value::Null);
        assert_eq!(doc["data"]["name"], json!("Hi"));
        assert_eq!(doc["data"]["level"], json!(7));
        assert_eq!(doc["backed_tokens"][0]["amount"], json!("100000000"));
        assert_eq!(doc["backed_tokens"][0]["token_symbol"], json!("WAX"));
    }

    /// `float`/`double` attributes are decoded from `serialized_data` as NUMBERS for BOTH assets and
    /// templates (the schema-true value). The live API may show a float/double *asset* attr as a string
    /// when its `logmint` action used the attribute-map string variant — a mint-action detail absent
    /// from the snapshot's canonicalized `serialized_data`, so numbers is the correct decode here.
    #[test]
    fn map_asset_and_template_emit_float_as_number() {
        let mut reg = SchemaRegistry::default();
        let fmt = vec![
            atomicdata::Field::new("name", "string"),
            atomicdata::Field::new("score", "double"),
        ];
        reg.schemas
            .entry("c".into())
            .or_default()
            .insert("s".into(), fmt);

        // blob: id=4 name:string "Hi"; id=5 score:double 0.5 (IEEE-754 LE = 00 00 00 00 00 00 E0 3F)
        let blob =
            json!([0x04, 0x02, 0x48, 0x69, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xE0, 0x3F]);
        let ctx = RowCtx {
            code: "atomicassets",
            scope: "alice",
            table: "assets",
            primary_key: "1".into(),
            payer: "owner",
            block_num: 1,
            token_ok: false,
            lightapi_fields: false,
        };
        let asset = json!({
            "asset_id": "1", "collection_name": "c", "schema_name": "s",
            "template_id": -1, "ram_payer": "owner", "backed_tokens": [],
            "immutable_serialized_data": blob.clone(), "mutable_serialized_data": [],
        });
        let adoc = map_asset(&ctx, asset, &reg);
        assert_eq!(adoc["immutable_data"]["score"], json!(0.5));
        assert_eq!(adoc["data"]["score"], json!(0.5));
        assert_eq!(adoc["data"]["name"], json!("Hi"));

        // TEMPLATE (same blob/schema): also a number.
        let tctx = RowCtx {
            table: "templates",
            scope: "c",
            ..ctx
        };
        let tmpl =
            json!({ "template_id": 7, "schema_name": "s", "immutable_serialized_data": blob });
        let tdoc = map_template(&tctx, tmpl, &reg);
        assert_eq!(tdoc["immutable_data"]["score"], json!(0.5));
    }
}
