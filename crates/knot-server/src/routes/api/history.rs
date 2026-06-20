//! Doc snapshot history endpoints:
//! GET  /api/docs/:doc_id/history              → metadata list (newest-first)
//! GET  /api/docs/:doc_id/history/:seq/markdown → markdown preview of snapshot
//! POST /api/docs/:doc_id/history/:seq/restore  → replace live doc with snapshot

use axum::{
    Json,
    body::Body,
    extract::{Path, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_crdt::{Engine, YrsEngine};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

/// Require the caller to be at least Editor (not Viewer-only).
/// Expects `EffectiveDocRole` already set by `require_doc_role_mw`.
fn require_editor(req: &Request) -> Option<Response> {
    if req.extensions().get::<AuthContext>().is_none() {
        return Some(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    }
    match req.extensions().get::<EffectiveDocRole>().copied() {
        None => Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", "")),
        Some(role) if role.0 == knot_storage::WorkspaceRole::Viewer => {
            Some(json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""))
        }
        Some(_) => None,
    }
}

pub async fn list(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(snapshots) = state.snapshots.clone() else {
        return internal();
    };
    match snapshots.list(doc_id, 50).await {
        Ok(metas) => Json(metas).into_response(),
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "history list");
            internal()
        }
    }
}

pub async fn preview_markdown(
    State(state): State<AppState>,
    Path((doc_id, seq)): Path<(Uuid, i64)>,
    req: Request,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(snapshots) = state.snapshots.clone() else {
        return internal();
    };
    let snap = match snapshots.by_seq(doc_id, seq).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return json_err(
                StatusCode::NOT_FOUND,
                "history.not_found",
                "snapshot not found",
            );
        }
        Err(e) => {
            tracing::error!(error=?e, %doc_id, seq, "history by_seq");
            return internal();
        }
    };

    // Load into a transient doc and serialize to markdown (mirrors export_inline).
    let engine = YrsEngine;
    let transient = engine.new_doc();
    if let Err(e) = engine.apply_update(&transient, &snap.state_bytes) {
        tracing::error!(error=?e, %doc_id, seq, "history apply_update");
        return internal();
    }
    let text = match knot_markdown::to_markdown::serialise(&engine, &transient) {
        Ok(md) => md,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, seq, "history serialise");
            return internal();
        }
    };

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
        .body(Body::from(text))
        .unwrap()
}

pub async fn restore(
    State(state): State<AppState>,
    Path((doc_id, seq)): Path<(Uuid, i64)>,
    req: Request,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(snapshots) = state.snapshots.clone() else {
        return internal();
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };

    // Fetch the snapshot.
    let snap = match snapshots.by_seq(doc_id, seq).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            return json_err(
                StatusCode::NOT_FOUND,
                "history.not_found",
                "snapshot not found",
            );
        }
        Err(e) => {
            tracing::error!(error=?e, %doc_id, seq, "restore by_seq");
            return internal();
        }
    };

    // Render the snapshot to markdown via a transient doc, then parse it
    // back to update bytes. This normalizes the content through the canonical
    // markdown round-trip (same loss profile as Plan 5 export).
    let engine = YrsEngine;
    let transient = engine.new_doc();
    if let Err(e) = engine.apply_update(&transient, &snap.state_bytes) {
        tracing::error!(error=?e, %doc_id, seq, "restore apply_update");
        return internal();
    }
    let markdown = match knot_markdown::to_markdown::serialise(&engine, &transient) {
        Ok(md) => md,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, seq, "restore serialise");
            return internal();
        }
    };
    let update_bytes = match knot_markdown::from_markdown::parse(&markdown) {
        Ok((_h, bytes)) => bytes,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, seq, "restore parse");
            return internal();
        }
    };

    // Send to the live room to replace content atomically.
    let room = rooms.acquire(doc_id).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    if room
        .tx
        .send(knot_crdt::Event::ReplaceWithMarkdown {
            update_bytes,
            reply: tx,
        })
        .await
        .is_err()
    {
        return internal();
    }
    match rx.await {
        Ok(Ok(_seq)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error=%e, %doc_id, seq, "restore replace");
            json_err(StatusCode::INTERNAL_SERVER_ERROR, "restore.apply", "")
        }
        Err(_) => internal(),
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
