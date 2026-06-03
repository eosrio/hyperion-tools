//! Light API server entry point.
//!
//! Reproduces the cc32d9 `eosio_light_api` HTTP API over the per-chain MongoDB written by
//! `snapshot-load` / maintained by a Hyperion deployment. tokio + axum, one pooled `mongodb::Client`,
//! configured via TOML (see `light-api.toml`).

mod asset;
mod config;
mod db;
mod error;
mod handlers;
mod keyfmt;
mod meta;
mod respond;
mod rex;
mod routes;
mod state;
mod timeutil;

use anyhow::{Context, Result};
use clap::Parser;

use crate::config::Config;
use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(
    name = "light-api",
    about = "Serve the cc32d9 eosio_light_api from Hyperion's MongoDB"
)]
struct Args {
    /// Path to the TOML config.
    #[arg(long, default_value = "light-api.toml")]
    config: String,
    /// Override the bind address from config.
    #[arg(long)]
    bind: Option<String>,
    /// Override the port from config.
    #[arg(long)]
    port: Option<u16>,
    /// Override the MongoDB URI from config.
    #[arg(long)]
    mongo_uri: Option<String>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut cfg = Config::load(&args.config)?;
    if let Some(b) = args.bind.clone() {
        cfg.server.bind = b;
    }
    if let Some(p) = args.port {
        cfg.server.port = p;
    }
    if let Some(u) = args.mongo_uri.clone() {
        cfg.mongo.uri = u;
    }

    // Build the runtime honoring the optional worker-thread override (mirrors archive-server).
    let mut rt = tokio::runtime::Builder::new_multi_thread();
    rt.enable_all();
    if let Some(n) = cfg.server.threads {
        rt.worker_threads(n.max(1));
    }
    let rt = rt.build()?;
    rt.block_on(run(cfg))
}

async fn run(cfg: Config) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "light_api=info,tower_http=warn".into()),
        )
        .init();

    let bind = cfg.server.bind.clone();
    let port = cfg.server.port;
    let chains: Vec<&str> = cfg.networks.iter().map(|n| n.name.as_str()).collect();
    tracing::info!(
        "light-api: {} chains [{}] -> mongo db prefix '{}'",
        chains.len(),
        chains.join(", "),
        cfg.mongo.prefix
    );

    let state = AppState::connect(&cfg)
        .await
        .context("connecting to MongoDB")?;

    // Ensure the query-path indexes exist (best-effort) before serving — at chain scale this is the
    // difference between an indexed lookup and a full-collection scan for /topholders, /key, etc.
    if cfg.mongo.ensure_indexes {
        for net in &cfg.networks {
            state.ensure_indexes(&net.name).await;
        }
    }

    // Start the expensive aggregate-count scans in the background so /usercount is never cold/blocking.
    handlers::warm_counts(&state).await;

    let app = routes::router(state).layer(tower_http::trace::TraceLayer::new_for_http());

    let addr = format!("{bind}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app)
        .await
        .context("axum server error")?;
    Ok(())
}
