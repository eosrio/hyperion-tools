//! Fast repeated decoding with the Wire codec.
//!
//! `hyperion_wire_protocol::codec::decode_value` validates, re-encodes and indexes its whole ABI
//! on every call. For a large contract ABI that is over 100 µs per value, far more than the decode
//! itself. [`WireDecoder`] decodes each `(ABI, type)` pair against an ABI pruned to the types
//! reachable from that type, which the codec validates in a few microseconds.
//!
//! Strictness never changes: the first successful decode of every pair also runs against the full
//! ABI, and the pruned ABI is used only when both decodes succeed with identical values. A pair
//! whose decodes disagree stays on the full-ABI path, and a pair whose full-ABI decode fails is not
//! cached, so an ABI the codec rejects keeps failing exactly as before.

use std::collections::{HashMap, HashSet};

use hyperion_wire_protocol::codec::{CodecError, CodecProfile, decode_value};
use serde_json::{Map, Value};

enum Entry {
    Pruned(Value),
    Full,
}

/// Per-thread cache of pruned ABIs, keyed by a caller-chosen stable ABI key and type name.
#[derive(Default)]
pub struct WireDecoder {
    entries: HashMap<((u64, u64), String), Entry>,
}

impl WireDecoder {
    /// Decode `bytes` as `type_name` under `abi`. `key` must identify `abi` for the whole life of
    /// the decoder (e.g. account and ABI version); two different ABIs must never share a key.
    pub fn decode(
        &mut self,
        key: (u64, u64),
        abi: &Value,
        type_name: &str,
        bytes: &[u8],
        profile: &CodecProfile,
    ) -> Result<Value, CodecError> {
        if let Some(entry) = self.entries.get(&(key, type_name.to_owned())) {
            return match entry {
                Entry::Pruned(pruned) => decode_value(pruned, type_name, bytes, profile),
                Entry::Full => decode_value(abi, type_name, bytes, profile),
            };
        }
        let full = decode_value(abi, type_name, bytes, profile)?;
        let entry = match prune(abi, type_name) {
            Some(pruned)
                if decode_value(&pruned, type_name, bytes, profile)
                    .ok()
                    .as_ref()
                    == Some(&full) =>
            {
                Entry::Pruned(pruned)
            }
            _ => Entry::Full,
        };
        self.entries.insert((key, type_name.to_owned()), entry);
        Ok(full)
    }
}

/// The element type under the codec's suffixes: `T$`, `T?`, `T[]` and fixed arrays `T[N]`.
fn base_type(mut kind: &str) -> &str {
    loop {
        if let Some(b) = kind
            .strip_suffix('$')
            .or_else(|| kind.strip_suffix('?'))
            .or_else(|| kind.strip_suffix("[]"))
        {
            kind = b;
        } else if kind.ends_with(']') {
            match kind.rfind('[') {
                Some(i) => kind = &kind[..i],
                None => return kind,
            }
        } else {
            return kind;
        }
    }
}

/// `abi` reduced to `version` plus the typedefs, structs, variants and enums reachable from `root`
/// (and the protobuf descriptors when a `protobuf::` type is reachable), in their original order.
/// `None` when the ABI is not shaped as expected; the caller then keeps the full ABI.
fn prune(abi: &Value, root: &str) -> Option<Value> {
    let obj = abi.as_object()?;
    let section = |name: &str| {
        obj.get(name)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    };
    let index = |name: &str, key: &str| -> HashMap<&str, &Value> {
        section(name)
            .iter()
            .filter_map(|r| Some((r.get(key)?.as_str()?, r)))
            .collect()
    };
    let (aliases, structs, variants) = (
        index("types", "new_type_name"),
        index("structs", "name"),
        index("variants", "name"),
    );

    let mut keep: HashSet<&str> = HashSet::new();
    let mut protobuf = false;
    let mut work = vec![root];
    while let Some(kind) = work.pop() {
        let kind = base_type(kind);
        if kind.starts_with("protobuf::") {
            protobuf = true;
            continue;
        }
        if !keep.insert(kind) {
            continue;
        }
        if let Some(alias) = aliases.get(kind) {
            work.push(alias.get("type")?.as_str()?);
        } else if let Some(s) = structs.get(kind) {
            if let Some(base) = s
                .get("base")
                .and_then(Value::as_str)
                .filter(|b| !b.is_empty())
            {
                work.push(base);
            }
            for field in s
                .get("fields")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                work.push(field.get("type")?.as_str()?);
            }
        } else if let Some(v) = variants.get(kind) {
            for t in v.get("types")?.as_array()? {
                work.push(t.as_str()?);
            }
        }
        // Enums have a builtin underlying type; anything else is a builtin or unknown, and an
        // unknown type fails the pruned decode just as it fails the full one.
    }

    let mut out = Map::new();
    out.insert("version".into(), obj.get("version")?.clone());
    for (name, key) in [
        ("types", "new_type_name"),
        ("structs", "name"),
        ("variants", "name"),
        ("enums", "name"),
    ] {
        let kept: Vec<Value> = section(name)
            .iter()
            .filter(|r| {
                r.get(key)
                    .and_then(Value::as_str)
                    .is_some_and(|n| keep.contains(n))
            })
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.insert(name.into(), Value::Array(kept));
        }
    }
    if protobuf {
        out.insert("protobuf_types".into(), obj.get("protobuf_types")?.clone());
    }
    Some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn abi() -> Value {
        json!({
            "version": "sysio::abi/1.2",
            "types": [{"new_type_name": "amount", "type": "uint64"}],
            "structs": [
                {"name": "base_row", "base": "", "fields": [{"name": "id", "type": "uint64"}]},
                {"name": "row", "base": "base_row", "fields": [{"name": "amounts", "type": "amount[]"}, {"name": "tag", "type": "choice?"}]},
                {"name": "unrelated", "base": "", "fields": [{"name": "x", "type": "string"}]}
            ],
            "variants": [{"name": "choice", "types": ["uint8", "string"]}],
            "tables": [{"name": "rows", "type": "row", "index_type": "i64", "key_names": [], "key_types": [], "table_id": 1, "secondary_indexes": []}]
        })
    }

    #[test]
    fn prunes_to_the_reachable_closure() {
        let p = prune(&abi(), "row").unwrap();
        let names: Vec<&str> = p["structs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["base_row", "row"]);
        assert_eq!(p["types"].as_array().unwrap().len(), 1);
        assert_eq!(p["variants"].as_array().unwrap().len(), 1);
        assert!(p.get("tables").is_none());
    }

    #[test]
    fn pruned_and_full_decodes_agree() {
        let profile = CodecProfile::default();
        // id=7, amounts=[1,2], tag=Some(variant 1 "hi")
        let mut bytes = 7u64.to_le_bytes().to_vec();
        bytes.push(2);
        bytes.extend(1u64.to_le_bytes());
        bytes.extend(2u64.to_le_bytes());
        bytes.extend([1, 1, 2, b'h', b'i']);
        let full = decode_value(&abi(), "row", &bytes, &profile).unwrap();
        let mut dec = WireDecoder::default();
        assert_eq!(
            dec.decode((1, 1), &abi(), "row", &bytes, &profile).unwrap(),
            full
        );
        assert!(matches!(
            dec.entries.get(&((1, 1), "row".to_owned())),
            Some(Entry::Pruned(_))
        ));
        assert_eq!(
            dec.decode((1, 1), &abi(), "row", &bytes, &profile).unwrap(),
            full
        );
        // Trailing bytes fail on both paths.
        bytes.push(0);
        assert!(dec.decode((1, 1), &abi(), "row", &bytes, &profile).is_err());
    }

    #[test]
    fn rejected_abis_are_never_cached() {
        let profile = CodecProfile::default();
        let mut bad = abi();
        bad["structs"][2]["fields"][0]["type"] = json!("no_such_type");
        let mut dec = WireDecoder::default();
        assert!(dec.decode((2, 1), &bad, "row", &[0; 8], &profile).is_err());
        assert!(dec.entries.is_empty());
    }
}
