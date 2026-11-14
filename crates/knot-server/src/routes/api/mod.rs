//! `/api/*` routes. RequireSession + Scope + Csrf layered here.

use axum::{Router, middleware};

use crate::AppState;
use crate::auth::{csrf_mw, require_session_mw};

pub mod api_keys;
pub mod audit;
pub mod blobs;
pub mod boards;
pub mod comments;
pub mod contributors;
pub mod docs;
pub mod export;
pub mod export_import;
pub mod favorites;
pub mod grants;
pub mod history;
pub mod markdown;
pub mod notifications;
pub mod search;
pub mod shares;
pub mod short_links;
pub mod suggestions;
pub mod tasks;
pub mod whats_new;
pub mod workspace;

pub fn router(state: AppState) -> Router<AppState> {
    // Inside the session and CSRF layers (a 401 or a CSRF failure costs no
    // token), outside the doc-role layer and the handlers (a refused request
    // costs no ACL lookup).
    let rate_limit = middleware::from_fn_with_state(state.clone(), crate::rate_limit::http::api_mw);
    Router::new()
        .merge(workspace::router())
        .merge(api_keys::router())
        .merge(docs::router(state))
        .merge(blobs::router())
        .merge(search::router())
        .merge(shares::router())
        .merge(short_links::router())
        .merge(boards::router())
        .merge(tasks::router())
        .merge(notifications::router())
        .merge(favorites::router())
        .merge(export_import::router())
        .merge(whats_new::router())
        // Last layer is outermost: require_session -> scope -> csrf ->
        // rate limit -> handler.
        .layer(rate_limit)
        .layer(middleware::from_fn(csrf_mw))
        .layer(middleware::from_fn(crate::auth::scope_mw))
        .layer(middleware::from_fn(require_session_mw))
}
