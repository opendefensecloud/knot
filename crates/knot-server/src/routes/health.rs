//! Health & readiness endpoints. Filled in by Plan 2 Task 7.

use axum::{Router, routing::get};

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/healthz", get(|| async { "ok" }))
}
