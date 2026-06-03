//! Assembly of the cc32d9 `chain{}` metadata block: static `[[networks]]` config + the live
//! freshest `block_num`/`block_time`/`sync` read from Mongo.

use serde_json::{json, Value};

use crate::error::ApiError;
use crate::state::AppState;
use crate::timeutil;

/// Resolved chain metadata for one request.
pub struct ChainMeta {
    pub block_num: i64,
    pub block_time: String,
    /// Head-block age in seconds (0 if unknown).
    pub sync: i64,
    /// Whether the chain is within its configured `sync_threshold_secs`.
    pub in_sync: bool,
}

impl AppState {
    /// Read + resolve chain metadata (uses the TTL block cache).
    pub async fn chain_meta(&self, chain: &str) -> Result<ChainMeta, ApiError> {
        let net = self.network(chain)?;
        let fresh = self.freshest(chain).await?;
        let age = timeutil::age_secs(&fresh.block_time);
        let in_sync = age.is_some_and(|a| a as u64 <= net.sync_threshold_secs);
        Ok(ChainMeta {
            block_num: fresh.block_num,
            block_time: fresh.block_time,
            sync: age.unwrap_or(0),
            in_sync,
        })
    }

    /// Build the `chain{}` JSON object. `with_rex` adds the `rex_enabled` flag (account endpoints).
    pub async fn chain_block(&self, chain: &str, with_rex: bool) -> Result<Value, ApiError> {
        let net = self.network(chain)?.clone();
        let m = self.chain_meta(chain).await?;
        let mut obj = json!({
            "network": net.name,
            "sync": m.sync,
            "decimals": net.decimals,
            "systoken": net.systoken,
            "chainid": net.chainid,
            "production": net.production,
            "block_num": m.block_num,
            "block_time": m.block_time,
            "description": net.description,
        });
        if with_rex {
            obj["rex_enabled"] = json!(if net.rex_enabled { 1 } else { 0 });
        }
        Ok(obj)
    }
}
