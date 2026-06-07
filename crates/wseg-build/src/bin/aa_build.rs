//! aa-build — build the AtomicAssets faceted `.wseg` segment from a Hyperion-state MongoDB.
//!
//! Streams `atomicassets-schemas` → `-templates` → `-assets` (that order; schemas must precede the
//! rows that reference them) into [`AtomicBuilder`], then writes the segment. Reuses the same Mongo
//! state `snapshot-load --tables atomic` produced, so the POC needs no re-decode.

use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use futures::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use mongodb::{Client, Database};
use wseg_build::aa_builder::AtomicBuilder;

#[derive(Parser)]
#[command(about = "Build the AtomicAssets faceted .wseg from a Hyperion-state MongoDB")]
struct Args {
    #[arg(long, default_value = "mongodb://127.0.0.1:27017")]
    mongo_uri: String,
    #[arg(long, default_value = "aatest_waxtest")]
    db: String,
    #[arg(long, default_value = "aa-testnet.wseg")]
    out: String,
    /// Comma-separated data-attribute facet fields to inverted-index.
    #[arg(long, default_value = "rarity")]
    data_fields: String,
    /// Cap the number of assets streamed (for a fast validation pass; 0 = all).
    #[arg(long, default_value_t = 0)]
    limit: u64,
}

async fn stream(
    db: &Database,
    coll: &str,
    projection: Document,
    limit: u64,
    b: &mut AtomicBuilder,
    t0: Instant,
) -> Result<u64> {
    let opts = FindOptions::builder()
        .projection(projection)
        .batch_size(50_000)
        .build();
    let mut cur = db
        .collection::<Document>(coll)
        .find(doc! {})
        .with_options(opts)
        .await
        .with_context(|| format!("find {coll}"))?;
    let mut n = 0u64;
    while let Some(d) = cur.try_next().await? {
        b.push(coll, &d);
        n += 1;
        if limit > 0 && n >= limit {
            break;
        }
        if n.is_multiple_of(10_000_000) {
            eprintln!(
                "[aa-build]   {coll}: {n} ({:.0}s)",
                t0.elapsed().as_secs_f64()
            );
        }
    }
    eprintln!(
        "[aa-build] {coll}: {n} docs ({:.0}s)",
        t0.elapsed().as_secs_f64()
    );
    Ok(n)
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let fields: Vec<String> = args
        .data_fields
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    eprintln!(
        "[aa-build] db={} out={} data_fields={:?}",
        args.db, args.out, fields
    );
    let client = Client::with_uri_str(&args.mongo_uri)
        .await
        .context("connect mongo")?;
    let db = client.database(&args.db);

    let t0 = Instant::now();
    let mut b = AtomicBuilder::new(fields);
    // config first (singleton — contract/collection_format/supported_tokens; sets the collection_format
    // index), then collections (depend on that index for their `data` attrs), then schemas (need
    // `format`), then templates, then assets (projected).
    stream(&db, "atomicassets-config", doc! {}, 0, &mut b, t0).await?;
    stream(
        &db,
        "atomicassets-collections",
        doc! { "collection_name": 1, "author": 1, "allow_notify": 1, "authorized_accounts": 1,
        "notify_accounts": 1, "market_fee": 1, "data": 1 },
        0,
        &mut b,
        t0,
    )
    .await?;
    stream(&db, "atomicassets-schemas", doc! {}, 0, &mut b, t0).await?;
    stream(
        &db,
        "atomicassets-templates",
        doc! { "collection_name": 1, "schema_name": 1, "template_id": 1, "immutable_data": 1,
        "transferable": 1, "burnable": 1, "max_supply": 1, "issued_supply": 1 },
        0,
        &mut b,
        t0,
    )
    .await?;
    stream(
        &db,
        "atomicassets-assets",
        doc! { "asset_id": 1, "owner": 1, "collection_name": 1, "schema_name": 1,
        "template_id": 1, "block_num": 1, "immutable_data": 1, "mutable_data": 1 },
        args.limit,
        &mut b,
        t0,
    )
    .await?;

    eprintln!("[aa-build] writing {} ...", args.out);
    let stats = b.finish(&args.out).context("write segment")?;
    let sz = std::fs::metadata(&args.out)?.len();
    eprintln!(
        "[aa-build] wrote {} | {} assets, {} templates, {} schemas, {} collections | {} MiB ({} bytes) in {:.0}s",
        args.out,
        stats.assets,
        stats.templates,
        stats.schemas,
        stats.collections,
        sz >> 20,
        sz,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
