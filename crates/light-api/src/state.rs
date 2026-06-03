//! Shared application state: one pooled `mongodb::Client`, the configured networks, and a small
//! TTL cache of each chain's freshest block (so `/networks` and every `chain{}` block stay cheap).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use mongodb::options::ClientOptions;
use mongodb::Client;
use tokio::sync::{Mutex, RwLock};

use crate::config::{Config, NetworkCfg};

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub client: Client,
    pub prefix: String,
    /// Ordered, as declared in config — drives `/networks` output order.
    pub networks: Vec<NetworkCfg>,
    pub net_index: HashMap<String, NetworkCfg>,
    pub meta_cache: RwLock<HashMap<String, CachedMeta>>,
    pub meta_ttl: Duration,
    /// Cache of expensive aggregate counts (`/usercount`, `/holdercount`) — these are full-collection
    /// scans that would be O(seconds) per request at chain scale, so they are computed in the
    /// background and served from here (mirrors cc32d9's holder-count cron).
    pub counts: RwLock<HashMap<String, CountEntry>>,
    /// Keys with an in-flight background refresh — prevents a scan stampede under concurrent load.
    pub refreshing: Mutex<HashSet<String>>,
    pub count_ttl: Duration,
}

/// Cached freshest-block snapshot for one chain.
#[derive(Clone)]
pub struct CachedMeta {
    pub block_num: i64,
    pub block_time: String,
    pub at: Instant,
}

/// A cached aggregate count.
#[derive(Clone, Copy)]
pub struct CountEntry {
    pub value: u64,
    pub at: Instant,
}

impl std::ops::Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// Build the pooled client (mirrors snapshot-load's auth_source handling) and assemble state.
    pub async fn connect(cfg: &Config) -> Result<AppState> {
        let mut opts = ClientOptions::parse(&cfg.mongo.uri).await?;
        opts.max_pool_size = Some(cfg.mongo.max_pool_size);
        opts.min_pool_size = Some(4);
        opts.app_name = Some("light-api".to_string());
        // Apply auth_source only when the URI already carries a credential — see snapshot-load
        // mongo.rs: a username-less credential is rejected at connect time.
        if let Some(src) = &cfg.mongo.auth_source {
            if let Some(cred) = &mut opts.credential {
                cred.source = Some(src.clone());
            }
        }
        let client = Client::with_options(opts)?;
        Ok(AppState(Arc::new(Inner {
            client,
            prefix: cfg.mongo.prefix.clone(),
            networks: cfg.networks.clone(),
            net_index: cfg.network_index(),
            meta_cache: RwLock::new(HashMap::new()),
            meta_ttl: Duration::from_secs(3),
            counts: RwLock::new(HashMap::new()),
            refreshing: Mutex::new(HashSet::new()),
            count_ttl: Duration::from_secs(300),
        })))
    }

    /// Serve a cached aggregate count, refreshing it in the background when stale or absent. Never
    /// blocks the request on the underlying scan: returns the last-known value (or 0 if never
    /// computed) and spawns at most one refresh per key. Mirrors cc32d9's count cron.
    pub async fn cached_count<F, Fut>(&self, key: String, compute: F) -> u64
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Option<u64>> + Send + 'static,
    {
        let (value, stale) = {
            let c = self.counts.read().await;
            match c.get(&key) {
                Some(e) => (e.value, e.at.elapsed() > self.count_ttl),
                None => (0, true),
            }
        };
        if stale {
            // Single-flight: only spawn if no refresh is already in flight for this key.
            let mut inflight = self.refreshing.lock().await;
            if inflight.insert(key.clone()) {
                let st = self.clone();
                tokio::spawn(async move {
                    if let Some(v) = compute().await {
                        st.counts.write().await.insert(
                            key.clone(),
                            CountEntry {
                                value: v,
                                at: Instant::now(),
                            },
                        );
                    }
                    st.refreshing.lock().await.remove(&key);
                });
            }
        }
        value
    }
}
