//! Router: maps each cc32d9 path to its handler. All endpoints are GET and live under `/api`.

use axum::routing::get;
use axum::Router;

use crate::handlers;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/networks", get(handlers::networks))
        .route("/api/status", get(handlers::status))
        .route("/api/sync/:chain", get(handlers::sync))
        .route("/api/account/:chain/:account", get(handlers::account))
        .route("/api/accinfo/:chain/:account", get(handlers::accinfo))
        .route("/api/balances/:chain/:account", get(handlers::balances))
        .route("/api/rexbalance/:chain/:account", get(handlers::rexbalance))
        .route("/api/rexraw/:chain/:account", get(handlers::rexraw))
        .route("/api/key/:key", get(handlers::key))
        .route("/api/codehash/:hash", get(handlers::codehash))
        .route(
            "/api/tokenbalance/:chain/:account/:contract/:symbol",
            get(handlers::tokenbalance),
        )
        .route(
            "/api/topholders/:chain/:contract/:symbol/:count",
            get(handlers::topholders),
        )
        .route(
            "/api/holdercount/:chain/:contract/:symbol",
            get(handlers::holdercount),
        )
        .route("/api/usercount/:chain", get(handlers::usercount))
        .route("/api/topram/:chain/:count", get(handlers::topram))
        .route("/api/topstake/:chain/:count", get(handlers::topstake))
        .with_state(state)
}
