//! Select protocol semantics before any schema-specific fast path.
//!
//! The Wire component is compiled from the coordinated package's exact internal
//! Rust source. This is an executable source-linking experiment, not a public
//! native ABI or a second operator-managed plugin.

use std::{fmt, str::FromStr};

use anyhow::{Context, Result, bail, ensure};
use hyperion_wire_protocol::codec::{self, CodecProfile};
use serde_json::{Value, json};
mod deployments;
pub use deployments::DeploymentRegistry;

pub const WIRE_SOURCE_REVISION: &str = "5e94b871691f4919f060dd0a6a603f2d40e2fa36";

/// Validated timestamp parameters shared by block rendering and selected codecs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimestampProfile {
    epoch_milliseconds: i64,
    slot_milliseconds: i64,
}

impl TimestampProfile {
    pub const ANTELOPE: Self = Self {
        epoch_milliseconds: 946_684_800_000,
        slot_milliseconds: 500,
    };

    fn wire(profile: &CodecProfile) -> Self {
        Self {
            epoch_milliseconds: profile.epoch_milliseconds,
            slot_milliseconds: profile.slot_milliseconds,
        }
    }

    pub fn unix_milliseconds(self, slot: u32) -> i64 {
        // Only fixed, validated profiles can construct this type. Their full u32
        // slot domain fits i64 and the node's four-digit Gregorian year domain.
        self.epoch_milliseconds + i64::from(slot) * self.slot_milliseconds
    }

    pub fn slot_to_iso(self, slot: u32) -> String {
        let ms = self.unix_milliseconds(slot);
        let secs = ms.div_euclid(1000);
        let days = secs.div_euclid(86_400);
        let tod = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let mut year = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = mp + if mp < 10 { 3 } else { -9 };
        year += i64::from(month <= 2);
        format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}",
            tod / 3600,
            tod / 60 % 60,
            tod % 60,
            ms.rem_euclid(1000)
        )
    }
}

/// Explicit protocol selection. The existing Antelope dialect remains default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ProtocolProvider {
    #[default]
    Antelope,
    Wire(CodecProfile),
}

impl FromStr for ProtocolProvider {
    type Err = anyhow::Error;

    fn from_str(id: &str) -> Result<Self> {
        if id == "antelope" {
            Ok(Self::Antelope)
        } else {
            Ok(Self::Wire(CodecProfile::for_id(id)?))
        }
    }
}

impl fmt::Display for ProtocolProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

impl ProtocolProvider {
    pub fn id(&self) -> &str {
        match self {
            Self::Antelope => "antelope",
            Self::Wire(profile) => &profile.id,
        }
    }

    /// Validate even programmatically constructed profiles before use.
    pub fn validate(&self) -> Result<()> {
        if let Self::Wire(profile) = self {
            ensure!(
                *profile == CodecProfile::for_id(&profile.id)?,
                "unsupported_feature: altered Wire protocol profile"
            );
        }
        Ok(())
    }

    pub fn timestamp_profile(&self) -> Result<TimestampProfile> {
        self.validate()?;
        Ok(match self {
            Self::Antelope => TimestampProfile::ANTELOPE,
            Self::Wire(profile) => TimestampProfile::wire(profile),
        })
    }

    /// A protocol-aware caller must refuse before opening outputs/checkpoints or
    /// entering an incompatible fixed-layout reader. No ABI override changes this.
    pub fn require_antelope(&self, path: &str) -> Result<()> {
        self.validate()?;
        ensure!(
            matches!(self, Self::Antelope),
            "unsupported_feature: {} does not support {} (use protocol replay for Wire frames)",
            path,
            self.id()
        );
        Ok(())
    }

    /// Bind deployment metadata to the selected chain codec before interpreting
    /// values. Antelope selection cannot activate Wire through an input record.
    pub fn decode_kv_record(
        &self,
        deployments: &Value,
        record: &Value,
        source: &Value,
    ) -> Result<Value> {
        self.validate()?;
        ensure!(
            matches!(self, Self::Wire(_)),
            "unsupported_feature: Wire deployments require a selected Wire provider"
        );
        for deployment in deployments
            .as_array()
            .context("invalid_abi: expected deployment array")?
        {
            ensure!(
                deployment["profileId"].as_str() == Some(self.id()),
                "invalid_abi: deployment profile does not match the selected chain provider"
            );
        }
        DeploymentRegistry::new(deployments)?.decode_kv_record(record, source)
    }

    pub fn decode_abi(&self, bytes: &[u8]) -> Result<Value> {
        self.validate()?;
        match self {
            Self::Wire(profile) => Ok(codec::decode_abi(bytes, profile)?),
            Self::Antelope => {
                let text = rs_abieos::Abieos::new()
                    .abi_bin_to_json(bytes)
                    .map_err(|e| anyhow::anyhow!("abi_bin_to_json: {e:?}"))?;
                Ok(serde_json::from_str(&text)?)
            }
        }
    }

    pub fn decode_value(&self, abi: &Value, ty: &str, bytes: &[u8]) -> Result<Value> {
        self.validate()?;
        match self {
            Self::Wire(profile) => Ok(codec::decode_value(abi, ty, bytes, profile)?),
            Self::Antelope => {
                let engine = rs_abieos::Abieos::new();
                engine
                    .set_abi_json("protocol", &abi.to_string())
                    .map_err(|e| anyhow::anyhow!("set_abi_json: {e:?}"))?;
                let text = engine
                    .bin_to_json("protocol", ty, bytes)
                    .map_err(|e| anyhow::anyhow!("bin_to_json: {e:?}"))?;
                Ok(serde_json::from_str(&text)?)
            }
        }
    }

    pub fn encode_value(&self, abi: &Value, ty: &str, value: &Value) -> Result<Vec<u8>> {
        self.validate()?;
        match self {
            Self::Wire(profile) => Ok(codec::encode_value(abi, ty, value, profile)?),
            Self::Antelope => {
                let engine = rs_abieos::Abieos::new();
                engine
                    .set_abi_json("protocol", &abi.to_string())
                    .map_err(|e| anyhow::anyhow!("set_abi_json: {e:?}"))?;
                engine
                    .json_to_bin("protocol", ty, &value.to_string())
                    .map_err(|e| anyhow::anyhow!("json_to_bin: {e:?}"))
            }
        }
    }

    /// Complete Wire transport records, retaining every nested byte payload.
    pub fn decode_bundle(&self, abi: &Value, bytes: &[u8]) -> Result<Value> {
        let Self::Wire(profile) = self else {
            bail!(
                "unsupported_feature: complete bundle replay requires an explicitly selected Wire profile"
            );
        };
        self.validate()?;
        let envelope = self.decode_value(abi, "result", bytes)?;
        let name = envelope[0]
            .as_str()
            .context("invalid result variant")?
            .to_owned();
        let mut bundle =
            json!({"profileId":self.id(), "rawFrameHex":hex::encode(bytes), "envelope":envelope});
        if !name.starts_with("get_blocks_result_") {
            return Ok(bundle);
        }
        let result = bundle["envelope"][1].clone();
        for (field, ty) in [("block", "signed_block"), ("traces", "transaction_trace[]")] {
            if let Some(raw) = result[field].as_str() {
                bundle[field] = self.decode_value(abi, ty, &hex::decode(raw)?)?;
            }
        }
        if let Some(raw) = result["deltas"].as_str() {
            let decoded = codec::ship::decode_deltas(abi, &hex::decode(raw)?, profile)?;
            let mut rows = Vec::new();
            for delta in decoded["deltas"].as_array().context("invalid deltas")? {
                ensure!(
                    delta[0] == "table_delta_v0",
                    "unsupported_feature: unknown table delta layout"
                );
                let stream = delta[1]["name"].as_str().context("missing stream")?;
                for row in delta[1]["rows"].as_array().context("missing rows")? {
                    let raw = row["data"].as_str().context("missing raw row")?;
                    rows.push(
                        json!({"stream":stream,"present":row["present"],"rawHex":raw,
                        "value":self.decode_value(abi, stream, &hex::decode(raw)?)?}),
                    );
                }
            }
            bundle["deltas"] = decoded["deltas"].clone();
            bundle["deltaFraming"] = decoded["framing"].clone();
            bundle["deltaRows"] = Value::Array(rows);
        }
        Ok(bundle)
    }

    pub fn normalize_kv_row(
        &self,
        abi: &Value,
        stream: &str,
        present: bool,
        bytes: &[u8],
        source: &Value,
    ) -> Result<Value> {
        ensure!(
            matches!(self, Self::Wire(_)),
            "unsupported_feature: Wire KV normalization requires a Wire provider"
        );
        ensure!(
            matches!(stream, "contract_row_kv" | "contract_index_kv"),
            "unsupported_feature: unknown Wire KV stream"
        );
        let decoded = self.decode_value(abi, stream, bytes)?;
        ensure!(
            decoded[0] == format!("{stream}_v0"),
            "unsupported_feature: unknown Wire KV row version"
        );
        let row = &decoded[1];
        let raw_name = |value: &Value| -> Result<String> {
            let bytes = self.encode_value(abi, "name", value)?;
            Ok(u64::from_le_bytes(
                bytes
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid raw name"))?,
            )
            .to_string())
        };
        let mut record = json!({"kind":stream,"present":present,"code":raw_name(&row["code"])? ,"payer":raw_name(&row["payer"])? ,"tableId":row["table_id"]});
        if stream == "contract_row_kv" {
            record["keyHex"] = row["key"].clone();
            record["valueHex"] = row["value"].clone();
        } else {
            record["secondaryKeyHex"] = row["sec_key"].clone();
            record["primaryKeyHex"] = row["pri_key"].clone();
        }
        let mut normalized = hyperion_wire_protocol::normalize_kv_record(&record, source)?;
        normalized["rawHex"] = json!(hex::encode(bytes));
        normalized["decoded"] = decoded;
        Ok(normalized)
    }
}
