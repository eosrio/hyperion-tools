//! wseg-build — build a frozen WormDB Light-API segment (.wseg) from the
//! per-chain Hyperion MongoDB.
//!
//! Tables:
//!   balances (0)  — holder -> packed "<contract>\t<symbol>\t<decimals>\t<amount>\n…"
//!   accinfo  (5)  — account -> cc32d9 accinfo fragment (resources…linkauth[,code]})
//!
//! `--probe <acct>` renders one account's accinfo fragment and exits (fast parity check).

mod accinfo;
mod name;
mod wseg;

use anyhow::{Context, Result};
use clap::Parser;
use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::{Client, Database};
use wseg::{IndexEntry, Table};

const TABLE_BALANCES: u32 = 0;

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
    /// Comma-separated tables to build.
    #[arg(long, default_value = "balances,accinfo")]
    tables: String,
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

fn bson_i64(b: Option<&Bson>) -> Option<i64> {
    match b? {
        Bson::Int64(v) => Some(*v),
        Bson::Int32(v) => Some(*v as i64),
        Bson::Double(d) => Some(*d as i64),
        _ => None,
    }
}

/// Build the balances table: stream `accounts` grouped by holder, pack each holder's rows.
async fn build_balances(db: &Database) -> Result<Table> {
    let coll = db.collection::<Document>("accounts");
    let t0 = std::time::Instant::now();
    let mut cursor = coll
        .find(doc! {})
        .sort(doc! { "scope": 1 }) // index-ordered stream → groups rows by holder
        .projection(doc! { "_id": 0, "scope": 1, "code": 1, "symbol": 1, "decimals": 1, "amount_str": 1, "amount": 1 })
        .batch_size(20_000)
        .await
        .context("find accounts")?;

    let mut arena: Vec<u8> = Vec::with_capacity(1 << 30);
    let mut index: Vec<IndexEntry> = Vec::with_capacity(18_000_000);
    let mut cur_scope: Option<String> = None;
    let mut cur_blob: Vec<u8> = Vec::with_capacity(256);
    let mut rows: u64 = 0;

    let flush = |arena: &mut Vec<u8>,
                 index: &mut Vec<IndexEntry>,
                 scope: &str,
                 blob: &[u8]|
     -> Result<()> {
        if blob.is_empty() {
            return Ok(());
        }
        let off = arena.len();
        anyhow::ensure!(blob.len() <= u32::MAX as usize, "holder blob too large");
        arena.extend_from_slice(blob);
        index.push(IndexEntry {
            key: name::encode(scope),
            off: off as u64,
            len: blob.len() as u32,
        });
        Ok(())
    };

    while let Some(d) = cursor.try_next().await.context("cursor")? {
        let scope = match d.get_str("scope") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let code = d.get_str("code").unwrap_or("");
        let symbol = d.get_str("symbol").unwrap_or("");
        let decimals = bson_i64(d.get("decimals")).unwrap_or(0);
        let amount = if let Ok(s) = d.get_str("amount_str") {
            s.to_string()
        } else if let Ok(f) = d.get_f64("amount") {
            format!("{:.*}", decimals.max(0) as usize, f)
        } else {
            "0".to_string()
        };

        if cur_scope.as_deref() != Some(scope) {
            if let Some(prev) = cur_scope.take() {
                flush(&mut arena, &mut index, &prev, &cur_blob)?;
            }
            cur_scope = Some(scope.to_string());
            cur_blob.clear();
        }
        cur_blob.extend_from_slice(code.as_bytes());
        cur_blob.push(b'\t');
        cur_blob.extend_from_slice(symbol.as_bytes());
        cur_blob.push(b'\t');
        cur_blob.extend_from_slice(decimals.to_string().as_bytes());
        cur_blob.push(b'\t');
        cur_blob.extend_from_slice(amount.as_bytes());
        cur_blob.push(b'\n');
        rows += 1;
        if rows % 4_000_000 == 0 {
            eprintln!(
                "[balances] {} rows, {} holders, {} MiB, {:.0}s",
                rows,
                index.len(),
                arena.len() >> 20,
                t0.elapsed().as_secs_f64()
            );
        }
    }
    if let Some(prev) = cur_scope.take() {
        flush(&mut arena, &mut index, &prev, &cur_blob)?;
    }
    eprintln!(
        "[balances] {} rows -> {} holders ({} MiB) in {:.0}s",
        rows,
        index.len(),
        arena.len() >> 20,
        t0.elapsed().as_secs_f64()
    );
    Ok(Table {
        table_id: TABLE_BALANCES,
        index,
        arena,
    })
}

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

    if let Some(acct) = args.probe.as_deref() {
        return accinfo::probe_accinfo(&db, acct).await;
    }

    let t0 = std::time::Instant::now();
    let want: Vec<&str> = args.tables.split(',').map(|s| s.trim()).collect();
    let mut tables: Vec<Table> = Vec::new();
    if want.contains(&"balances") {
        tables.push(build_balances(&db).await?);
    }
    if want.contains(&"accinfo") {
        tables.push(accinfo::build_accinfo(&db).await?);
    }
    anyhow::ensure!(
        !tables.is_empty(),
        "no tables selected (use --tables balances,accinfo)"
    );

    let summary: Vec<String> = tables
        .iter()
        .map(|t| format!("tbl{}={} keys", t.table_id, t.index.len()))
        .collect();
    eprintln!("[wseg-build] writing {} [{}]", args.out, summary.join(", "));
    wseg::write_segment(&args.out, tables).context("write segment")?;
    let sz = std::fs::metadata(&args.out)?.len();
    eprintln!(
        "[wseg-build] wrote {} ({} MiB) in {:.0}s total",
        args.out,
        sz >> 20,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
