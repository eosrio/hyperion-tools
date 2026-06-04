//! AtomicAssets faceted-store table IDs + key functions for the `.wseg` segment.
//!
//! Extends the WormDB segment table namespace (Light-API used 0..=10: balances=0, accinfo=5,
//! token_holders=6, pub_keys=7, top_ram=8, top_stake=9, codehash=10) with the AtomicAssets state
//! store: a columnar forward store keyed by `asset_id` plus per-dimension inverted indexes (sorted
//! u64 posting lists), schema/template forward stores (so each template's immutable data is stored
//! once), and presorted orderings. The full design is in `benchmark/atomicassets/WORMDB_STORE_DESIGN.md`.
//!
//! Keys are read back in WormDB's Zig reader, so the hash + name encoding MUST match byte-for-byte
//! (`name::encode` ↔ `core/name.zig`, [`fnv1a64`] ↔ `name.zig` fnv1a64 / `builder::fnv1a64`).

use crate::name;

// ── Table IDs — POC subset (11..=20); 21+ reserved for range/cardinality/market (later phases) ──
/// Forward store: key = `asset_id` (u64), blob = compact asset record.
pub const TABLE_AA_FWD: u32 = 11;
/// Inverted: key = `name::encode(owner)`, blob = ascending `asset_id` posting list.
pub const TABLE_AA_BY_OWNER: u32 = 12;
/// Inverted: key = `name::encode(collection_name)`, blob = posting list.
pub const TABLE_AA_BY_COLL: u32 = 13;
/// Inverted: key = [`coll_schema_key`], blob = posting list.
pub const TABLE_AA_BY_SCHEMA: u32 = 14;
/// Inverted: key = `template_id` (u64), blob = posting list.
pub const TABLE_AA_BY_TMPL: u32 = 15;
/// Inverted: key = [`data_attr_key`], blob = posting list (the `data:field=value` filter).
pub const TABLE_AA_DATA_ATTR: u32 = 16;
/// Schema formats: key = [`coll_schema_key`], blob = compact field list.
pub const TABLE_AA_SCHEMAS: u32 = 17;
/// Presorted: sentinel key [`SENTINEL_KEY`], blob = `asset_id`s descending (≈ mint order).
pub const TABLE_AA_SORTED_ID: u32 = 18;
/// Template forward store: key = `template_id` (u64), blob = compact template record (immutable data).
pub const TABLE_AA_TMPL_FWD: u32 = 19;
/// Collection forward store: key = `name::encode(collection_name)`, blob = compact collection record.
pub const TABLE_AA_COLL_FWD: u32 = 20;

/// The single-entry key used by presorted-ordering tables (one blob, looked up by a fixed key).
pub const SENTINEL_KEY: u64 = 0;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV1a-64 over a sequence of byte segments joined by a single NUL (`0x00`) between segments — i.e.
/// `fnv1a64_joined(&[a, b])` hashes `a \0 b`. A single segment hashes identically to the existing
/// `builder::fnv1a64(s)` (verified in tests), so this is the same hash the WormDB reader uses.
pub fn fnv1a64_joined(parts: &[&[u8]]) -> u64 {
    let mut h = FNV_OFFSET;
    let mut mix = |b: u8| {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    };
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            mix(0x00); // NUL separator between segments
        }
        for &b in *p {
            mix(b);
        }
    }
    h
}

/// Inverted-index / schema-format key for a `(collection, schema)` pair.
pub fn coll_schema_key(collection: &str, schema: &str) -> u64 {
    fnv1a64_joined(&[collection.as_bytes(), schema.as_bytes()])
}

/// Inverted-index key for a decoded data attribute filter `data:field=value`, scoped to the
/// `(collection, schema)` it belongs to. `value` is the canonical JSON string form the API filters on
/// (numbers as their decimal string, bool as `"0"`/`"1"`, strings verbatim) — matching
/// eosio-contract-api's `sales_filters` `filter TEXT[]` `d:key=value` convention.
pub fn data_attr_key(collection: &str, schema: &str, field: &str, value: &str) -> u64 {
    fnv1a64_joined(&[
        collection.as_bytes(),
        schema.as_bytes(),
        field.as_bytes(),
        value.as_bytes(),
    ])
}

/// Inverted-index key for an owner account (`TABLE_AA_BY_OWNER`).
pub fn owner_key(owner: &str) -> u64 {
    name::encode(owner)
}

/// Inverted-index / forward-store key for a collection (`TABLE_AA_BY_COLL` / `TABLE_AA_COLL_FWD`).
pub fn collection_key(collection: &str) -> u64 {
    name::encode(collection)
}

/// Forward-store / inverted-index key for a template. `template_id` is the on-chain i32; callers
/// should only key templated assets (`template_id >= 0`), never the no-template sentinel `-1`.
pub fn template_key(template_id: i64) -> u64 {
    template_id as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_single_segment_matches_existing_hash() {
        // A single segment must hash identically to builder::fnv1a64 (exposed via key_hash), so keys
        // written here line up with WormDB's reader.
        for s in ["eosio", "atomicassets", "WAX", "rarity=Mythic", ""] {
            assert_eq!(fnv1a64_joined(&[s.as_bytes()]), crate::builder::key_hash(s));
        }
    }

    #[test]
    fn joined_differs_from_concatenation() {
        // The NUL separator must matter: ("ab","c") != ("a","bc") and != "abc".
        assert_ne!(
            fnv1a64_joined(&[b"ab", b"c"]),
            fnv1a64_joined(&[b"a", b"bc"])
        );
        assert_ne!(fnv1a64_joined(&[b"ab", b"c"]), fnv1a64_joined(&[b"abc"]));
    }

    #[test]
    fn key_functions_are_deterministic_and_distinct() {
        assert_eq!(
            coll_schema_key("mycol", "mysch"),
            coll_schema_key("mycol", "mysch")
        );
        assert_eq!(
            data_attr_key("mycol", "mysch", "rarity", "Mythic"),
            data_attr_key("mycol", "mysch", "rarity", "Mythic")
        );
        // Different field/value/schema → different key.
        assert_ne!(
            data_attr_key("mycol", "mysch", "rarity", "Mythic"),
            data_attr_key("mycol", "mysch", "rarity", "Common")
        );
        assert_ne!(
            data_attr_key("mycol", "mysch", "rarity", "Mythic"),
            data_attr_key("mycol", "other", "rarity", "Mythic")
        );
        // owner/collection keys are antelope names; template key is the raw id.
        assert_eq!(owner_key("waxupbitcold"), name::encode("waxupbitcold"));
        assert_eq!(template_key(123), 123u64);
    }

    #[test]
    fn table_ids_are_unique() {
        let ids = [
            TABLE_AA_FWD,
            TABLE_AA_BY_OWNER,
            TABLE_AA_BY_COLL,
            TABLE_AA_BY_SCHEMA,
            TABLE_AA_BY_TMPL,
            TABLE_AA_DATA_ATTR,
            TABLE_AA_SCHEMAS,
            TABLE_AA_SORTED_ID,
            TABLE_AA_TMPL_FWD,
            TABLE_AA_COLL_FWD,
        ];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            assert!(seen.insert(id), "duplicate table id {id}");
            assert!(
                id > 10,
                "table id {id} collides with Light-API namespace 0..=10"
            );
        }
    }
}
