//! `/api/*` routes. Csrf + RequireSession layered here.

use axum::{Router, middleware};

use crate::AppState;
use crate::auth::{csrf_mw, require_session_mw};

pub mod workspace;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(workspace::router())
        .layer(middleware::from_fn(csrf_mw))
        .layer(middleware::from_fn(require_session_mw))
}
