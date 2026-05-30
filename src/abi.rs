//! abi_def decoding and the abi-index NDJSON doc builder.

use anyhow::{anyhow, Context, Result};
use rs_abieos::Abieos;
use serde_json::Value;

/// Decode a serialized abi_def (hex) to its JSON + action/table name lists.
pub fn decode_abi_def(
    abieos: &Abieos,
    abi_hex: &str,
) -> Result<(String, Vec<String>, Vec<String>)> {
    let abi_bin = hex::decode(abi_hex).context("abi hex decode")?;
    let abi_json = abieos
        .abi_bin_to_json(&abi_bin)
        .map_err(|e| anyhow!("abi_bin_to_json: {e:?}"))?;
    let v: Value = serde_json::from_str(&abi_json)?;
    let names = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok((abi_json, names("actions"), names("tables")))
}

/// Build the abi-index NDJSON doc for a setabi. On success it carries
/// {abi, actions, tables}; on a decode failure (a malformed on-chain ABI) it
/// preserves the raw `abi_hex` and tags the doc with `abi_decode_error` so
/// downstream ingestion can flag it instead of inferring from an empty `abi`.
pub fn build_abi_doc(abieos: &Abieos, account: &str, block: u32, abi_hex: &str) -> String {
    match decode_abi_def(abieos, abi_hex) {
        Ok((abi, actions, tables)) => serde_json::json!({
            "account": account, "block": block, "abi": abi,
            "abi_hex": abi_hex, "actions": actions, "tables": tables,
        })
        .to_string(),
        Err(e) => serde_json::json!({
            "account": account, "block": block, "abi": "",
            "abi_hex": abi_hex, "actions": [], "tables": [],
            "abi_decode_error": e.to_string(),
        })
        .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_abi_is_tagged() {
        // "00" = abi_def with an empty version string -> unsupported -> decode error.
        let doc = build_abi_doc(&Abieos::new(), "badcontract", 100, "00");
        let v: serde_json::Value = serde_json::from_str(&doc).unwrap();
        assert_eq!(v["account"], "badcontract");
        assert_eq!(v["abi_hex"], "00");
        assert_eq!(v["abi"], "");
        assert!(v.get("abi_decode_error").is_some());
    }
}
