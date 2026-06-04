//! Decode AtomicAssets `serialized_data` (the on-chain `eosio::atomicdata` format) into named
//! attributes, driven by a schema `format`.
//!
//! AtomicAssets stores collection/schema/template/asset data as `serialized_data` byte blobs that are
//! **not** standard Antelope/ABI — they use a custom, schema-format-driven binary encoding
//! (`eosio::atomicdata`). A generic ABI decode of an `atomicassets` table row hands you those fields
//! as opaque `bytes` (hex); this crate turns them into the named attribute key/values the AtomicAssets
//! API serves.
//!
//! Ported from the canonical contract (`pinknetworkx/atomicassets-contract` `include/atomicdata.hpp`),
//! cross-validated byte-for-byte against `atomicassets-js` (the lib the API uses) and the independent
//! XPRNetwork AssemblyScript reimplementation. Verified against a real 84-byte on-chain golden vector
//! (see the tests).
//!
//! ## Wire format (decode)
//! The blob is a flat sequence of `[varuint identifier][value]` pairs, read until the slice is
//! exhausted — stored blobs carry **no terminator**. `identifier = format_index + RESERVED(4)`, so id
//! `4` is `format[0]`. Only *present* attributes are written (sparse); a missing attribute is simply
//! absent (no null marker, no presence bit). Each value is decoded by the type at `format[id-4]`.
//!
//! ## Per-type encoding
//! - `int8/16/32/64` — zigzag, then unsigned LEB128 varint.
//! - `uint8/16/32/64` — unsigned LEB128 varint.
//! - `fixed8/16/32/64` — fixed-width little-endian (1/2/4/8 bytes), unsigned. *Not* varint.
//! - `byte` — exactly 1 raw byte. `bool` — 1 byte, normalized to 0/1.
//! - `float` / `double` — 4 / 8 bytes IEEE-754 little-endian.
//! - `string` / `image` — `[varuint len][UTF-8 bytes]` (identical; `image` is **not** base58).
//! - `ipfs` — `[varuint len][raw multihash bytes]`, output as a base58btc string (`Qm…`).
//! - `bytes` — `[varuint len][raw bytes]` → hex (atomicassets-js-only; never in genuine on-chain data).
//! - `<base>[]` — `[varuint count][elem…]`, each element by its base type (recursive).
//!
//! ## API parity
//! `uint64`/`int64`/`fixed64` are emitted as decimal **strings** (like the AtomicAssets API, which does
//! this to dodge JS number precision); all narrower ints and `bool` as JSON numbers; `float`/`double`
//! as numbers; `string`/`image`/`ipfs` as strings; arrays as JSON arrays. Output preserves blob order
//! (ascending schema index for the present attributes).

use anyhow::{anyhow, bail, Result};
use serde_json::{Number, Value};

/// AtomicAssets `RESERVED` identifier offset: on-blob id = 0-based schema-format index + 4.
const RESERVED: u64 = 4;

/// One field of a schema `format` (the on-chain `{name, type}`). `type` order is significant —
/// identifiers are positional (`index + RESERVED`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub r#type: String,
}

impl Field {
    pub fn new(name: impl Into<String>, ty: impl Into<String>) -> Self {
        Field {
            name: name.into(),
            r#type: ty.into(),
        }
    }

    /// Parse a schema's `format` — a JSON array of `{"name":…, "type":…}` as snapshot-load decodes it
    /// from the `schemas` table — into the ordered field list this decoder needs.
    pub fn from_format_json(v: &Value) -> Result<Vec<Field>> {
        let arr = v
            .as_array()
            .ok_or_else(|| anyhow!("atomicdata: schema format is not a JSON array"))?;
        arr.iter()
            .map(|e| {
                let name = e
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("atomicdata: format entry missing string `name`"))?;
                let ty = e
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("atomicdata: format entry missing string `type`"))?;
                Ok(Field::new(name, ty))
            })
            .collect()
    }
}

/// Decode a `serialized_data` blob into its present attributes (API-parity JSON values), in blob order.
///
/// `format` must be the schema's field list in its original on-chain order, since identifiers are
/// positional. Returns an error on a truncated blob, an identifier below `RESERVED`, an out-of-range
/// identifier, or an unsupported type.
pub fn deserialize(data: &[u8], format: &[Field]) -> Result<Vec<(String, Value)>> {
    let mut c = Cursor::new(data);
    let mut out = Vec::new();
    while c.remaining() > 0 {
        let id = c.read_varuint()?;
        let idx = id
            .checked_sub(RESERVED)
            .ok_or_else(|| anyhow!("atomicdata: identifier {id} < RESERVED({RESERVED})"))?
            as usize;
        let field = format.get(idx).ok_or_else(|| {
            anyhow!(
                "atomicdata: identifier {id} -> format index {idx} out of range ({} fields)",
                format.len()
            )
        })?;
        let v = deserialize_attribute(&field.r#type, &mut c)?;
        out.push((field.name.clone(), v));
    }
    Ok(out)
}

/// Convenience: decode straight into a `serde_json::Map`. NOTE: key order depends on whether
/// `serde_json`'s `preserve_order` feature is enabled in the final binary; use [`deserialize`] when
/// blob order must be preserved regardless.
pub fn deserialize_to_object(
    data: &[u8],
    format: &[Field],
) -> Result<serde_json::Map<String, Value>> {
    Ok(deserialize(data, format)?.into_iter().collect())
}

fn deserialize_attribute(ty: &str, c: &mut Cursor) -> Result<Value> {
    // Array: "<base>[]" — varuint count, then each element by the base type (recursive).
    if let Some(base) = ty.strip_suffix("[]") {
        let n = c.read_varuint()? as usize;
        // Cap the pre-allocation by the remaining bytes so a corrupt count can't OOM us.
        let mut arr = Vec::with_capacity(n.min(c.remaining() + 1));
        for _ in 0..n {
            arr.push(deserialize_attribute(base, c)?);
        }
        return Ok(Value::Array(arr));
    }

    let v = match ty {
        // signed: zigzag then LEB128. 64-bit -> JSON string (API parity).
        "int8" => Value::from(zigzag_decode(c.read_varuint()?) as i8),
        "int16" => Value::from(zigzag_decode(c.read_varuint()?) as i16),
        "int32" => Value::from(zigzag_decode(c.read_varuint()?) as i32),
        "int64" => Value::String(zigzag_decode(c.read_varuint()?).to_string()),
        // unsigned: LEB128. 64-bit -> JSON string.
        "uint8" => Value::from(c.read_varuint()? as u8),
        "uint16" => Value::from(c.read_varuint()? as u16),
        "uint32" => Value::from(c.read_varuint()? as u32),
        "uint64" => Value::String(c.read_varuint()?.to_string()),
        // fixed-width little-endian, unsigned. 64-bit -> JSON string.
        "fixed8" => Value::from(c.take(1)?[0]),
        "fixed16" => Value::from(u16::from_le_bytes(c.take(2)?.try_into().unwrap())),
        "fixed32" => Value::from(u32::from_le_bytes(c.take(4)?.try_into().unwrap())),
        "fixed64" => Value::String(u64::from_le_bytes(c.take(8)?.try_into().unwrap()).to_string()),
        // single raw byte (contract `byte` type)
        "byte" => Value::from(c.read_u8()?),
        // 1 byte, normalized to 0/1 (atomicassets-js parity)
        "bool" => Value::from(u8::from(c.read_u8()? == 1)),
        // IEEE-754 little-endian
        "float" => finite_number(f32::from_le_bytes(c.take(4)?.try_into().unwrap()) as f64),
        "double" => finite_number(f64::from_le_bytes(c.take(8)?.try_into().unwrap())),
        // length-prefixed UTF-8 (image == string; NOT base58)
        "string" | "image" => {
            let n = c.read_varuint()? as usize;
            Value::String(String::from_utf8_lossy(c.take(n)?).into_owned())
        }
        // length-prefixed raw multihash -> base58btc string (CIDv0 "Qm…")
        "ipfs" => {
            let n = c.read_varuint()? as usize;
            Value::String(bs58::encode(c.take(n)?).into_string())
        }
        // length-prefixed blob (atomicassets-js-only; never in genuine on-chain data) -> hex
        "bytes" => {
            let n = c.read_varuint()? as usize;
            Value::String(hex_encode(c.take(n)?))
        }
        other => bail!("atomicdata: unsupported attribute type '{other}'"),
    };
    Ok(v)
}

/// ZigZag decode (signed `intN`): even -> n/2, odd -> -(n/2)-1. Bit-identical to the contract.
#[inline]
fn zigzag_decode(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

/// `serde_json::Number` can't hold NaN/inf; on-chain data is finite, but guard defensively (-> null).
fn finite_number(f: f64) -> Value {
    Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn hex_encode(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 0x0f) as usize] as char);
    }
    s
}

/// Byte cursor over a `serialized_data` blob.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }
    fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    fn read_u8(&mut self) -> Result<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| anyhow!("atomicdata: unexpected end of data"))?;
        self.pos += 1;
        Ok(b)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| anyhow!("atomicdata: length overflow"))?;
        let s = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| anyhow!("atomicdata: unexpected end of data (need {n} bytes)"))?;
        self.pos = end;
        Ok(s)
    }
    /// Unsigned LEB128 (low group first, 0x80 = continuation), matching the contract's varuint.
    fn read_varuint(&mut self) -> Result<u64> {
        let mut value: u64 = 0;
        let mut shift: u32 = 0;
        loop {
            if shift >= 64 {
                bail!("atomicdata: varuint exceeds 64 bits");
            }
            let b = self.read_u8()?;
            value |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Decode a hex string (spaces ignored) to bytes — test helper, no dep.
    fn hx(s: &str) -> Vec<u8> {
        let h: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
        assert!(h.len().is_multiple_of(2), "odd hex length");
        h.chunks(2)
            .map(|p| {
                let hi = (p[0] as char).to_digit(16).unwrap();
                let lo = (p[1] as char).to_digit(16).unwrap();
                (hi * 16 + lo) as u8
            })
            .collect()
    }

    fn fmt(pairs: &[(&str, &str)]) -> Vec<Field> {
        pairs.iter().map(|(n, t)| Field::new(*n, *t)).collect()
    }

    /// THE GOLDEN VECTOR — a real on-chain 84-byte blob exercising uint64, string, fixed16[], negative
    /// float, bool, ipfs (CIDv0), int16, and int16[] (extremes + negatives), with a sparse/absent
    /// int32 field. Sourced from atomicassets-js `test/serialization_contract.test.ts`; hand-verified
    /// byte-for-byte. This is the single most authoritative cross-check.
    #[test]
    fn golden_onchain_vector() {
        let format = fmt(&[
            ("id", "uint64"),
            ("name", "string"),
            ("test2", "fixed16[]"),
            ("test3", "float"),
            ("unused", "int32"),
            ("isTrueTrue", "bool"),
            ("image", "ipfs"),
            ("gibnumber", "int16"),
            ("onemore", "int16[]"),
        ]);
        let data = hx(
            "04 12 05 06 4d 75 6e 69 63 68 06 04 12 00 7b 00 21 00 90 03 07 00 00 40 bf 09 01 0a 22 \
             12 20 b7 41 a3 b1 cf 5b fe ae 20 8c 86 ef bf ac 8e 0b bc 0c 92 ee a7 ef 9a 2d 96 40 12 \
             e6 c2 21 0f 4c 0b f6 01 0c 08 fe ff 03 ff ff 03 10 18 10 be 3a a9 03 f2 c0 01",
        );
        assert_eq!(data.len(), 84, "golden blob must be 84 bytes");

        let got = deserialize(&data, &format).unwrap();
        let expected: Vec<(String, Value)> = vec![
            ("id".into(), json!("18")), // uint64 -> string
            ("name".into(), json!("Munich")),
            ("test2".into(), json!([18, 123, 33, 912])),
            ("test3".into(), json!(-0.75)),
            ("isTrueTrue".into(), json!(1)),
            (
                "image".into(),
                json!("Qmag1NRBcpYyz27Kq2demHavXoi7nwbcCfkUq5vh6nuNN7"),
            ),
            ("gibnumber".into(), json!(123)),
            (
                "onemore".into(),
                json!([32767, -32768, 8, 12, 8, 3743, -213, 12345]),
            ),
        ];
        assert_eq!(got, expected);
        // 'unused' (int32, id 8) is sparse/absent -> not in the output.
        assert!(!got.iter().any(|(k, _)| k == "unused"));
    }

    /// int16 vs uint16 vs fixed16 of the SAME value 534 -> three distinct wire encodings.
    #[test]
    fn three_encodings_of_534() {
        // ids 6,7,9 -> format indices 2,3,5. Pad the lower indices.
        let format = fmt(&[
            ("p0", "uint8"),
            ("p1", "uint8"),
            ("i", "int16"),
            ("u", "uint16"),
            ("p4", "uint8"),
            ("f", "fixed16"),
        ]);
        // int16 534 -> zigzag 1068 -> LEB128 ac 08 ; uint16 534 -> LEB128 96 04 ; fixed16 534 -> LE 16 02
        let data = hx("06 ac 08 07 96 04 09 16 02");
        let got = deserialize(&data, &format).unwrap();
        assert_eq!(
            got,
            vec![
                ("i".into(), json!(534)),
                ("u".into(), json!(534)),
                ("f".into(), json!(534)),
            ]
        );
    }

    /// float 0.75 and double 1024.25 — IEEE-754 little-endian.
    #[test]
    fn float_and_double() {
        let format = fmt(&[("wear", "float"), ("share", "double")]);
        let data = hx("04 00 00 40 3f 05 00 00 00 00 00 01 90 40");
        let got = deserialize(&data, &format).unwrap();
        assert_eq!(
            got,
            vec![
                ("wear".into(), json!(0.75)),
                ("share".into(), json!(1024.25))
            ]
        );
    }

    #[test]
    fn negative_int_and_64bit_strings() {
        let format = fmt(&[("a", "int32"), ("b", "uint64"), ("c", "int64")]);
        // a=int32 -5 -> zigzag 9 -> 09 ; b=uint64 18446744073709551615 (u64::MAX) -> LEB128 ; c=int64 -1 -> zigzag 1 -> 01
        // u64::MAX LEB128 = ff ff ff ff ff ff ff ff ff 01
        let data = hx("04 09 05 ff ff ff ff ff ff ff ff ff 01 06 01");
        let got = deserialize(&data, &format).unwrap();
        assert_eq!(
            got,
            vec![
                ("a".into(), json!(-5)),
                ("b".into(), json!("18446744073709551615")), // uint64 -> string
                ("c".into(), json!("-1")),                   // int64 -> string
            ]
        );
    }

    #[test]
    fn string_value() {
        let format = fmt(&[("name", "string")]);
        let data = hx("04 09 4d 34 41 34 20 53 6b 69 6e"); // "M4A4 Skin"
        let got = deserialize(&data, &format).unwrap();
        assert_eq!(got, vec![("name".into(), json!("M4A4 Skin"))]);
    }

    #[test]
    fn empty_blob_is_empty() {
        let format = fmt(&[("id", "uint64")]);
        assert_eq!(deserialize(&[], &format).unwrap(), vec![]);
    }

    #[test]
    fn empty_string_and_empty_ipfs() {
        let format = fmt(&[("s", "string"), ("i", "ipfs")]);
        let data = hx("04 00 05 00");
        let got = deserialize(&data, &format).unwrap();
        assert_eq!(got, vec![("s".into(), json!("")), ("i".into(), json!(""))]);
    }

    #[test]
    fn image_is_a_plain_string_not_base58() {
        // The dangerous landmine: a field NAMED image with TYPE image is a plain UTF-8 string.
        let format = fmt(&[("image", "image")]);
        let data = hx("04 05 68 65 6c 6c 6f"); // "hello"
        assert_eq!(
            deserialize(&data, &format).unwrap(),
            vec![("image".into(), json!("hello"))]
        );
    }

    #[test]
    fn errors_on_bad_identifier_and_truncation() {
        let format = fmt(&[("id", "uint64")]);
        // identifier 4 (id) but no value bytes -> truncation error
        assert!(deserialize(&hx("04"), &format).is_err());
        // identifier 99 -> out of range
        assert!(deserialize(&hx("63 00"), &format).is_err());
        // identifier 1 (< RESERVED) -> error
        assert!(deserialize(&hx("01 00"), &format).is_err());
    }

    #[test]
    fn from_format_json_parses() {
        let v = json!([{"name":"rarity","type":"string"},{"name":"power","type":"uint32"}]);
        let fields = Field::from_format_json(&v).unwrap();
        assert_eq!(
            fields,
            vec![
                Field::new("rarity", "string"),
                Field::new("power", "uint32")
            ]
        );
    }
}
