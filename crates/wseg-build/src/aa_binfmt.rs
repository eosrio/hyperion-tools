//! Compact binary records for the AtomicAssets faceted store (the `.wseg` blob arenas).
//!
//! Four blob shapes, all little-endian:
//!  - **posting list** (inverted indexes): `u32 count | u64 asset_id × count` (ascending).
//!  - **schema format** (`TABLE_AA_SCHEMAS`): `u16 nfields | (u8 type_tag, u8 name_len, name) × n`.
//!  - **asset forward** (`TABLE_AA_FWD`): structural u64 name keys + sparse data attributes keyed by
//!    schema-field index (so field NAMES are never repeated per asset — the compactness win).
//!  - **template forward** (`TABLE_AA_TMPL_FWD`): a template's immutable attributes, stored ONCE.
//!
//! Data attribute values are stored as their canonical string form (the same string the API filters
//! on) keyed by `field_idx` — type-aware binary value packing is a later optimization.

// ── little-endian put/get helpers ────────────────────────────────────────────────────────────────
fn pu16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn pu32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn pu64(o: &mut Vec<u8>, v: u64) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn pi32(o: &mut Vec<u8>, v: i32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn gu16(b: &[u8], p: &mut usize) -> u16 {
    let v = u16::from_le_bytes(b[*p..*p + 2].try_into().unwrap());
    *p += 2;
    v
}
fn gu32(b: &[u8], p: &mut usize) -> u32 {
    let v = u32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
    *p += 4;
    v
}
fn gu64(b: &[u8], p: &mut usize) -> u64 {
    let v = u64::from_le_bytes(b[*p..*p + 8].try_into().unwrap());
    *p += 8;
    v
}
fn gi32(b: &[u8], p: &mut usize) -> i32 {
    let v = i32::from_le_bytes(b[*p..*p + 4].try_into().unwrap());
    *p += 4;
    v
}

// ── schema field type tags (atomicdata type system → u8; bit7 = array of the base type) ───────────
const ARRAY_BIT: u8 = 0x80;
const SCALAR_TYPES: [&str; 20] = [
    "int8", "int16", "int32", "int64", "uint8", "uint16", "uint32", "uint64", "fixed8", "fixed16",
    "fixed32", "fixed64", "byte", "bool", "float", "double", "string", "image", "ipfs", "bytes",
];

/// Map an atomicdata type string (`"uint16"`, `"string[]"`, …) to a 1-byte tag. Unknown → `string`.
pub fn type_tag(ty: &str) -> u8 {
    let (base, arr) = match ty.strip_suffix("[]") {
        Some(b) => (b, ARRAY_BIT),
        None => (ty, 0),
    };
    let idx = SCALAR_TYPES.iter().position(|t| *t == base).unwrap_or(16) as u8; // 16 = string
    idx | arr
}

/// Inverse of [`type_tag`].
pub fn tag_type(tag: u8) -> String {
    let base = SCALAR_TYPES
        .get((tag & !ARRAY_BIT) as usize)
        .copied()
        .unwrap_or("string");
    if tag & ARRAY_BIT != 0 {
        format!("{base}[]")
    } else {
        base.to_string()
    }
}

// ── schema format blob ───────────────────────────────────────────────────────────────────────────
/// Encode a schema `format` (ordered `(name, type)` fields) to its blob.
pub fn encode_schema_format(fields: &[(String, String)]) -> Vec<u8> {
    let mut o = Vec::with_capacity(2 + fields.len() * 12);
    pu16(&mut o, fields.len() as u16);
    for (name, ty) in fields {
        o.push(type_tag(ty));
        o.push(name.len().min(255) as u8);
        o.extend_from_slice(&name.as_bytes()[..name.len().min(255)]);
    }
    o
}

/// Decode a schema-format blob into `(name, type)` fields (order preserved → field index = position).
pub fn decode_schema_format(b: &[u8]) -> Vec<(String, String)> {
    let mut p = 0usize;
    let n = gu16(b, &mut p) as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let tag = b[p];
        p += 1;
        let len = b[p] as usize;
        p += 1;
        let name = String::from_utf8_lossy(&b[p..p + len]).into_owned();
        p += len;
        out.push((name, tag_type(tag)));
    }
    out
}

// ── posting list blob ────────────────────────────────────────────────────────────────────────────
/// Encode a posting list: sorts + dedups `ids` in place, then `u32 count | u64 × count`.
pub fn encode_posting_list(ids: &mut Vec<u64>) -> Vec<u8> {
    ids.sort_unstable();
    ids.dedup();
    let mut o = Vec::with_capacity(4 + ids.len() * 8);
    pu32(&mut o, ids.len() as u32);
    for &id in ids.iter() {
        pu64(&mut o, id);
    }
    o
}

/// Zero-copy view over a posting-list blob (the bytes stay mmap-resident; reads are unaligned LE).
pub struct PostingList<'a> {
    body: &'a [u8],
    pub len: usize,
}
impl<'a> PostingList<'a> {
    pub fn new(blob: &'a [u8]) -> Self {
        let len = u32::from_le_bytes(blob[0..4].try_into().unwrap()) as usize;
        PostingList {
            body: &blob[4..4 + len * 8],
            len,
        }
    }
    #[inline]
    pub fn get(&self, i: usize) -> u64 {
        u64::from_le_bytes(self.body[i * 8..i * 8 + 8].try_into().unwrap())
    }
    /// Sorted-merge intersection of two ascending posting lists into `out`.
    pub fn intersect(a: &PostingList, b: &PostingList, out: &mut Vec<u64>) {
        out.clear();
        let (mut i, mut j) = (0usize, 0usize);
        while i < a.len && j < b.len {
            let (x, y) = (a.get(i), b.get(j));
            match x.cmp(&y) {
                std::cmp::Ordering::Less => i += 1,
                std::cmp::Ordering::Greater => j += 1,
                std::cmp::Ordering::Equal => {
                    out.push(x);
                    i += 1;
                    j += 1;
                }
            }
        }
    }
}

// ── asset + template forward blobs ───────────────────────────────────────────────────────────────
/// A decoded data attribute: schema field index + its canonical string value.
pub type Attr = (u8, String);

const ASSET_VERSION: u8 = 1;

/// Encode an asset forward record. `immutable` is non-empty only for assets without a template
/// (templated assets get their immutable data from the template blob, joined at read time).
pub fn encode_asset(
    owner: u64,
    collection: u64,
    schema: u64,
    template_id: i32,
    block_num: u32,
    immutable: &[Attr],
    mutable: &[Attr],
) -> Vec<u8> {
    let mut o = Vec::with_capacity(30 + (immutable.len() + mutable.len()) * 12);
    o.push(ASSET_VERSION);
    pu64(&mut o, owner);
    pu64(&mut o, collection);
    pu64(&mut o, schema);
    pi32(&mut o, template_id);
    pu32(&mut o, block_num);
    put_attrs(&mut o, immutable);
    put_attrs(&mut o, mutable);
    o
}

/// Encode a template forward record (its immutable attributes, stored once).
pub fn encode_template(template_id: i32, schema: u64, immutable: &[Attr]) -> Vec<u8> {
    let mut o = Vec::with_capacity(16 + immutable.len() * 12);
    o.push(ASSET_VERSION);
    pi32(&mut o, template_id);
    pu64(&mut o, schema);
    put_attrs(&mut o, immutable);
    o
}

fn put_attrs(o: &mut Vec<u8>, attrs: &[Attr]) {
    pu16(o, attrs.len() as u16);
    for (idx, val) in attrs {
        o.push(*idx);
        let vb = val.as_bytes();
        pu16(o, vb.len() as u16);
        o.extend_from_slice(vb);
    }
}

fn get_attrs(b: &[u8], p: &mut usize) -> Vec<Attr> {
    let n = gu16(b, p) as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let idx = b[*p];
        *p += 1;
        let len = gu16(b, p) as usize;
        let val = String::from_utf8_lossy(&b[*p..*p + len]).into_owned();
        *p += len;
        out.push((idx, val));
    }
    out
}

/// Decoded asset forward record.
#[derive(Debug, PartialEq)]
pub struct AssetRec {
    pub owner: u64,
    pub collection: u64,
    pub schema: u64,
    pub template_id: i32,
    pub block_num: u32,
    pub immutable: Vec<Attr>,
    pub mutable: Vec<Attr>,
}

pub fn decode_asset(b: &[u8]) -> AssetRec {
    let mut p = 1usize; // skip version
    let owner = gu64(b, &mut p);
    let collection = gu64(b, &mut p);
    let schema = gu64(b, &mut p);
    let template_id = gi32(b, &mut p);
    let block_num = gu32(b, &mut p);
    let immutable = get_attrs(b, &mut p);
    let mutable = get_attrs(b, &mut p);
    AssetRec {
        owner,
        collection,
        schema,
        template_id,
        block_num,
        immutable,
        mutable,
    }
}

/// Decoded template forward record: (template_id, schema, immutable attrs).
pub fn decode_template(b: &[u8]) -> (i32, u64, Vec<Attr>) {
    let mut p = 1usize;
    let template_id = gi32(b, &mut p);
    let schema = gu64(b, &mut p);
    let immutable = get_attrs(b, &mut p);
    (template_id, schema, immutable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_tag_round_trips_incl_arrays() {
        for t in ["uint64", "string", "double", "ipfs", "image", "bool"] {
            assert_eq!(tag_type(type_tag(t)), t);
        }
        assert_eq!(tag_type(type_tag("string[]")), "string[]");
        assert_eq!(tag_type(type_tag("uint16[]")), "uint16[]");
        // unknown → string
        assert_eq!(tag_type(type_tag("weirdtype")), "string");
    }

    #[test]
    fn schema_format_round_trips() {
        let fields = vec![
            ("name".to_string(), "string".to_string()),
            ("level".to_string(), "uint16".to_string()),
            ("traits".to_string(), "string[]".to_string()),
            ("img".to_string(), "ipfs".to_string()),
        ];
        assert_eq!(decode_schema_format(&encode_schema_format(&fields)), fields);
    }

    #[test]
    fn posting_list_sorts_dedups_and_reads_back() {
        let mut ids = vec![5u64, 1, 9, 5, 3, 1];
        let blob = encode_posting_list(&mut ids);
        assert_eq!(ids, vec![1, 3, 5, 9]);
        let pl = PostingList::new(&blob);
        assert_eq!(pl.len, 4);
        let got: Vec<u64> = (0..pl.len).map(|i| pl.get(i)).collect();
        assert_eq!(got, vec![1, 3, 5, 9]);
    }

    #[test]
    fn posting_list_intersection() {
        let mut a = vec![1u64, 3, 5, 7, 9];
        let mut b = vec![2u64, 3, 5, 8, 9, 10];
        let (ba, bb) = (encode_posting_list(&mut a), encode_posting_list(&mut b));
        let mut out = Vec::new();
        PostingList::intersect(&PostingList::new(&ba), &PostingList::new(&bb), &mut out);
        assert_eq!(out, vec![3, 5, 9]);
    }

    #[test]
    fn asset_record_round_trips() {
        let rec = AssetRec {
            owner: crate::name::encode("alice"),
            collection: crate::name::encode("mycollection"),
            schema: crate::name::encode("mysch"),
            template_id: 26,
            block_num: 409250749,
            immutable: vec![],
            mutable: vec![(0u8, "female".to_string()), (1u8, "83".to_string())],
        };
        let blob = encode_asset(
            rec.owner,
            rec.collection,
            rec.schema,
            rec.template_id,
            rec.block_num,
            &rec.immutable,
            &rec.mutable,
        );
        assert_eq!(decode_asset(&blob), rec);
    }

    #[test]
    fn template_record_round_trips() {
        let immutable = vec![(0u8, "Charizard".to_string()), (2u8, "150".to_string())];
        let blob = encode_template(3, crate::name::encode("pokemon"), &immutable);
        let (tid, schema, attrs) = decode_template(&blob);
        assert_eq!(tid, 3);
        assert_eq!(schema, crate::name::encode("pokemon"));
        assert_eq!(attrs, immutable);
    }
}
