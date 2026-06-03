//! TOML configuration for the Light API server.
//!
//! One file declares the Mongo connection, the HTTP bind, and the set of networks served. Each
//! `[[networks]]` entry supplies the static `chain{}` metadata the cc32d9 API returns (chainid,
//! systoken, decimals, …) — the live `block_num`/`block_time`/`sync` are read from Mongo at request
//! time. A network named `eos` maps to the Mongo database `<prefix>_eos`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub mongo: MongoCfg,
    #[serde(default)]
    pub server: ServerCfg,
    #[serde(default)]
    pub networks: Vec<NetworkCfg>,
}

#[derive(Debug, Deserialize)]
pub struct MongoCfg {
    /// `mongodb://[user:pass@]host:port[/...]`.
    pub uri: String,
    /// Database name prefix; DB for chain `eos` is `<prefix>_eos`. Matches snapshot-load's default.
    #[serde(default = "default_prefix")]
    pub prefix: String,
    /// Optional auth database, applied to the typed credential (never string-appended to the URI).
    pub auth_source: Option<String>,
    #[serde(default = "default_pool")]
    pub max_pool_size: u32,
    /// Best-effort create the indexes the query paths rely on (topholders sort, pub_keys lookup,
    /// freshest-block read, …) at startup. Idempotent; harmless no-op if they already exist. Set
    /// `false` against a read-only replica.
    #[serde(default = "default_true")]
    pub ensure_indexes: bool,
}

#[derive(Debug, Deserialize)]
pub struct ServerCfg {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// tokio worker threads; `None` → runtime default (number of CPUs).
    pub threads: Option<usize>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            port: default_port(),
            threads: None,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct NetworkCfg {
    pub name: String,
    pub chainid: String,
    pub systoken: String,
    pub decimals: u32,
    #[serde(default = "default_production")]
    pub production: u32,
    #[serde(default)]
    pub rex_enabled: bool,
    #[serde(default)]
    pub description: String,
    /// Above this delay (head block age in seconds) the chain reports OUT_OF_SYNC.
    #[serde(default = "default_sync_threshold")]
    pub sync_threshold_secs: u64,
}

fn default_prefix() -> String {
    "hyperion".into()
}
fn default_pool() -> u32 {
    64
}
fn default_true() -> bool {
    true
}
fn default_bind() -> String {
    "0.0.0.0".into()
}
fn default_port() -> u16 {
    7000
}
fn default_production() -> u32 {
    1
}
fn default_sync_threshold() -> u64 {
    30
}

impl Config {
    /// Read + parse + validate the TOML config at `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Config> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.networks.is_empty() {
            bail!("config declares no [[networks]] — at least one is required");
        }
        let mut seen = std::collections::HashSet::new();
        for n in &self.networks {
            if !seen.insert(n.name.as_str()) {
                bail!("duplicate network name in config: {}", n.name);
            }
        }
        Ok(())
    }

    /// Build a name → meta lookup (clones; networks are few and small).
    pub fn network_index(&self) -> HashMap<String, NetworkCfg> {
        self.networks
            .iter()
            .map(|n| (n.name.clone(), n.clone()))
            .collect()
    }
}
