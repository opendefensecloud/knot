//! Excalidraw boards:
//!
//!   POST   /api/docs/:doc_id/boards          → 201 { id, doc_id, label, … }       (editor+)
//!   GET    /api/docs/:doc_id/boards          → 200 [Board, …]                     (viewer+)
//!   DELETE /api/boards/:id                   → 204                                (editor+ on parent)
//!   GET    /api/boards/:id/svg               → image/svg+xml (cached preview)      (viewer+)
//!   PUT    /api/boards/:id/svg               → 204                                (editor+)
//!
//! ACL is inherited from the parent document; there's no per-board grant table.
//! Viewers see the cached SVG preview but cannot upload a new one or join the
//! board's collaborative WS (the upgrade handler also enforces editor+ for v1).

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

const SVG_MAX_BYTES: usize = 1024 * 1024; // 1 MB cap on preview uploads.

#[derive(serde::Deserialize)]
struct CreateBody {
    #[serde(default)]
    label: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/docs/:doc_id/boards", post(create).get(list))
        .route("/api/boards/:id", delete(remove))
        .route("/api/boards/:id/svg", get(get_svg).put(put_svg))
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

async fn require_doc_role(
    state: &AppState,
    ctx: &AuthContext,
    doc_id: Uuid,
    editor_required: bool,
) -> Option<Response> {
    let Some(acl) = state.acl.clone() else {
        return Some(internal());
    };
    match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(role)) => {
            use knot_storage::WorkspaceRole;
            let ok = if editor_required {
                matches!(role, WorkspaceRole::Owner | WorkspaceRole::Editor)
            } else {
                true
            };
            if !ok {
                Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""))
            } else {
                None
            }
        }
        Ok(None) => Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", "")),
        Err(_) => Some(internal()),
    }
}

async fn create(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if let Some(r) = require_doc_role(&state, &ctx, doc_id, true).await {
        return r;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), 4096).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let body: CreateBody = if bytes.is_empty() {
        CreateBody { label: None }
    } else {
        match serde_json::from_slice(&bytes) {
            Ok(b) => b,
            Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
        }
    };
    let Some(boards) = state.boards.clone() else {
        return internal();
    };
    match boards.create(doc_id, ctx.user_id, body.label).await {
        Ok(b) => (StatusCode::CREATED, Json(b)).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "board create");
            internal()
        }
    }
}

async fn list(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if let Some(r) = require_doc_role(&state, &ctx, doc_id, false).await {
        return r;
    }
    let Some(boards) = state.boards.clone() else {
        return internal();
    };
    match boards.list_for_doc(doc_id).await {
        Ok(list) => Json(list).into_response(),
        Err(_) => internal(),
    }
}

async fn remove(State(state): State<AppState>, Path(id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(boards) = state.boards.clone() else {
        return internal();
    };
    let board = match boards.get(id).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::NOT_FOUND, "board.not_found", ""),
    };
    if let Some(r) = require_doc_role(&state, &ctx, board.doc_id, true).await {
        return r;
    }
    if let Err(e) = boards.delete(id).await {
        tracing::error!(error=?e, "board delete");
        return internal();
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn get_svg(State(state): State<AppState>, Path(id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(boards) = state.boards.clone() else {
        return internal();
    };
    let board = match boards.get(id).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::NOT_FOUND, "board.not_found", ""),
    };
    if let Some(r) = require_doc_role(&state, &ctx, board.doc_id, false).await {
        return r;
    }
    let svg = match boards.get_svg(id).await {
        Ok(s) => s,
        Err(_) => return internal(),
    };
    match svg {
        Some(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")
            .header(header::CACHE_CONTROL, "private, max-age=10")
            .body(Body::from(bytes))
            .unwrap(),
        None => json_err(StatusCode::NOT_FOUND, "board.no_preview", ""),
    }
}

async fn put_svg(State(state): State<AppState>, Path(id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(boards) = state.boards.clone() else {
        return internal();
    };
    let board = match boards.get(id).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::NOT_FOUND, "board.not_found", ""),
    };
    if let Some(r) = require_doc_role(&state, &ctx, board.doc_id, true).await {
        return r;
    }
    let bytes = match axum::body::to_bytes(req.into_body(), SVG_MAX_BYTES).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "board.svg_too_large", ""),
    };
    if let Err(e) = boards.set_svg(id, &bytes).await {
        tracing::error!(error=?e, "board set_svg");
        return internal();
    }
    StatusCode::NO_CONTENT.into_response()
}
