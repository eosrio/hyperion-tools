//! wseg-build — the MongoDB frontend that builds a WormDB Light-API segment (`.wseg`).
//! Streams the per-chain Hyperion collections into the shared [`wseg_build::Builder`].
//!
//!   --probe <acct>  renders one account's accinfo fragment and exits (fast parity check).

use anyhow::{Context, Result};
use clap::Parser;
use futures::TryStreamExt;
use mongodb::bson::{doc, Document};
use mongodb::{Client, Database};
use wseg_build::Builder;

#[derive(Parser)]
#[command(about = "Build a WormDB Light-API segment (.wseg) from a Hyperion MongoDB")]
struct Args {
    #[arg(long, default_value = "mongodb://127.0.0.1:27017")]
    mongo_uri: String,
    #[arg(long, default_value = "hyperion_wax")]
    db: String,
    #[arg(long, default_value = "wax.wseg")]
    out: String,
    #[arg(long, default_value = "wax")]
    chain: String,
    /// Render one account's accinfo fragment to stdout and exit (parity check, no build).
    #[arg(long)]
    probe: Option<String>,
}

/// Redact any `user:pass@` userinfo from a Mongo URI before logging it.
fn redact_uri(uri: &str) -> String {
    if let Some(scheme_end) = uri.find("://") {
        let after = &uri[scheme_end + 3..];
        if let Some(at) = after.find('@') {
            return format!("{}://***@{}", &uri[..scheme_end], &after[at + 1..]);
        }
    }
    uri.to_string()
}

/// Stream every doc of `coll` into the builder.
async fn stream_into(db: &Database, coll: &'static str, b: &mut Builder) -> Result<u64> {
    let mut cur = db
        .collection::<Document>(coll)
        .find(doc! {})
        .batch_size(20_000)
        .await
        .with_context(|| format!("find {coll}"))?;
    let mut n = 0u64;
    while let Some(d) = cur.try_next().await? {
        b.push(coll, &d);
        n += 1;
    }
    Ok(n)
}

const COLLS: [&str; 5] = [
    "accounts",
    "permissions",
    "eosio-userres",
    "eosio-delband",
    "account_codehash",
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    eprintln!(
        "[wseg-build] chain={} connecting {} db={}",
        args.chain,
        redact_uri(&args.mongo_uri),
        args.db
    );
    let client = Client::with_uri_str(&args.mongo_uri)
        .await
        .context("connect mongo")?;
    let db = client.database(&args.db);

    // --probe: push just this account's accinfo source docs into a fresh builder, then render.
    if let Some(acct) = args.probe.as_deref() {
        let mut b = Builder::new();
        let queries: [(&str, Document); 3] = [
            ("eosio-userres", doc! { "@scope": acct }),
            ("permissions", doc! { "account": acct }),
            ("account_codehash", doc! { "account": acct }),
        ];
        for (coll, q) in queries {
            let mut cur = db.collection::<Document>(coll).find(q).await?;
            while let Some(d) = cur.try_next().await? {
                b.push(coll, &d);
            }
        }
        // delband: both directions in one query so a self-delegation isn't pushed twice.
        let mut cur = db
            .collection::<Document>("eosio-delband")
            .find(doc! { "$or": [ { "from": acct }, { "to": acct } ] })
            .await?;
        while let Some(d) = cur.try_next().await? {
            b.push("eosio-delband", &d);
        }
        println!("{}", b.render_account(acct));
        return Ok(());
    }

    let t0 = std::time::Instant::now();
    let mut b = Builder::new();
    for coll in COLLS {
        let n = stream_into(&db, coll, &mut b).await?;
        eprintln!(
            "[wseg-build] {coll}: {n} docs ({:.0}s)",
            t0.elapsed().as_secs_f64()
        );
    }
    eprintln!("[wseg-build] writing {} ...", args.out);
    let (holders, accounts) = b.finish(&args.out).context("write segment")?;
    let sz = std::fs::metadata(&args.out)?.len();
    eprintln!(
        "[wseg-build] wrote {} ({holders} holders, {accounts} accounts, {} MiB) in {:.0}s",
        args.out,
        sz >> 20,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
