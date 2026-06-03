//! MongoDB access helpers layered on [`Inner`]: per-chain database resolution, common
//! find/find_one/count wrappers over `Collection<Document>`, and the freshest-block read that backs
//! the `chain{}` metadata.

use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::{Database, IndexModel};

use crate::config::NetworkCfg;
use crate::error::ApiError;
use crate::state::{CachedMeta, Inner};

/// Candidate "head" collections to read the freshest `@block_num`/`@block_time` from. These dynamic
/// system collections carry both fields and a `{@block_num:-1}` index; the first that exists wins.
const HEAD_CANDIDATES: &[&str] = &["eosio-global", "eosio-rexpool", "eosio-rammarket"];

impl Inner {
    /// Look up a configured network or fail with 404.
    pub fn network(&self, chain: &str) -> Result<&NetworkCfg, ApiError> {
        self.net_index
            .get(chain)
            .ok_or_else(|| ApiError::UnknownChain(chain.to_string()))
    }

    /// `Database` handle for a configured chain (`<prefix>_<chain>`); 404 for unknown chains.
    pub fn db_for(&self, chain: &str) -> Result<Database, ApiError> {
        self.network(chain)?;
        Ok(self.client.database(&format!("{}_{}", self.prefix, chain)))
    }

    /// Best-effort create the indexes the query paths rely on (idempotent — a no-op if they already
    /// exist). Failures are logged, never fatal (e.g. a read-only replica). Critical at chain scale:
    /// without `accounts.{code,symbol,amount:-1}` `/topholders` is a full-collection sort, and
    /// without `pub_keys.{key}` `/key` is a full scan.
    pub async fn ensure_indexes(&self, chain: &str) {
        let Ok(db) = self.db_for(chain) else { return };
        let specs: &[(&str, Document)] = &[
            ("accounts", doc! { "scope": 1 }),
            ("accounts", doc! { "code": 1, "scope": 1, "symbol": 1 }),
            ("accounts", doc! { "code": 1, "symbol": 1, "amount": -1 }),
            ("permissions", doc! { "account": 1 }),
            ("pub_keys", doc! { "key": 1 }),
            ("pub_keys", doc! { "key_pub": 1 }),
            ("eosio-delband", doc! { "to": 1 }),
            ("eosio-delband", doc! { "from": 1 }),
            ("account_codehash", doc! { "code_hash": 1 }),
            ("voters", doc! { "block_num": -1 }),
            // topram sorts the whole table by ram_bytes; without this it scans+sorts 21.75M docs.
            ("eosio-userres", doc! { "ram_bytes": -1 }),
            // topstake sorts by the loader-emitted numeric `stake` (net+cpu base units).
            ("eosio-userres", doc! { "stake": -1 }),
            // rexbal/rexfund live in the `eosio` scope (not per-account), so accinfo/rex queries them
            // by `owner` — index it for rex-enabled chains (no-op where the collection is absent).
            ("eosio-rexbal", doc! { "owner": 1 }),
            ("eosio-rexfund", doc! { "owner": 1 }),
        ];
        let mut made = 0;
        for (coll, keys) in specs {
            let model = IndexModel::builder().keys(keys.clone()).build();
            match db.collection::<Document>(coll).create_index(model).await {
                Ok(_) => made += 1,
                Err(e) => tracing::debug!("ensure_index {chain}/{coll} skipped: {e}"),
            }
        }
        tracing::info!("ensure_indexes {chain}: {made}/{} ok", specs.len());
    }

    /// `find(filter)` with optional sort/projection/limit → all matching docs.
    pub async fn find(
        &self,
        db: &Database,
        coll: &str,
        filter: Document,
        sort: Option<Document>,
        projection: Option<Document>,
        limit: Option<i64>,
    ) -> Result<Vec<Document>, ApiError> {
        let collection = db.collection::<Document>(coll);
        let mut action = collection.find(filter);
        if let Some(s) = sort {
            action = action.sort(s);
        }
        if let Some(p) = projection {
            action = action.projection(p);
        }
        if let Some(l) = limit {
            action = action.limit(l);
        }
        let cursor = action.await?;
        Ok(cursor.try_collect().await?)
    }

    pub async fn find_one(
        &self,
        db: &Database,
        coll: &str,
        filter: Document,
    ) -> Result<Option<Document>, ApiError> {
        Ok(db.collection::<Document>(coll).find_one(filter).await?)
    }

    pub async fn count(
        &self,
        db: &Database,
        coll: &str,
        filter: Document,
    ) -> Result<u64, ApiError> {
        Ok(db
            .collection::<Document>(coll)
            .count_documents(filter)
            .await?)
    }

    /// Count the number of *distinct* values of `field` matching `filter`.
    ///
    /// Implemented as a `$group`+`$count` aggregation (with `allowDiskUse`) rather than the simpler
    /// `distinct` command: at WAX scale `permissions` has ~21.75M distinct accounts, and the raw
    /// `distinct` result array busts MongoDB's 16 MB BSON reply cap — the command fails outright and
    /// the count never populates. The aggregation streams through the index instead.
    pub async fn distinct_count(
        &self,
        db: &Database,
        coll: &str,
        field: &str,
        filter: Document,
    ) -> Result<u64, ApiError> {
        let mut pipeline = Vec::with_capacity(3);
        if !filter.is_empty() {
            pipeline.push(doc! { "$match": filter });
        }
        pipeline.push(doc! { "$group": { "_id": format!("${field}") } });
        pipeline.push(doc! { "$count": "n" });
        let cursor = db
            .collection::<Document>(coll)
            .aggregate(pipeline)
            .allow_disk_use(true)
            .await?;
        let rows: Vec<Document> = cursor.try_collect().await?;
        Ok(rows
            .first()
            .and_then(|d| {
                d.get_i32("n")
                    .ok()
                    .map(|v| v as u64)
                    .or_else(|| d.get_i64("n").ok().map(|v| v as u64))
            })
            .unwrap_or(0))
    }

    /// Read the freshest `(block_num, block_time)` for a chain, using the TTL cache. Tries the head
    /// candidate collections first (they have `@block_time`), then falls back to the newest `voters`
    /// doc for `block_num` with an empty time.
    pub async fn freshest(&self, chain: &str) -> Result<CachedMeta, ApiError> {
        if let Some(c) = self.meta_cache.read().await.get(chain) {
            if c.at.elapsed() < self.meta_ttl {
                return Ok(c.clone());
            }
        }
        let db = self.db_for(chain)?;
        let meta = self.read_freshest(&db).await?;
        let mut cache = self.meta_cache.write().await;
        cache.insert(chain.to_string(), meta.clone());
        Ok(meta)
    }

    async fn read_freshest(&self, db: &Database) -> Result<CachedMeta, ApiError> {
        for cand in HEAD_CANDIDATES {
            let doc = db
                .collection::<Document>(cand)
                .find_one(doc! {})
                .sort(doc! { "@block_num": -1 })
                .await?;
            if let Some(d) = doc {
                let block_num = bson_i64(d.get("@block_num")).unwrap_or(0);
                let block_time = d.get_str("@block_time").unwrap_or("").to_string();
                if block_num > 0 {
                    return Ok(CachedMeta {
                        block_num,
                        block_time,
                        at: std::time::Instant::now(),
                    });
                }
            }
        }
        // Fallback: newest voters doc carries block_num but no block_time.
        let v = db
            .collection::<Document>("voters")
            .find_one(doc! {})
            .sort(doc! { "block_num": -1 })
            .await?;
        let block_num = v
            .as_ref()
            .and_then(|d| bson_i64(d.get("block_num")))
            .unwrap_or(0);
        Ok(CachedMeta {
            block_num,
            block_time: String::new(),
            at: std::time::Instant::now(),
        })
    }
}

/// Best-effort `i64` from a BSON value that may be Int32/Int64/Double/numeric-String.
pub fn bson_i64(v: Option<&Bson>) -> Option<i64> {
    match v? {
        Bson::Int32(i) => Some(*i as i64),
        Bson::Int64(i) => Some(*i),
        Bson::Double(d) => Some(*d as i64),
        Bson::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Best-effort `f64` from a BSON value (numbers or numeric strings).
pub fn bson_f64(v: Option<&Bson>) -> Option<f64> {
    match v? {
        Bson::Double(d) => Some(*d),
        Bson::Int32(i) => Some(*i as f64),
        Bson::Int64(i) => Some(*i as f64),
        Bson::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Best-effort owned `String` from a BSON value (passes through strings, stringifies numbers).
pub fn bson_string(v: Option<&Bson>) -> Option<String> {
    match v? {
        Bson::String(s) => Some(s.clone()),
        Bson::Int32(i) => Some(i.to_string()),
        Bson::Int64(i) => Some(i.to_string()),
        Bson::Double(d) => Some(d.to_string()),
        _ => None,
    }
}
