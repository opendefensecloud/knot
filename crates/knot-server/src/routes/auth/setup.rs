//! POST /auth/setup — first-run bootstrap.
//!
//! Creates the singleton workspace + first user (owner role) and
//! immediately logs the operator in by setting the `sid` cookie. Returns
//! 410 once any user exists.

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
};
use chrono::Utc;
use knot_auth::SessionToken;
use knot_storage::WorkspaceRole;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::http_error::json_err;

#[derive(Deserialize)]
pub struct SetupRequest {
    pub email: String,
    pub password: String,
    pub display_name: String,
    /// Optional workspace name; defaults to "Workspace".
    pub workspace_name: Option<String>,
}

#[derive(Serialize)]
pub struct SetupResponse {
    pub user_id: String,
    pub workspace_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/auth/setup", post(setup))
}

async fn setup(State(state): State<AppState>, Json(req): Json<SetupRequest>) -> Response {
    let Some(users) = state.users.clone() else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage",
            "storage unavailable",
        );
    };
    let Some(workspaces) = state.workspaces.clone() else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage",
            "storage unavailable",
        );
    };
    let Some(sessions) = state.sessions.clone() else {
        return json_err(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage",
            "storage unavailable",
        );
    };

    if req.password.len() < 8 {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "auth.weak_password",
            "password must be at least 8 characters",
        );
    }

    let count = match users.count().await {
        Ok(n) => n,
        Err(e) => {
            tracing::error!(error=?e, "setup count");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
        }
    };
    if count > 0 {
        return json_err(
            StatusCode::GONE,
            "auth.setup_closed",
            "setup already complete",
        );
    }

    let ws_name = req.workspace_name.as_deref().unwrap_or("Workspace");
    let ws = match workspaces.create("default", ws_name).await {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(error=?e, "create workspace");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
        }
    };

    let hash = match state.hasher.hash(&req.password) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, "hash");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
        }
    };
    let user = match users
        .create_local(&req.email, &req.display_name, &hash)
        .await
    {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error=?e, "create user");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
        }
    };
    if let Err(e) = workspaces
        .add_member(ws.id, user.id, WorkspaceRole::Owner)
        .await
    {
        tracing::error!(error=?e, "add member");
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
    }

    // Mint session. The cookie carries the raw token; the store keeps only the
    // keyed HMAC (see session_loader), so create must hash exactly like login.
    let token = SessionToken::generate();
    let exp = Utc::now() + chrono::Duration::from_std(crate::auth::cookies::SESSION_TTL).unwrap();
    let sid_hash = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());
    if let Err(e) = sessions
        .create(&sid_hash, user.id, ws.id, exp, None, None)
        .await
    {
        tracing::error!(error=?e, "create session");
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
    }

    let (sid_cookie, csrf_cookie) = crate::auth::cookies::build_session_cookies(&state, &token);

    let mut resp = (
        StatusCode::CREATED,
        Json(SetupResponse {
            user_id: user.id.to_string(),
            workspace_id: ws.id.to_string(),
        }),
    )
        .into_response();
    crate::auth::cookies::append_session_cookies(&mut resp, &sid_cookie, &csrf_cookie);
    resp
}
