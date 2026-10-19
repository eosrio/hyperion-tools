//! ABI intervals belong to the caller's canonical view. Never consult a head ABI.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use super::ProtocolProvider;

struct Deployment {
    id: String,
    chain: String,
    contract: String,
    from: (u32, u32),
    until: Option<(u32, u32)>,
    provider: ProtocolProvider,
    hex: String,
    abi: Value,
}

pub struct DeploymentRegistry(Vec<Deployment>);

fn position(value: &Value) -> Result<(u32, u32)> {
    let number = |key: &str| {
        value[key]
            .as_f64()
            .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 0.0 && *n <= f64::from(u32::MAX))
            .map(|n| n as u32)
            .with_context(|| format!("invalid_abi: invalid position {key}"))
    };
    Ok((number("blockNum")?, number("ordinal")?))
}

impl DeploymentRegistry {
    pub fn new(inputs: &Value) -> Result<Self> {
        let items = inputs
            .as_array()
            .context("invalid_abi: expected deployment array")?;
        ensure!(
            items.len() <= 100_000,
            "limits_exceeded: too many deployments"
        );
        let mut result = Vec::new();
        let mut ids = std::collections::HashSet::new();
        for item in items {
            let text = |key: &str| {
                item[key]
                    .as_str()
                    .with_context(|| format!("invalid_abi: missing {key}"))
            };
            let id = text("id")?;
            let chain = text("chainId")?;
            let contract = text("contract")?;
            ensure!(
                !id.is_empty() && ids.insert(id.to_owned()),
                "invalid_abi: empty or duplicate deployment identity"
            );
            ensure!(
                chain.len() == 64
                    && chain
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid_abi: invalid chain scope"
            );
            ensure!(
                contract
                    .parse::<u64>()
                    .ok()
                    .is_some_and(|n| n.to_string() == contract),
                "invalid_abi: invalid raw contract identity"
            );
            let provider: ProtocolProvider = text("profileId")?.parse()?;
            ensure!(
                matches!(provider, ProtocolProvider::Wire(_)),
                "invalid_abi: Wire deployment requires a Wire profile"
            );
            let raw = text("abiHex")?;
            ensure!(
                raw.len() % 2 == 0
                    && raw
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "invalid_abi: invalid ABI hex"
            );
            let bytes = hex::decode(raw).context("invalid_abi: invalid ABI hex")?;
            let abi = provider.decode_abi(&bytes)?;
            let from = position(&item["from"])?;
            let until = item.get("until").map(position).transpose()?;
            ensure!(
                until.is_none_or(|end| from < end),
                "invalid_abi: empty deployment interval"
            );
            result.push(Deployment {
                id: id.into(),
                chain: chain.into(),
                contract: contract.into(),
                from,
                until,
                provider,
                hex: hex::encode(bytes),
                abi,
            });
        }
        Ok(Self(result))
    }

    pub fn decode_kv_record(&self, record: &Value, source: &Value) -> Result<Value> {
        let mut result = hyperion_wire_protocol::normalize_kv_record(record, source)?;
        let at = position(source)?;
        let candidates: Vec<_> = self
            .0
            .iter()
            .filter(|d| {
                d.chain == source["chainId"].as_str().unwrap_or("")
                    && d.contract == record["code"].as_str().unwrap_or("")
                    && d.from <= at
                    && d.until.is_none_or(|end| at < end)
            })
            .collect();
        ensure!(
            candidates.len() == 1,
            "invalid_abi: missing or ambiguous ABI deployment"
        );
        let selected = candidates[0];
        ensure!(
            source["abiDeployment"] == selected.id,
            "invalid_abi: source deployment does not match its position"
        );
        let mut matches = Vec::new();
        for table in selected.abi["tables"]
            .as_array()
            .context("invalid_abi: missing tables")?
        {
            if record["kind"] == "contract_row_kv"
                && table["table_id"].as_f64() == record["tableId"].as_f64()
            {
                matches.push((table, None));
            }
            if record["kind"] == "contract_index_kv" {
                for index in table["secondary_indexes"]
                    .as_array()
                    .context("invalid_abi: missing secondary metadata")?
                {
                    if index["table_id"].as_f64() == record["tableId"].as_f64() {
                        matches.push((table, Some(index)));
                    }
                }
            }
        }
        ensure!(
            matches.len() == 1,
            "invalid_abi: missing or ambiguous numeric table mapping"
        );
        let (table, index) = matches[0];
        result["profileId"] = json!(selected.provider.id());
        result["abiHex"] = json!(selected.hex);
        result["tableName"] = table["name"].clone();
        result["primaryTableId"] = table["table_id"].clone();
        result["valueType"] = table["type"].clone();
        if let Some(index) = index {
            result["secondaryName"] = index["name"].clone();
            result["secondaryKeyType"] = index["key_type"].clone();
        }
        if record["kind"] == "contract_row_kv" && record["present"] == true {
            result["value"] = selected.provider.decode_value(
                &selected.abi,
                table["type"]
                    .as_str()
                    .context("invalid_abi: missing value type")?,
                &hex::decode(
                    record["valueHex"]
                        .as_str()
                        .context("invalid_record: missing value bytes")?,
                )?,
            )?;
        }
        Ok(result)
    }
}
