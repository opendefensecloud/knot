//! Personal API tokens — management endpoints (session-auth only; token
//! auth is rejected here, see `token_scopes`):
//! - POST   /api/tokens        body: {name, scopes: ["read",…]} → secret shown once
//! - GET    /api/tokens        list (never the secret or hash)
//! - DELETE /api/tokens/{id}   revoke

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, post},
};
use knot_storage::{API_SCOPES, ApiToken, ApiTokenSecret};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/tokens", post(create).get(list))
        .route("/api/tokens/{id}", delete(revoke))
}

#[derive(Deserialize)]
struct CreateToken {
    name: String,
    scopes: Vec<String>,
}

#[derive(Serialize)]
struct CreatedToken {
    #[serde(flatten)]
    token: TokenResponse,
    /// The plaintext secret. Returned exactly once, never stored.
    token_secret: String,
}

#[derive(Serialize)]
struct TokenResponse {
    id: String,
    name: String,
    scopes: Vec<String>,
    created_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn response(t: &ApiToken) -> TokenResponse {
    TokenResponse {
        id: t.id.to_string(),
        name: t.name.clone(),
        scopes: t.scopes.clone(),
        created_at: t.created_at,
        last_used_at: t.last_used_at,
        revoked_at: t.revoked_at,
    }
}

fn auth_ctx(req: &Request) -> Option<AuthContext> {
    req.extensions().get::<AuthContext>().cloned()
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

async fn read_json<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, ()> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

async fn create(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = auth_ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Ok(body) = read_json::<CreateToken>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let name = body.name.trim();
    if name.is_empty() || name.len() > 100 {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "token.name",
            "1-100 characters",
        );
    }
    // Reject unknown scopes (trust-boundary validation; the store re-checks).
    if body.scopes.is_empty()
        || body
            .scopes
            .iter()
            .any(|s| !API_SCOPES.contains(&s.as_str()))
    {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "token.scopes",
            "non-empty subset of: read, create, write, delete",
        );
    }
    let Some(tokens) = state.tokens.as_ref() else {
        return internal();
    };
    // dedupe + keep creation order stable
    let mut scopes = body.scopes.clone();
    scopes.dedup();
    match tokens
        .create(ctx.user_id, ctx.workspace_id, name, &scopes)
        .await
    {
        Ok(ApiTokenSecret { token, secret }) => (
            StatusCode::CREATED,
            Json(CreatedToken {
                token: response(&token),
                token_secret: secret,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error=?e, "api-token create failed");
            internal()
        }
    }
}

async fn list(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = auth_ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(tokens) = state.tokens.as_ref() else {
        return internal();
    };
    match tokens.list_for_user(ctx.user_id, ctx.workspace_id).await {
        Ok(list) => (
            StatusCode::OK,
            Json(list.iter().map(response).collect::<Vec<_>>()),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error=?e, "api-token list failed");
            internal()
        }
    }
}

async fn revoke(State(state): State<AppState>, Path(id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = auth_ctx(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(tokens) = state.tokens.as_ref() else {
        return internal();
    };
    match tokens.revoke(id, ctx.user_id, ctx.workspace_id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        // Not yours or unknown — same answer either way (IDOR).
        Ok(false) => json_err(StatusCode::NOT_FOUND, "token.not_found", ""),
        Err(e) => {
            tracing::warn!(error=?e, "api-token revoke failed");
            internal()
        }
    }
}
