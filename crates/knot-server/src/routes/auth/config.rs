//! GET /auth/config — public, pre-login probe.
//!
//! Tells the login page which options to show: whether first-run setup is
//! still available (no users yet), whether OIDC/SSO is configured, and
//! whether password login is enabled. Unauthenticated by design — it only
//! exposes low-sensitivity booleans, and the real gates stay on
//! `POST /auth/setup` and `GET /auth/oidc/login`.

use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct AuthConfig {
    pub setup_available: bool,
    pub oidc_enabled: bool,
    pub password_login_enabled: bool,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/auth/config", get(config))
}

async fn config(State(state): State<AppState>) -> impl IntoResponse {
    let setup_available = match state.users.clone() {
        Some(users) => match users.count().await {
            Ok(n) => n == 0,
            Err(e) => {
                tracing::error!(error=?e, "auth config: user count failed");
                false
            }
        },
        None => false,
    };

    Json(AuthConfig {
        setup_available,
        oidc_enabled: state.oidc_enabled,
        password_login_enabled: true,
    })
}
