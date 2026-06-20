//! Share token management (owner-only):
//! POST   /api/docs/:doc_id/shares           { expires_at? } → 201
//! GET    /api/docs/:doc_id/shares           → 200 [ShareResponse, ...]
//! DELETE /api/docs/:doc_id/shares/:share_id → 204

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, post},
};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

#[derive(serde::Deserialize)]
struct CreateBody {
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(serde::Serialize)]
struct ShareResponse {
    id: String,
    token: String,
    url: String,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/docs/:doc_id/shares", post(create).get(list))
        .route("/api/docs/:doc_id/shares/:share_id", delete(revoke))
}

async fn require_owner(state: &AppState, ctx: &AuthContext, doc_id: Uuid) -> Option<Response> {
    let Some(acl) = state.acl.clone() else {
        return Some(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""));
    };
    match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(knot_storage::WorkspaceRole::Owner)) => None,
        Ok(_) => Some(json_err(
            StatusCode::FORBIDDEN,
            "acl.no_grant",
            "owner required",
        )),
        Err(_) => Some(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")),
    }
}

fn url_for(state: &AppState, token: &str) -> String {
    format!("{}/p/{}", state.base_url.trim_end_matches('/'), token)
}

async fn create(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if let Some(r) = require_owner(&state, &ctx, doc_id).await {
        return r;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), 4096).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let body: CreateBody = if bytes.is_empty() {
        CreateBody { expires_at: None }
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(b) => b,
            Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
        }
    };
    let Some(shares) = state.shares.clone() else {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
    };
    match shares
        .create(ctx.workspace_id, doc_id, body.expires_at, ctx.user_id)
        .await
    {
        Ok(s) => (
            StatusCode::CREATED,
            Json(ShareResponse {
                id: s.id.to_string(),
                url: url_for(&state, &s.token),
                token: s.token,
                expires_at: s.expires_at,
                created_at: s.created_at,
            }),
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "share create");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
        }
    }
}

async fn list(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if let Some(r) = require_owner(&state, &ctx, doc_id).await {
        return r;
    }
    let Some(shares) = state.shares.clone() else {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
    };
    match shares.list_active(doc_id).await {
        Ok(list) => Json(
            list.into_iter()
                .map(|s| ShareResponse {
                    id: s.id.to_string(),
                    url: url_for(&state, &s.token),
                    token: s.token,
                    expires_at: s.expires_at,
                    created_at: s.created_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(_) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""),
    }
}

async fn revoke(
    State(state): State<AppState>,
    Path((doc_id, share_id)): Path<(Uuid, Uuid)>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if let Some(r) = require_owner(&state, &ctx, doc_id).await {
        return r;
    }
    let Some(shares) = state.shares.clone() else {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
    };
    match shares.revoke(share_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(_) => json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""),
    }
}
