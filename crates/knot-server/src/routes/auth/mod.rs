//! Routes mounted under `/auth/*`. CSRF is NOT enforced here — these
//! endpoints establish the session in the first place.

use axum::Router;

use crate::AppState;

pub mod config;
pub mod local;
pub mod oidc;
pub mod setup;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(setup::router())
        .merge(config::router())
        .merge(local::router())
        .merge(oidc::router())
}
