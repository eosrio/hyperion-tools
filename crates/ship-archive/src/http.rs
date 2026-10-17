//! The optional HTTP endpoints (`--http <ADDR>`), served by one tiny_http thread beside the
//! SHiP listener:
//!
//! | endpoint   | response                                                                    |
//! |------------|-----------------------------------------------------------------------------|
//! | `/health`  | 200 and JSON: version, chain id, served ranges, last block, sessions, findings, and with `--api` the chain API's listener and request slots |
//! | `/ready`   | 200 while the server accepts sessions (the store is open and the listener bound), 503 once it shuts down |
//! | `/metrics` | 200 and the Prometheus text exposition ([`crate::metrics`])                 |
//!
//! Anything else is 404, and a method other than GET or HEAD 405. Query strings are ignored.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::{Context, Result, anyhow};
use hyperion_ship::catalog::Finding;
use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};
use tracing::{debug, warn};

use crate::api::GET_BLOCK;
use crate::fetch::MemoryBudget;
use crate::metrics::Metrics;
use crate::store::{Source, Store};

/// What the endpoints report.
pub(crate) struct State {
    pub store: Arc<Store>,
    pub findings: Arc<[Finding]>,
    pub metrics: Arc<Metrics>,
    pub budget: Arc<MemoryBudget>,
    /// The chain API, with `--api`.
    pub api: Option<ApiHealth>,
    /// Set while the server accepts sessions.
    pub ready: AtomicBool,
}

/// What `/health` says of the chain API besides its in-flight count.
pub(crate) struct ApiHealth {
    pub listen: SocketAddr,
    pub max_in_flight: usize,
}

/// The bound HTTP listener, not serving yet.
pub(crate) struct Http {
    server: Arc<Server>,
    addr: SocketAddr,
}

impl Http {
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        let server = Server::http(addr).map_err(|e| anyhow!("bind --http {addr}: {e}"))?;
        let addr = server
            .server_addr()
            .to_ip()
            .context("the HTTP listener has no IP address")?;
        Ok(Self {
            server: Arc::new(server),
            addr,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Serve on a thread of its own until the returned handle is stopped or dropped.
    pub fn spawn(self, state: Arc<State>) -> Result<Serving> {
        let server = Arc::clone(&self.server);
        let thread = std::thread::Builder::new()
            .name("http".into())
            .spawn(move || {
                // `recv` fails once `unblock` is called: that is the stop signal.
                while let Ok(request) = server.recv() {
                    respond(request, &state);
                }
            })
            .context("start the HTTP thread")?;
        Ok(Serving {
            server: self.server,
            thread: Some(thread),
        })
    }
}

/// The HTTP thread, stopped (after the request it is answering) when dropped.
pub(crate) struct Serving {
    server: Arc<Server>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.server.unblock();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            warn!("the HTTP thread panicked");
        }
    }
}

fn respond(request: Request, state: &State) {
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .to_owned();
    let (code, content_type, body) = match request.method() {
        Method::Get | Method::Head => route(&path, state),
        _ => (405, TEXT, "method not allowed\n".to_owned()),
    };
    let header = Header::from_bytes(&b"Content-Type"[..], content_type)
        .expect("static content types are valid header values");
    let response = Response::from_string(body)
        .with_status_code(code)
        .with_header(header);
    if let Err(e) = request.respond(response) {
        debug!("an HTTP response failed: {e}");
    }
}

const TEXT: &[u8] = b"text/plain; charset=utf-8";
const JSON: &[u8] = b"application/json";
const METRICS: &[u8] = b"text/plain; version=0.0.4; charset=utf-8";

fn route(path: &str, state: &State) -> (u16, &'static [u8], String) {
    match path {
        "/health" => (200, JSON, health(state).to_string()),
        "/ready" if state.ready.load(Ordering::Relaxed) => (200, TEXT, "ready\n".to_owned()),
        "/ready" => (503, TEXT, "shutting down\n".to_owned()),
        "/metrics" => (
            200,
            METRICS,
            state.metrics.render(&state.store, &state.budget),
        ),
        _ => (404, TEXT, "not found\n".to_owned()),
    }
}

/// `/health`: what is served, and what opening the logs found.
pub(crate) fn health(state: &State) -> Value {
    let store = &state.store;
    let range = |(begin, end): (u32, u32)| json!({ "begin": begin, "end": end });
    let mut served = serde_json::Map::new();
    for source in Source::ALL {
        let r = match source {
            Source::Ship(kind) => store.served.ship(kind),
            Source::Blocks => store.served.blocks,
        };
        served.insert(source.label().to_owned(), range(r));
    }
    let findings: Vec<Value> = state.findings.iter().map(finding_json).collect();
    let mut health = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "chain_id": hex::encode(store.chain_id),
        "served": served,
        "last": { "num": store.last.num, "id": hex::encode(store.last.id) },
        "sessions": state.metrics.sessions(),
        "findings": findings,
    });
    if let Some(api) = &state.api {
        health["api"] = json!({
            "listen": api.listen.to_string(),
            "get_block": GET_BLOCK,
            "in_flight": state.metrics.api_in_flight(),
            "max_in_flight": api.max_in_flight,
        });
    }
    health
}

/// A finding as JSON: its kind, whether it is a warning, and its message.
pub(crate) fn finding_json(f: &Finding) -> Value {
    let kind = match f {
        Finding::Empty { .. } => "empty",
        Finding::NameMismatch { .. } => "name_mismatch",
        Finding::Duplicate { .. } => "duplicate",
        Finding::LargeEntry { .. } => "large_entry",
    };
    json!({ "kind": kind, "warning": f.is_warning(), "message": f.to_string() })
}
