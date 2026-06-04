I now have complete grounding in every relevant file. Here is the full blueprint.

---

# WormDB AtomicAssets Faceted-Query State Store — Implementation Blueprint

## Ground Truth References

All code extensions anchor to these files:
- `/P:/eosrio/hyperion-tools/crates/wseg-build/src/wseg.rs` — container writer, `IndexEntry`, `Table`, `write_segment`
- `/P:/eosrio/hyperion-tools/crates/wseg-build/src/builder.rs` — `Builder` pattern, `fnv1a64`, `token_key`, presorted sentinel blobs (lines 455–519)
- `/P:/eosrio/hyperion-tools/crates/wseg-build/src/binfmt.rs` — compact binary record pattern to mirror for the forward store
- `/P:/eosrio/hyperion-tools/crates/atomicdata/src/lib.rs` — `Field`, `deserialize`, attribute type system
- `/P:/eosrio/hyperion-tools/crates/snapshot-load/src/atomicassets.rs` — `SchemaRegistry`, `build_schema_registry`, `map_asset`, `map_template`
- `/P:/eosrio/hyperion-tools/crates/snapshot-load/src/main.rs` lines 1253–1302 — the `items_tx` sink channel to extend
- `/P:/eosrio/hyperion-tools/crates/snapshot-load/src/mongo.rs` lines 476–512 — the index set to replicate in wseg form

---

## 1. SEGMENT FORMAT

### 1.1 Table ID Assignment (extending the existing namespace)

Existing: `TABLE_BALANCES=0`, `TABLE_ACCINFO=5`, `TABLE_TOKEN_HOLDERS=6`, `TABLE_PUB_KEYS=7`, `TABLE_TOP_RAM=8`, `TABLE_TOP_STAKE=9`, `TABLE_CODEHASH=10`.

New AA tables to add as constants in a new file `crates/wseg-build/src/aa_tables.rs`:

```
TABLE_AA_FWD          = 11   forward store:  key=asset_id u64,           blob=compact asset record
TABLE_AA_BY_OWNER     = 12   inverted:       key=name::encode(owner),    blob=posting list
TABLE_AA_BY_COLL      = 13   inverted:       key=name::encode(coll),     blob=posting list
TABLE_AA_BY_SCHEMA    = 14   inverted:       key=fnv1a64("coll:schema"), blob=posting list
TABLE_AA_BY_TMPL      = 15   inverted:       key=template_id as u64      blob=posting list
TABLE_AA_DATA_ATTR    = 16   inverted:       key=fnv1a64("coll:schema:field:val"), blob=posting list
TABLE_AA_SCHEMAS      = 17   schema formats: key=fnv1a64("coll:schema"), blob=compact field list
TABLE_AA_SORTED_ID    = 18   presorted:      sentinel key=0,             blob=sorted asset_ids desc (≈ mint order)
TABLE_AA_SORTED_UPD   = 19   presorted:      sentinel key=0,             blob=asset_ids sorted by updated_at_time desc
TABLE_AA_SORTED_TMPL  = 20   presorted:      sentinel key=0,             blob=(template_mint u32, asset_id u64) pairs asc
TABLE_AA_CARDINALITY  = 21   cardinality:    key=table_id u64,           blob=[u32 count] per-dimension/per-collection stats
TABLE_AA_TMPL_FWD     = 22   template fwd:   key=template_id as u64,     blob=compact template record
TABLE_AA_COLL_FWD     = 23   collection fwd: key=name::encode(coll),     blob=compact collection record
```

AtomicMarket tables (Phase 2):

```
TABLE_AM_SALE_FWD     = 30   forward:        key=sale_id u64,            blob=compact sale record (denormalized)
TABLE_AM_BY_SELLER    = 31   inverted:       key=name::encode(seller),   blob=posting list
TABLE_AM_BY_COLL      = 32   inverted:       key=name::encode(coll),     blob=posting list
TABLE_AM_BY_STATE     = 33   inverted:       key=state as u64,           blob=posting list
TABLE_AM_PRICE_IDX    = 34   range:          key=price_units u64,        blob=posting list
TABLE_AM_SORTED_PRICE = 35   presorted:      sentinel 0,                 blob=(price u64, sale_id u64) pairs
TABLE_AM_SORTED_CRTD  = 36   presorted:      sentinel 0,                 blob=sale_ids desc by created_at
```

The `.wseg` header `table_count` field accommodates up to `u32::MAX` tables; the format extension is zero-cost: `write_segment` in `wseg.rs` at line 40 sorts and writes whatever `Vec<Table>` it receives. No format version bump is needed.

### 1.2 Forward Store Blob (TABLE_AA_FWD, table 11)

Key: `asset_id.parse::<u64>()` from the JSON string the abieos decode yields.

Blob layout (little-endian, written by `aa_binfmt::encode_asset`):

```
u8   version = 0x01
u64  owner              (name::encode)
u64  collection_name    (name::encode)
u64  schema_name        (name::encode)
i32  template_id        (-1 = no template)
u32  block_num
u8   flags              bit0=has_backed_tokens, bit1=has_mutable_data
[has_backed_tokens]
  u8   count
  per token: u64 symbol_code | i64 units
[has_mutable_data]
  u16  nattrs
  per attr: u8 schema_field_idx | u8 type_tag | [value bytes, type-dependent]
```

Mutable data attributes are stored using the schema field index (0-based) as the key — same sparseness model as `atomicdata`'s `identifier = index + RESERVED` — so the server can decode without a separate schema fetch IF it already has the field list. The `TABLE_AA_SCHEMAS` blob (table 17) is loaded into memory at startup for exactly this.

Immutable data is NOT stored in the forward blob. It is stored once in the template forward blob (TABLE_AA_TMPL_FWD, table 22) and joined at request time via `template_id`. For assets without a template (template_id = -1), immutable data IS stored in the asset blob using the same attr encoding (flag bit 2 = `has_immutable_data`).

### 1.3 Posting List Blob Format

Used by tables 12–16 (and 31–33 for market). Written by `aa_binfmt::encode_posting_list`:

```
u32  count
u64  asset_id[0]   (sorted ascending)
u64  asset_id[1]
...
u64  asset_id[count-1]
```

Total bytes: `4 + count * 8`. For owner "waxburnervx" with 1M assets: 8 MB per posting-list blob. The `IndexEntry.len` is u32, capping a single blob at 4 GiB (536M u64s). WAX has 232M live assets; the single largest owner at WAX is nowhere near that ceiling.

For Phase 2 (>500M assets or a single pathological owner), introduce run-splitting: two `IndexEntry` rows for the same key pointing to consecutive blob ranges, detected at query time by scanning all entries for that key before intersecting. This requires no format change — `write_segment` already allows duplicate keys and the binary search can be extended to find all matching entries.

### 1.4 Data Attribute Inverted Index Key (TABLE_AA_DATA_ATTR, table 16)

Key function in `aa_tables.rs`:

```rust
pub fn data_attr_key(collection: &str, schema: &str, field: &str, value_str: &str) -> u64 {
    // FNV1a-64 of "collection\0schema\0field\0value"
    fnv1a64_multi(&[collection.as_bytes(), b"\x00",
                    schema.as_bytes(),     b"\x00",
                    field.as_bytes(),      b"\x00",
                    value_str.as_bytes()])
}
```

The value_str is the canonical JSON string representation produced by `atomicdata::deserialize`: numbers become their decimal string, booleans "0"/"1", strings their UTF-8 value. This matches eosio-contract-api's `filter TEXT[]` convention `d:key=value` directly — the server hashes the same tuple.

For numeric range queries (template_mint, price), a separate TABLE_AA_RANGE_TMPL (table 24) uses `key = template_mint as u64` (u32 cast), one posting list per distinct template_mint value. The binary search gives range start; forward scan to the end key gives the range. For price range (AtomicMarket), TABLE_AM_PRICE_IDX (table 34) uses `key = price_units as u64`.

### 1.5 Schema Format Blob (TABLE_AA_SCHEMAS, table 17)

Key: `fnv1a64("collection\0schema")` (null byte separator, same pattern as the forward-store data attr keys).

Blob written by `aa_binfmt::encode_schema_format`:

```
u16  nfields
per field:
  u8  type_tag    (see enum below)
  u8  name_len
  u8  name[name_len]
```

`type_tag` enum (fits u8, covers all `atomicdata` types):

```
0=int8 1=int16 2=int32 3=int64
4=uint8 5=uint16 6=uint32 7=uint64
8=fixed8 9=fixed16 10=fixed32 11=fixed64
12=byte 13=bool 14=float 15=double
16=string 17=image 18=ipfs 19=bytes
128..= → bit7 set = array of (tag & 0x7F)
```

This blob is read at server startup into a `HashMap<u64, Vec<(u8 type_tag, String field_name)>>` for O(1) schema lookup. The entire schema dataset is ~80k schemas × avg 6 fields × ~12 bytes/field = ~6 MB in memory.

### 1.6 Presorted Ordering Blobs (Tables 18–20)

Modeled directly on `top_ram_tbl`/`top_stake_tbl` in `builder.rs` lines 455–519. Sentinel key 0, one blob per table.

TABLE_AA_SORTED_ID (table 18) — assets by asset_id descending (serves `sort=asset_id` and `sort=minted`):
```
u32  count
u64  asset_id[0]   (desc: largest first = most recently minted first)
...
```

TABLE_AA_SORTED_UPD (table 19) — assets by updated_at_time descending. Since snapshots lack updated_at_time (H field), this presorted table is left empty in the snapshot-built segment and populated only by the SHiP delta overlay (Phase 2).

TABLE_AA_SORTED_TMPL (table 20) — assets by template_mint ascending:
```
u32  count
per entry: u32 template_mint | u64 asset_id
```
Stride = 12 bytes. Server slices `[offset*12 .. (offset+limit)*12]` and reads (template_mint, asset_id) pairs.

### 1.7 Cardinality Stats (TABLE_AA_CARDINALITY, table 21)

One `IndexEntry` per collection (key = `name::encode(collection)`), blob:
```
u32  total_assets
u32  schemas_count
// per schema in this collection:
u16  nschemas
per schema: u64 schema_key | u32 asset_count
```

This is the WormDB equivalent of `getSchemaAssetCount` (the eosio-contract-api 75k-asset guard that prevents full-table scans when only a weak filter is present). The server checks this table before executing a query and returns a 400/hint if no strong filter would reduce the scan below the threshold.

---

## 2. QUERY EXECUTION

### 2.1 Single-Filter Lookup

Every single-dimension filter follows the existing WormDB pattern: binary search over the table's sorted `IndexEntry` array → one `(off, len)` → slice into arena.

For exact-match dimensions (owner, collection, schema, template): one binary search in the relevant inverted-index table → posting list blob → `[u32 count][u64 asset_id...]`.

For data attribute exact match (`data:rarity=legendary`): compute `data_attr_key("collection", "schema", "rarity", "legendary")` → binary search in TABLE_AA_DATA_ATTR → posting list.

### 2.2 Multi-Filter Intersection

Given N active filters with posting lists P1, P2, ..., Pn (each a sorted ascending u64 slice):

```
fn intersect(lists: &[&[u64]]) -> Vec<u64> {
    // Sort lists by length ascending (smallest first reduces work)
    let mut sorted = lists.to_vec();
    sorted.sort_by_key(|l| l.len());
    let mut result: &[u64] = sorted[0];
    let mut buf: Vec<u64> = Vec::new();
    for next in &sorted[1..] {
        buf.clear();
        // O(|A|+|B|) merge-sort intersection
        let (mut i, mut j) = (0, 0);
        while i < result.len() && j < next.len() {
            match result[i].cmp(&next[j]) {
                Ordering::Equal => { buf.push(result[i]); i+=1; j+=1; }
                Ordering::Less  => i += 1,
                Ordering::Greater => j += 1,
            }
        }
        result = &buf; // continue with the shrunk set
    }
    result.to_vec()
}
```

This is pure Zig on the WormDB server side. The Rust side only builds the posting lists; the intersection logic lives entirely in the Zig reader.

For OR within a dimension (list[id] filter like `owner=A,B,C`): fetch each posting list, union-merge them (sorted merge), then intersect with other dimension results.

### 2.3 Range Queries (template_mint, price)

Binary search TABLE_AA_RANGE_TMPL for the lower bound key → forward scan until the upper bound key. Each key's posting list contains assets with that exact template_mint. Collect all posting lists in the range, flatten to a single sorted u64 array, then intersect with other active filters.

For open-ended range (`min_template_mint` only): scan from lower bound to end of table index. The `IndexEntry` array is sorted by key, so forward scan is O(distinct values in range × binary search entry size).

For the `is_burned` boolean filter: a dedicated TABLE_AA_BY_BURNED (an additional inverted table where key=1 holds all burned asset_ids) follows the same posting-list pattern.

### 2.4 Sorted Output + Skip+Take

After intersection, the result set is a `Vec<u64>` of matching asset_ids. For `sort=asset_id desc` (most common): the posting lists are already sorted ascending, so reverse the result in-place and slice `[offset..offset+limit]`.

For `sort=template_mint asc`: intersect then sort the result set by template_mint. The template_mint is not directly in the forward blob; use TABLE_AA_SORTED_TMPL: the presorted ordering contains ALL asset_ids with their template_mints. For small result sets (<10k), look up each asset's template_mint from the forward blob (8-byte random access after binary search). For large result sets, do a parallel scan of TABLE_AA_SORTED_TMPL and extract only matching asset_ids (this is the merge approach).

Practical approach for POC: for `sort != asset_id`, apply the filter first (intersection → result_set), then for each asset_id in result_set, look up template_mint from the forward blob (binary search + blob decode of the first 24 bytes), then sort in-process. This is O(|result_set| × log(N_assets)) and is fast enough for result sets under 100k. The presorted TABLE_AA_SORTED_TMPL is for the zero-filter presorted case (browse all assets sorted by template_mint without any filter).

### 2.5 In-Process Template/Schema/Collection Join

After intersection + pagination produces at most `limit` (typically ≤100) asset_ids, assemble each response document:

1. Look up forward blob (TABLE_AA_FWD): decode owner, collection, schema, template_id, backed_tokens, mutable_data.
2. Look up template forward blob (TABLE_AA_TMPL_FWD): decode immutable_data for this template_id. Cache recently-seen template blobs in a per-request LRU (templates are shared across many assets in the same request).
3. Look up schema from startup-loaded in-memory HashMap: get `Vec<Field>` for decoding the binary attribute blobs.
4. Merge mutable_data over immutable_data: `data = immutable.clone(); data.extend(mutable)` (mutable wins, matching `merge_data` in `atomicassets.rs` line 288).
5. Look up collection forward blob (TABLE_AA_COLL_FWD) if `collection_name` fields needed in response.

All look-ups are mmap binary searches — zero heap allocation except for the final response JSON. A 100-asset response needs at most 100 forward lookups + at most 100 template lookups (usually far fewer, as assets in a query result typically share a template) + 80k startup schema load.

### 2.6 Offers and AtomicMarket

Offers (TABLE_AA_OFFERS_FWD, table 25): forward store keyed by offer_id. Inverted indexes TABLE_AA_OFFER_BY_SENDER (table 26), TABLE_AA_OFFER_BY_RECV (table 27), TABLE_AA_OFFER_BY_ASSET (table 28) follow the same posting-list pattern.

AtomicMarket sales (TABLE_AM_SALE_FWD, table 30): forward store carries a **denormalized** snapshot of asset-derived facets at listing time: `collection_name`, `schema_name`, `template_id`, `template_mint`, owner, name (from `data.name`). This mirrors eosio-contract-api's `atomicmarket_sales_filters_listed` materialized view — denormalization avoids a join at query time for the most common filter (`collection_name + state`). Price is stored as raw units (i64) alongside the settlement symbol.

---

## 3. BUILD PIPELINE

### 3.1 New Files to Create

`crates/wseg-build/src/aa_tables.rs` — table ID constants + key functions:
- `TABLE_AA_FWD` through `TABLE_AM_SORTED_CRTD` constants
- `fn data_attr_key(coll, schema, field, val) -> u64`
- `fn schema_key(coll, schema) -> u64` = `fnv1a64_multi(&[coll, "\0", schema])`
- `fn asset_id_from_str(s: &str) -> Option<u64>` wrapping `s.parse::<u64>()`
- `fn template_id_to_key(t: i32) -> u64` (sentinel: `u64::MAX` for -1)

`crates/wseg-build/src/aa_binfmt.rs` — compact encoding functions:
- `fn encode_asset(out, owner, coll, schema, template_id, block_num, backed_tokens, mutable_attrs, immutable_attrs_if_no_tmpl)` — mirrors `binfmt::encode` pattern
- `fn encode_template(out, coll, schema, template_id, transferable, burnable, max_supply, issued_supply, immutable_attrs)`
- `fn encode_collection(out, coll, author, authorized_accts, market_fee, data_attrs)`
- `fn encode_schema_format(out, fields: &[Field])` — u16 count + per-field (type_tag, name_len, name)
- `fn encode_posting_list(out, ids: &mut Vec<u64>)` — sorts in-place then writes `[u32 count][u64...]`

`crates/wseg-build/src/aa_builder.rs` — the `AtomicBuilder` struct:

```rust
pub struct AtomicBuilder {
    // Forward stores
    fwd:        HashMap<u64, Vec<u8>>,   // asset_id -> blob
    tmpl_fwd:   HashMap<u64, Vec<u8>>,   // template_id -> blob
    coll_fwd:   HashMap<u64, Vec<u8>>,   // name::encode(coll) -> blob
    // Inverted indexes
    by_owner:   HashMap<u64, Vec<u64>>,  // name::encode(owner) -> [asset_id]
    by_coll:    HashMap<u64, Vec<u64>>,  // name::encode(coll) -> [asset_id]
    by_schema:  HashMap<u64, Vec<u64>>,  // schema_key -> [asset_id]
    by_tmpl:    HashMap<u64, Vec<u64>>,  // template_id_to_key -> [asset_id]
    data_attr:  HashMap<u64, Vec<u64>>,  // data_attr_key -> [asset_id]
    range_tmpl: HashMap<u64, Vec<u64>>,  // template_mint as u64 -> [asset_id]
    // Schema formats
    schema_blobs: HashMap<u64, Vec<u8>>, // schema_key -> encoded format blob
    // Presorted
    sorted_id:  Vec<u64>,                // all asset_ids, sorted at finish
    sorted_tmpl: Vec<(u32, u64)>,        // (template_mint, asset_id), sorted at finish
    // Cardinality: per-collection asset count
    coll_counts: HashMap<u64, u32>,
    pub rows: u64,
}
```

Methods:

- `fn push_schema(&mut self, doc: &Document)` — extract collection_name + schema_name + format; encode format blob into `schema_blobs`.
- `fn push_collection(&mut self, doc: &Document, reg: &SchemaRegistry)` — decode `serialized_data` via reg.collection_format; encode into `coll_fwd`.
- `fn push_template(&mut self, doc: &Document, reg: &SchemaRegistry)` — decode immutable_data; insert into `tmpl_fwd`.
- `fn push_asset(&mut self, doc: &Document, reg: &SchemaRegistry)` — the hot path:
  1. Parse asset_id, owner, collection, schema, template_id from the already-decoded MongoDB document (the same `map_asset` output shape).
  2. Decode mutable_data attributes via `reg.format_for(collection, schema)`.
  3. Call `encode_asset` → append blob to `fwd`.
  4. Insert asset_id into `by_owner[name::encode(owner)]`, `by_coll[name::encode(coll)]`, `by_schema[schema_key(coll,schema)]`, `by_tmpl[template_id_to_key(template_id)]`.
  5. For each mutable data attribute `(field, value)`: `data_attr[data_attr_key(coll, schema, field, &value.to_string())].push(asset_id)`.
  6. For template_mint (if not -1 and not 0): `range_tmpl[template_mint as u64].push(asset_id)`.
  7. `sorted_id.push(asset_id)`.
  8. `sorted_tmpl.push((template_mint as u32, asset_id))` if template_mint > 0.
  9. `coll_counts[name::encode(coll)] += 1`.
- `fn push(&mut self, coll: &str, doc: &Document, reg: &SchemaRegistry)` — routes by collection name.
- `fn finish(mut self, out: &str) -> io::Result<Stats>` — see §3.2.

### 3.2 `AtomicBuilder::finish`

```
1. Sort and encode each posting list:
   - For each HashMap<u64, Vec<u64>>: sort the Vec, call encode_posting_list, build IndexEntry.
   - Same drain pattern as builder.rs lines 387–411 (token_holders).

2. Presorted orderings:
   - sorted_id: sort desc (reverse), write [u32 count][u64...] to arena → TABLE_AA_SORTED_ID blob.
   - sorted_tmpl: sort by (template_mint asc, asset_id asc), write [u32 count][u32 tmpl | u64 id...] → TABLE_AA_SORTED_TMPL.

3. Cardinality table:
   - Per collection: emit a blob with total count + per-schema breakdown from coll_counts.

4. Assemble Vec<Table>:
   TABLE_AA_FWD        { fwd index + arena }
   TABLE_AA_BY_OWNER   { by_owner index + arena }
   TABLE_AA_BY_COLL    { by_coll index + arena }
   TABLE_AA_BY_SCHEMA  { by_schema index + arena }
   TABLE_AA_BY_TMPL    { by_tmpl index + arena }
   TABLE_AA_DATA_ATTR  { data_attr index + arena }
   TABLE_AA_SCHEMAS    { schema_blobs index + arena }
   TABLE_AA_SORTED_ID  { sentinel-0 index + arena }
   TABLE_AA_SORTED_TMPL{ sentinel-0 index + arena }
   TABLE_AA_CARDINALITY{ coll_counts index + arena }
   TABLE_AA_TMPL_FWD   { tmpl_fwd index + arena }
   TABLE_AA_COLL_FWD   { coll_fwd index + arena }

5. Call write_segment(out, tables) — zero change to wseg.rs.
```

### 3.3 Wiring into snapshot-load

`crates/snapshot-load/src/main.rs` — the `--wseg` branch (lines 1253–1302) currently sets `Arc::new(atomicassets::SchemaRegistry::default())` with the comment `// --wseg is Light-API only`.

Changes needed:

1. Add a new CLI flag `--wseg-atomic` (or extend `--wseg` with a `--tables atomic` combination check): when `table_specs` contains AtomicAssets tables AND `--wseg` is set, build both a `Builder` (for Light-API tables) AND an `AtomicBuilder` (for AA tables), each receiving items from the same channel by collection name.

2. Call `build_schema_registry` before the pipeline starts (it is already called for the Mongo path; extend the `--wseg` branch to run it too).

3. The sink thread receives `(coll, doc)` pairs. Route:
   - `"accounts"`, `"permissions"`, `"eosio-userres"`, `"eosio-delband"`, `"account_codehash"` → existing `Builder.push`.
   - `"atomicassets-schemas"`, `"atomicassets-collections"`, `"atomicassets-templates"`, `"atomicassets-assets"`, `"atomicassets-offers"` → `AtomicBuilder.push(coll, &doc, &reg)`.

4. At finish: both builders write their respective output files (or the AtomicBuilder merges its `Vec<Table>` with the light-API tables into one combined `.wseg` file — simpler for deployment, no format change required since `write_segment` handles any mix of table IDs).

The `run_pipeline` call at line 1278 already handles AtomicAssets collections when `table_specs` includes `"atomic"` — those items flow through `items_tx` to the sink thread. The only code-change in `main.rs` is lifting the `SchemaRegistry::default()` guard and adding the `AtomicBuilder` alongside the existing `Builder` in the sink thread.

### 3.4 Alternative Source: From Mongo State

`crates/wseg-build/src/main.rs` (the standalone `wseg-build` binary) can be extended with an `--tables atomic` flag: stream from the `atomicassets-*` Mongo collections in the same order (schemas first, then collections + templates, then assets — since schemas must be loaded before assets can have their data decoded).

---

## 4. FRESHNESS: DELTA OVERLAY

### 4.1 Frozen Segment + Delta File

The built `.wseg` is a frozen snapshot (immutable after write). Freshness is maintained by a separate **delta file** (`aa-delta.wseg` or a simple append-log) that records changes since the segment was built. The WormDB server merges the delta at query time: for a given asset_id, check the delta first (it is a small in-memory HashMap keyed by asset_id, holding the current forward blob); if absent, fall through to the frozen segment.

Delta format: a simple NDJSON or a small in-memory `HashMap<u64, Vec<u8>>` (forward store) + `HashMap<u64, DeltaPostingList>` (per-dimension inverted additions/removals). Given the segment is rebuilt weekly/nightly, the delta stays small.

### 4.2 SHiP Feed Integration

The Hyperion SHiP indexer processes `contract_row` deltas for `atomicassets:assets`, `atomicassets:templates`, etc. The same `map_asset`/`map_template` functions already used by `snapshot-load` are called on each incoming delta. The decoded document is passed to a live `AtomicBuilder` accumulating the delta:

- On `operation=insert`: add to forward store + all inverted indexes.
- On `operation=remove`: mark the forward blob as deleted (a tombstone byte) + remove from inverted indexes (requires storing the previous inverted index entries, so the delta builder also maintains a `prev_values: HashMap<u64, AssetKey>` recording owner/coll/schema/template_id of the previous state).
- On `operation=modify` (owner change on transfer): remove old owner posting, add new owner posting.

### 4.3 Re-Segment Trigger

Nightly (or on-demand after accumulating >100k delta entries): rebuild the full segment from the current Mongo state. The `AtomicBuilder::finish` call takes ~2–3 minutes for 232M assets (estimated: ~5 μs/asset for encoding + sorting). Swap the frozen file atomically (rename). The delta resets to empty.

---

## 5. EFFICIENCY PROOF PLAN

### 5.1 Minimal POC — Build First

**Scope**: Forward store (TABLE_AA_FWD) + four inverted indexes (TABLE_AA_BY_OWNER, TABLE_AA_BY_COLL, TABLE_AA_BY_SCHEMA, TABLE_AA_BY_TMPL) + two data-attribute indexes for the two most common data fields across WAX (`rarity` string + `name` string) + TABLE_AA_SORTED_ID presorted ordering. Schemas + templates as forward stores (TABLE_AA_SCHEMAS, TABLE_AA_TMPL_FWD).

This covers the query surface for: "assets by owner", "assets in collection", "assets with schema", "assets with template", "assets with rarity=X", "assets named Y", point-lookup by asset_id, browse sorted by mint order.

**Implementation checklist for POC**:

- [ ] `crates/wseg-build/src/aa_tables.rs` — constants + key functions (table IDs 11–18 only for POC)
- [ ] `crates/wseg-build/src/aa_binfmt.rs` — `encode_asset`, `encode_template`, `encode_schema_format`, `encode_posting_list`
- [ ] `crates/wseg-build/src/aa_builder.rs` — `AtomicBuilder` with `push_asset`, `push_template`, `push_schema`, `finish`
- [ ] Extend `crates/wseg-build/src/lib.rs` to `pub mod aa_tables; pub mod aa_binfmt; pub mod aa_builder;`
- [ ] Wire `AtomicBuilder` into the `--wseg` path of `crates/snapshot-load/src/main.rs` (lift `SchemaRegistry::default()` guard at line 1282, add `AtomicBuilder` to sink thread)
- [ ] Add `cargo test` fixture for `encode_asset` + `encode_posting_list` round-trip
- [ ] Run on the existing 89.6M WAX testnet snapshot already decoded into Mongo (the benchmark/validate harness) to get build time + output size
- [ ] Write a minimal Rust reader bin (`crates/wseg-build/src/aa_probe.rs`) that mmap-opens the AA wseg, exercises 5 query patterns, and measures latency — no Zig required for the POC measurement

### 5.2 Exact Measurements to Take

Run these against the WAX mainnet snapshot data (232.3M assets, 906k templates, 80k schemas, 110k collections):

| Measurement | How to measure | Baseline to beat |
|---|---|---|
| On-disk size of AA wseg (total) | `stat` the file | Mongo AA collections: ~8 GB total (from REQUIREMENTS.md §9); Postgres: 692 GB AA tables |
| On-disk size breakdown: fwd store vs inverted indexes vs sorted orderings | Sum the `blob_len` values from the wseg directory | Mongo `data.$**` wildcard index alone: ~5 GB |
| RSS at cold start (mmap, pages not yet faulted) | `/proc/PID/status VmRSS` after open + directory parse | Mongo mongod RSS for AA: ~GB range |
| RSS after 1000 representative queries (hot working set) | same, after query benchmark | target: tens of MiB |
| Build time from Mongo state (wseg-build) | wall clock, `cargo run --release -- --tables atomic` | — |
| Build time from snapshot (snapshot-load --wseg) | wall clock | — |
| Query latency — 5 representative queries (P50/P99, 1000 iterations each): | `aa_probe` tool, mmap binary | |
| Q1: point lookup by asset_id | binary search in TABLE_AA_FWD + template join | Mongo: ~1 ms; Postgres: ~2 ms |
| Q2: owner X assets, page 1, sort=asset_id desc, limit=100 | 1 posting list lookup + sort slice | target: <100 μs |
| Q3: collection Y + schema Z assets, data:rarity=Mythic, limit=100 | 3 inverted lookups + intersection | target: <500 μs |
| Q4: collection Y, sort=template_mint asc, limit=100 (weak filter, large set) | 1 inverted lookup (by_coll) + in-result sort | Mongo: ~50ms with wildcard; target: <5ms |
| Q5: browse all assets sorted by asset_id desc, page 500, limit=100 | presorted blob slice [offset..] | target: <10 μs |
| Intersection throughput for 2 large posting lists (1M × 500k) | microbenchmark in aa_probe | — |

### 5.3 Estimated Segment Sizes (232M assets)

Forward store (TABLE_AA_FWD): avg ~60 bytes/asset × 232M = ~14 GB raw, ~10 GB with typical sparseness (most assets have no mutable data and a template, so the forward blob is 1+8+8+8+4+4+1 = 34 bytes minimum + any backed tokens). Call it ~8–12 GB for the forward store.

Inverted indexes (4 structural dimensions): owner (~44M distinct, avg posting ~5 assets → 300 MB); collection (110k distinct, avg posting ~2100 → similar total); schema (80k distinct) + template (906k distinct). All four structural indexes combined: ~10–15 GB in blob arena (dominated by the posting lists themselves: 232M × 8 bytes = 1.8 GB minimum per dimension in raw u64s, plus 4-byte count headers).

Data attribute indexes (TABLE_AA_DATA_ATTR): depends entirely on the data schema distribution. The WAX `rarity` field across all schemas has maybe 20 distinct values × 232M/906k templates × avg assets per template ≈ large lists. Estimate: 2–4 GB for common string dimensions. This is the direct replacement for Mongo's ~5 GB `data.$**` wildcard index.

Schema + template forward stores: 906k templates × ~200 bytes/template = ~180 MB. Schema formats: ~6 MB.

Presorted orderings: 232M × 8 bytes = 1.8 GB for sorted_id; 232M × 12 bytes = 2.8 GB for sorted_tmpl. These are optional for the POC (only needed for zero-filter browse queries).

**Total estimate: 25–35 GB on disk for all tables.** This compares to 692 GB Postgres AA tables (which includes all history), but the honest comparison is Mongo AA state-only: the REQUIREMENTS.md §9 reports ~8 GB for 88.8M assets (9× compressed WiredTiger). Extrapolating linearly: ~21 GB for 232M assets in Mongo. The wseg uncompressed estimate of 25–35 GB is comparable to WiredTiger uncompressed; the mmap RSS working set (only the queried pages are faulted) is what delivers the footprint advantage — not raw on-disk size.

The **resident set advantage** is the key claim: a single query for "owner X, 100 assets" faults exactly one posting-list blob (at most 8 MB for a heavy owner) + 100 × 34-byte forward blobs = ~11 KB of forward data + one template blob per unique template. Total RSS for that query: under 10 MB of newly faulted pages. Mongo serving the same query requires the mongod process (multi-GB RSS for the index B-tree pages) to be warm.

### 5.4 Phased Build Sequence

**Phase 0 — Foundations (now, pre-POC)**
- [ ] `aa_tables.rs`: constants + key functions. No deps.
- [ ] `aa_binfmt.rs`: `encode_posting_list` + `encode_schema_format`. Unit-test round-trip.
- [ ] `aa_binfmt.rs`: `encode_asset`. Unit-test: encode → decode back to (owner, coll, schema, template_id).

**Phase 1 — POC build + measure (1–2 weeks)**
- [ ] `aa_builder.rs`: `AtomicBuilder` with `push_schema`, `push_template`, `push_asset`, `finish` (tables 11–18 only: fwd + 4 inverted + schemas + tmpl_fwd + sorted_id).
- [ ] Wire into `snapshot-load --wseg` path (lift `SchemaRegistry::default()` guard, add `AtomicBuilder` to sink).
- [ ] Build from WAX testnet Mongo (89.6M assets already loaded) → measure build time + on-disk size.
- [ ] `aa_probe` binary: mmap + execute Q1–Q5 queries → measure latency vs Mongo.
- [ ] Document the numbers in `benchmark/atomicassets/WSEG_RESULTS.md`.

**Phase 2 — Full dimension coverage**
- [ ] TABLE_AA_DATA_ATTR (table 16): index all decoded data attributes during `push_asset`. Requires iterating the merged `data` map.
- [ ] TABLE_AA_RANGE_TMPL (table 24): range-queryable template_mint index.
- [ ] TABLE_AA_CARDINALITY (table 21): per-collection asset counts for the weak-filter guard.
- [ ] TABLE_AA_SORTED_TMPL (table 20): presorted by template_mint.
- [ ] Offers support: TABLE_AA_OFFERS_FWD + inverted indexes.
- [ ] Run on WAX mainnet snapshot (232M assets) → headline numbers.

**Phase 3 — AtomicMarket + freshness**
- [ ] TABLE_AM_SALE_FWD + inverted + price range index (tables 30–36).
- [ ] Delta overlay: in-memory HashMap over the frozen segment, fed by SHiP.
- [ ] Nightly re-segment trigger.

**Phase 4 — WormDB Zig reader extension**
- [ ] Port `intersect` (sorted-set merge intersection) to Zig.
- [ ] Port `range_scan` (forward scan from binary search start key) to Zig.
- [ ] Port `decode_asset_blob` + `decode_template_blob` + schema startup load to Zig.
- [ ] Wire HTTP handlers in WormDB for `/atomicassets/assets`, `/atomicassets/templates`, etc., using the same `state-store interface` API shape Track A defines.

---

## 6. CRITICAL DETAILS

**Key collision risk in TABLE_AA_DATA_ATTR**: FNV1a-64 has a birthday-paradox collision probability of ~1 in 2^32 for 10^6 distinct (field, value) pairs. For WAX with ~20k distinct (collection, schema, field, value) tuples, collision probability is negligible. For Phase 2 with full data attribute indexing (potentially millions of distinct values), add a 2-byte "guard" inline in the blob header (first 2 bytes = `u16(fnv1a32(key_string))`) to detect false positives at query time. Same pattern as `token_holders` and `codehash` tables which store the header string precisely for this purpose.

**Posting list size bounds**: the `IndexEntry.len` field is `u32` (4 bytes), limiting a single posting list to 4 GiB = 536M u64 entries. WAX has 232M assets, so a single owner in the worst case can have all of them — this fits in one blob. However, arenas accumulate as `Vec<u8>`: at 232M × 8 bytes = 1.86 GB for the largest single posting list, the `Vec<u8>` arena itself must be pre-sized. Use `Vec::with_capacity(estimated_assets * 8 + 4)` in `AtomicBuilder::new()`.

**Build memory peak**: the `AtomicBuilder` holds all posting lists as `HashMap<u64, Vec<u64>>`. Peak memory before `finish` = (4 structural dimensions + N data dimensions) × 232M × 8 bytes. For 4 dimensions: 4 × 1.86 GB = 7.4 GB. For 10 data dimensions: 18.6 GB. This is the build-time constraint; the server itself only needs mmap (no heap). Consider a streaming build for data attributes: process assets in chunks, write partial posting lists to temp files, merge-sort at finish (analogous to external-merge-sort). For the POC with 4 structural dimensions only, 8–10 GB build RAM is acceptable.

**Atomicdata float/double rendering in posting-list keys**: the `data_attr_key` uses `value.to_string()` for numeric attributes. For float/double, this can differ between `f32::to_string` and `f64::to_string`. Pin to `format!("{:.prec$}", value, prec = precision)` using the schema's type to determine precision, matching eosio-contract-api's normalization. For the POC, skip float/double data attributes in the inverted index (they are rarely used as exact-match filters; range queries on numerics use TABLE_AA_RANGE_TMPL-style tables anyway).

**schema_name as u64 vs fnv1a64**: `schema_name` in Antelope is a `name` type (up to 12 chars, EOSIO name encoding). Use `name::encode(schema_name)` instead of `fnv1a64` for BY_SCHEMA keys when the schema_name is a valid Antelope name. This keeps consistency with the existing `name::encode` usage in `builder.rs` and allows the Zig reader to use the same `name.zig::encode` function it already has. For the composite `schema_key("coll:schema")` used in TABLE_AA_SCHEMAS and TABLE_AA_DATA_ATTR, continue using `fnv1a64`.

**Thread safety in AtomicBuilder**: the Builder is single-threaded (owned by the sink thread, same as the existing `Builder` in `main.rs` line 1258). The `run_pipeline` decode workers push into the `crossbeam_channel`; the sink thread serially calls `push` and `finish`. No locking needed.