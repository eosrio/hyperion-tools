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

// ── hybrid posting list (the on-disk win) ─────────────────────────────────────────────────────────
// A posting is stored RAW (sorted u64s) when small, or ROARING + a small raw "head" when large:
//   [u8 format][u32 full_count]
//     format 0 RAW     : [u64 × full_count]                            (sorted asc; tail = largest)
//     format 1 ROARING : [u32 head_n][u64 × head_n (top-K desc)][roaring bytes for the full set]
// Light keys stay zero-copy for page tails; heavy keys compress 3.7–56× and intersect via bitmap-AND,
// while their `head` keeps page-1 (the common case) a zero-copy slice.
use roaring::RoaringTreemap;

/// Postings with at most this many ids are stored raw (zero-copy, tiny, cheap to merge).
pub const RAW_MAX: usize = 512;
/// Heavy postings keep this many top (largest = newest) ids raw for instant page-1.
pub const HEAD_K: usize = 256;

/// Encode a posting list in the hybrid format (sorts + dedups `ids` in place).
pub fn encode_posting_hybrid(ids: &mut Vec<u64>) -> Vec<u8> {
    ids.sort_unstable();
    ids.dedup();
    if ids.len() <= RAW_MAX {
        let mut o = Vec::with_capacity(5 + ids.len() * 8);
        o.push(0u8);
        pu32(&mut o, ids.len() as u32);
        for &id in ids.iter() {
            pu64(&mut o, id);
        }
        o
    } else {
        let rt: RoaringTreemap = ids.iter().copied().collect();
        let mut rbytes = Vec::new();
        rt.serialize_into(&mut rbytes).unwrap();
        let head_n = HEAD_K.min(ids.len());
        let mut o = Vec::with_capacity(9 + head_n * 8 + rbytes.len());
        o.push(1u8);
        pu32(&mut o, ids.len() as u32);
        pu32(&mut o, head_n as u32);
        for &id in ids.iter().rev().take(head_n) {
            pu64(&mut o, id); // top-K largest, descending
        }
        o.extend_from_slice(&rbytes);
        o
    }
}

/// Reader over a hybrid posting blob.
pub enum Posting<'a> {
    Raw(PostingList<'a>),
    Roaring {
        full: usize,
        head: &'a [u8],
        body: &'a [u8],
    },
}
impl<'a> Posting<'a> {
    pub fn parse(blob: &'a [u8]) -> Posting<'a> {
        let full = u32::from_le_bytes(blob[1..5].try_into().unwrap()) as usize;
        match blob[0] {
            0 => Posting::Raw(PostingList {
                body: &blob[5..5 + full * 8],
                len: full,
            }),
            _ => {
                let hn = u32::from_le_bytes(blob[5..9].try_into().unwrap()) as usize;
                Posting::Roaring {
                    full,
                    head: &blob[9..9 + hn * 8],
                    body: &blob[9 + hn * 8..],
                }
            }
        }
    }
    /// Total number of ids in the posting (O(1)).
    pub fn len(&self) -> usize {
        match self {
            Posting::Raw(pl) => pl.len,
            Posting::Roaring { full, .. } => *full,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Call `f` for each of the top-`n` (largest = newest) ids — the page-1 read, zero-copy in both
    /// formats (raw tail / roaring head).
    pub fn head_for_each(&self, n: usize, mut f: impl FnMut(u64)) {
        match self {
            Posting::Raw(pl) => {
                for i in pl.len.saturating_sub(n)..pl.len {
                    f(pl.get(i));
                }
            }
            Posting::Roaring { head, .. } => {
                let cnt = head.len() / 8;
                for i in 0..n.min(cnt) {
                    f(u64::from_le_bytes(
                        head[i * 8..i * 8 + 8].try_into().unwrap(),
                    ));
                }
            }
        }
    }
    /// XOR of the top-`n` ids (the page-1 read as a benchmark sink).
    pub fn head_xor(&self, n: usize) -> u64 {
        let mut h = 0u64;
        self.head_for_each(n, |id| h ^= id);
        h
    }
    /// Materialize a roaring treemap (deserialize for ROARING; build for RAW) — for multi-filter AND.
    pub fn to_roaring(&self) -> RoaringTreemap {
        match self {
            Posting::Raw(pl) => (0..pl.len).map(|i| pl.get(i)).collect(),
            Posting::Roaring { body, .. } => RoaringTreemap::deserialize_from(*body).unwrap(),
        }
    }
}

// ── asset + template forward blobs ───────────────────────────────────────────────────────────────
/// A decoded data attribute: schema field index + its canonical string value.
pub type Attr = (u8, String);

const ASSET_VERSION: u8 = 1;

/// Encode an asset forward record. `immutable` is non-empty only for assets without a template
/// (templated assets get their immutable data from the template blob, joined at read time).
#[allow(clippy::too_many_arguments)]
pub fn encode_asset(
    owner: u64,
    collection: u64,
    schema: u64,
    template_id: i32,
    block_num: u32,
    template_mint: u32,
    immutable: &[Attr],
    mutable: &[Attr],
) -> Vec<u8> {
    let mut o = Vec::with_capacity(34 + (immutable.len() + mutable.len()) * 12);
    o.push(ASSET_VERSION);
    pu64(&mut o, owner);
    pu64(&mut o, collection);
    pu64(&mut o, schema);
    pi32(&mut o, template_id);
    pu32(&mut o, block_num);
    pu32(&mut o, template_mint); // materialized mint ordinal (rank within the template), reconstructable
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
    pub template_mint: u32,
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
    let template_mint = gu32(b, &mut p);
    let immutable = get_attrs(b, &mut p);
    let mutable = get_attrs(b, &mut p);
    AssetRec {
        owner,
        collection,
        schema,
        template_id,
        block_num,
        template_mint,
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

// ── config singleton (TABLE_AA_CONFIG) ─────────────────────────────────────────────────────────────
/// Encode the AtomicAssets config singleton. `supported_tokens` = (token_contract name, symbol,
/// precision). Layout: `version | contract(u64) | u16 ver_len | ver | u16 fmt_len | <schema-format blob>
/// | u16 n_tokens | (u64 token_contract, u8 sym_len, sym, u8 precision) × n`.
pub fn encode_config(
    contract: u64,
    version: &str,
    collection_format: &[(String, String)],
    supported_tokens: &[(u64, String, i64)],
) -> Vec<u8> {
    let mut o = Vec::new();
    o.push(ASSET_VERSION);
    pu64(&mut o, contract);
    let vb = version.as_bytes();
    let vlen = vb.len().min(u16::MAX as usize);
    pu16(&mut o, vlen as u16);
    o.extend_from_slice(&vb[..vlen]);
    let fmt = encode_schema_format(collection_format);
    // Cap the length prefix AND the appended bytes together so the header can never desync the body.
    let flen = fmt.len().min(u16::MAX as usize);
    pu16(&mut o, flen as u16);
    o.extend_from_slice(&fmt[..flen]);
    let ntok = supported_tokens.len().min(u16::MAX as usize);
    pu16(&mut o, ntok as u16);
    for (tc, sym, prec) in supported_tokens.iter().take(ntok) {
        pu64(&mut o, *tc);
        let sb = sym.as_bytes();
        let sl = sb.len().min(255);
        o.push(sl as u8);
        o.extend_from_slice(&sb[..sl]);
        o.push(*prec as u8);
    }
    o
}

/// Decode the config singleton: `(contract, version, collection_format, supported_tokens)`.
/// Fully bounds-checked — a truncated/corrupt blob returns `None` instead of panicking.
pub fn decode_config(
    b: &[u8],
) -> Option<(u64, String, Vec<(String, String)>, Vec<(u64, String, u8)>)> {
    if b.is_empty() {
        return None;
    }
    let mut p = 1usize;
    if b.len() < p + 8 {
        return None;
    }
    let contract = gu64(b, &mut p);
    if b.len() < p + 2 {
        return None;
    }
    let vlen = gu16(b, &mut p) as usize;
    if b.len() < p + vlen + 2 {
        return None;
    }
    let version = String::from_utf8_lossy(&b[p..p + vlen]).into_owned();
    p += vlen;
    let fmt_len = gu16(b, &mut p) as usize;
    if b.len() < p + fmt_len + 2 {
        return None;
    }
    let collection_format = decode_schema_format(&b[p..p + fmt_len]);
    p += fmt_len;
    let n = gu16(b, &mut p) as usize;
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        if b.len() < p + 9 {
            return None;
        }
        let tc = gu64(b, &mut p);
        let sl = b[p] as usize;
        p += 1;
        if b.len() < p + sl + 1 {
            return None;
        }
        let sym = String::from_utf8_lossy(&b[p..p + sl]).into_owned();
        p += sl;
        let prec = b[p];
        p += 1;
        tokens.push((tc, sym, prec));
    }
    Some((contract, version, collection_format, tokens))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// CROSS-REPO GOLDEN: `encode_asset` for fixed inputs must produce these exact bytes. The SAME
    /// array is decoded + asserted on the READER side by "golden asset record decodes byte-for-byte"
    /// in wormdb-domain-atomicassets/src/binfmt.zig. A silent field reorder/resize on either side
    /// (without a version bump) fails one of the two tests — pinning the cross-repo byte contract.
    #[test]
    fn golden_asset_record() {
        let golden: &[u8] = &[
            1, // version
            1, 0, 0, 0, 0, 0, 0, 0, // owner = 1
            2, 0, 0, 0, 0, 0, 0, 0, // collection = 2
            3, 0, 0, 0, 0, 0, 0, 0, // schema = 3
            7, 0, 0, 0, // template_id = 7
            100, 0, 0, 0, // block_num = 100
            42, 0, 0, 0, // template_mint = 42
            0, 0, // immutable attr count
            0, 0, // mutable attr count
        ];
        assert_eq!(encode_asset(1, 2, 3, 7, 100, 42, &[], &[]), golden);
    }

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
    fn hybrid_posting_raw_then_roaring() {
        // small → raw (format 0), deduped + sorted
        let mut small = vec![5u64, 1, 9, 5, 3];
        let b = encode_posting_hybrid(&mut small);
        assert_eq!(b[0], 0);
        let p = Posting::parse(&b);
        assert_eq!(p.len(), 4);
        assert_eq!(p.head_xor(2), 9 ^ 5); // top 2 largest
        assert_eq!(p.to_roaring().len(), 4);

        // large → roaring (format 1) with a raw head
        let mut big: Vec<u64> = (0..2000u64).map(|i| 1_099_511_627_776 + i * 7).collect();
        let b2 = encode_posting_hybrid(&mut big);
        assert_eq!(b2[0], 1);
        assert!(b2.len() < 2000 * 8, "roaring should compress vs 16KB raw");
        let p2 = Posting::parse(&b2);
        assert_eq!(p2.len(), 2000);
        let rt = p2.to_roaring();
        assert_eq!(rt.len(), 2000);
        assert!(rt.contains(1_099_511_627_776));
        assert!(rt.contains(1_099_511_627_776 + 1999 * 7));
        // head = the top-K largest
        let mut top = 0u64;
        p2.head_for_each(1, |id| top = id);
        assert_eq!(top, 1_099_511_627_776 + 1999 * 7);
    }

    #[test]
    fn asset_record_round_trips() {
        let rec = AssetRec {
            owner: crate::name::encode("alice"),
            collection: crate::name::encode("mycollection"),
            schema: crate::name::encode("mysch"),
            template_id: 26,
            block_num: 409250749,
            template_mint: 1422,
            immutable: vec![],
            mutable: vec![(0u8, "female".to_string()), (1u8, "83".to_string())],
        };
        let blob = encode_asset(
            rec.owner,
            rec.collection,
            rec.schema,
            rec.template_id,
            rec.block_num,
            rec.template_mint,
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

    #[test]
    fn config_round_trips() {
        let fmt = vec![
            ("name".to_string(), "string".to_string()),
            ("img".to_string(), "ipfs".to_string()),
        ];
        let tokens = vec![(crate::name::encode("eosio.token"), "EOS".to_string(), 4i64)];
        let blob = encode_config(crate::name::encode("atomicassets"), "1.2.0", &fmt, &tokens);
        let (c, v, f, t) = decode_config(&blob).expect("decode config");
        assert_eq!(c, crate::name::encode("atomicassets"));
        assert_eq!(v, "1.2.0");
        assert_eq!(f, fmt);
        assert_eq!(
            t,
            vec![(crate::name::encode("eosio.token"), "EOS".to_string(), 4u8)]
        );

        // truncated/empty blobs return None instead of panicking (bounds-checked decoder).
        assert!(decode_config(&[]).is_none());
        assert!(decode_config(&blob[..blob.len() - 1]).is_none());
        assert!(decode_config(&blob[..3]).is_none());
    }
}
