//! Health checks for load balancers and orchestrators, on a listener of their own.

use std::sync::Arc;

use axum::{Router, extract::State, routing::get};
use http::StatusCode;

use crate::App;

/// The health checks' service.
pub(crate) fn router(app: &Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .with_state(app.clone())
}

// r[impl health.live]
async fn healthz() -> &'static str {
    "ok\n"
}

// r[impl health.ready]
async fn readyz(State(app): State<Arc<App>>) -> (StatusCode, &'static str) {
    if app.ready() {
        (StatusCode::OK, "ready\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready\n")
    }
}
