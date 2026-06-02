//! Routes mounted under `/auth/*`. CSRF is NOT enforced here — these
//! endpoints establish the session in the first place.

use axum::Router;

use crate::AppState;

pub mod setup;

pub fn router() -> Router<AppState> {
    Router::new().merge(setup::router())
}
