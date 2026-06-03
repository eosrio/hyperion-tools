//! HTTP error type. cc32d9 returns plain-text error bodies (e.g. `Invalid count: 3`), so all errors
//! here render as `text/plain` with an appropriate status. Internal/driver detail is never leaked.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    /// Chain not in the configured `[[networks]]` set → 404.
    UnknownChain(String),
    /// Bad path/query input (e.g. top-N out of range, malformed key) → 400, body verbatim.
    BadRequest(String),
    /// MongoDB / serialization failure → 500, generic body.
    Internal(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::UnknownChain(c) => write!(f, "unknown chain: {c}"),
            ApiError::BadRequest(m) => write!(f, "{m}"),
            ApiError::Internal(m) => write!(f, "internal error: {m}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    fn parts(&self) -> (StatusCode, String) {
        match self {
            ApiError::UnknownChain(c) => (StatusCode::NOT_FOUND, format!("unknown chain: {c}")),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            // Body is generic; the detail is logged, not returned.
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(detail) = &self {
            tracing::error!("internal error: {detail}");
        }
        let (status, body) = self.parts();
        (
            status,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            body,
        )
            .into_response()
    }
}

/// Convert any `mongodb::error::Error` into an `Internal` error.
impl From<mongodb::error::Error> for ApiError {
    fn from(e: mongodb::error::Error) -> Self {
        ApiError::Internal(e.to_string())
    }
}
