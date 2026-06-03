//! Response helpers: JSON (compact or `?pretty=1`) and plain text.
//!
//! `serde_json::Value` objects here are backed by `BTreeMap` (default serde_json, no `preserve_order`
//! feature), so keys are already sorted — matching cc32d9's "formatted, sorted JSON" for `?pretty=1`.

use axum::extract::Query;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

/// `?pretty=1` query flag (any of `1`/`true`/`yes` enables it).
#[derive(Debug, Default, Deserialize)]
pub struct Pretty {
    pretty: Option<String>,
}

impl Pretty {
    pub fn on(&self) -> bool {
        matches!(self.pretty.as_deref(), Some("1" | "true" | "yes"))
    }
}

/// Extractor alias so handlers can take `pretty: Query<Pretty>`.
pub type PrettyQ = Query<Pretty>;

/// Render a JSON value, honoring the `?pretty=1` flag.
pub fn json(value: serde_json::Value, pretty: bool) -> Response {
    let body = if pretty {
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
    } else {
        value.to_string()
    };
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Render a plain-text body (`/tokenbalance`, `/holdercount`, `/usercount`, `/sync`, `/status`).
/// cc32d9 (Perl/Starman) terminates plain-text bodies with CRLF — match it for byte parity.
pub fn text(body: impl Into<String>) -> Response {
    let mut s = body.into();
    s.push_str("\r\n");
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], s).into_response()
}
