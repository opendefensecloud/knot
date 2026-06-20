//! Comment threads on documents.
//!
//! POST   /api/docs/:doc_id/comments                  { body, position_y?, anchor_text? } → 201
//! POST   /api/docs/:doc_id/comments/:thread_id/replies { body } → 201
//! GET    /api/docs/:doc_id/comments?include_resolved  → 200 [Comment]
//!
//! POST   /api/docs/:doc_id/comments/:thread_id/resolve   → 204
//! POST   /api/docs/:doc_id/comments/:thread_id/unresolve → 204
//!
//! POST   /api/docs/:doc_id/comments/:comment_id/reactions        { emoji } → 204
//! DELETE /api/docs/:doc_id/comments/:comment_id/reactions/:emoji  → 204
//!
//! PATCH  /api/docs/:doc_id/comments/:comment_id { body } → 200 (author only)
//! DELETE /api/docs/:doc_id/comments/:comment_id          → 204 (author or workspace owner)

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, patch, post},
};
use knot_storage::{CommentStoreError, WorkspaceRole};
use regex::Regex;
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

// ---------------------------------------------------------------------------
// Allowed emojis
// ---------------------------------------------------------------------------

const ALLOWED_EMOJIS: &[&str] = &["👍", "🎉", "❤️", "🚀", "👀", "🙏"];

fn emoji_allowed(e: &str) -> bool {
    ALLOWED_EMOJIS.contains(&e)
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateThreadBody {
    body: String,
    /// Base64-encoded Yjs RelativePosition bytes for the START of the range.
    #[serde(default)]
    position_y: Option<String>,
    /// Base64-encoded Yjs RelativePosition bytes for the END of the range.
    #[serde(default)]
    position_y_end: Option<String>,
    #[serde(default)]
    anchor_text: Option<String>,
}

#[derive(Deserialize)]
struct CreateReplyBody {
    body: String,
}

#[derive(Deserialize)]
struct EditBody {
    body: String,
}

#[derive(Deserialize)]
struct ReactionBody {
    emoji: String,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_resolved: bool,
}

// ---------------------------------------------------------------------------
// ACL helpers — read from extensions set by require_doc_role_mw
// ---------------------------------------------------------------------------

fn require_auth(req: &Request<Body>) -> Option<&AuthContext> {
    req.extensions().get::<AuthContext>()
}

fn require_editor(req: &Request<Body>) -> Option<Response> {
    if req.extensions().get::<AuthContext>().is_none() {
        return Some(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    }
    match req.extensions().get::<EffectiveDocRole>().copied() {
        None => Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", "")),
        Some(role) if role.0 == WorkspaceRole::Viewer => {
            Some(json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""))
        }
        Some(_) => None,
    }
}

fn require_viewer(req: &Request<Body>) -> Option<Response> {
    if req.extensions().get::<AuthContext>().is_none() {
        return Some(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    }
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""));
    }
    None
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

// ---------------------------------------------------------------------------
// Mention extraction + broadcast
// ---------------------------------------------------------------------------

/// Extract @mention handles from comment body.
fn extract_mentions(body: &str) -> Vec<String> {
    // Pattern: word-boundary @ followed by \w+
    let re = Regex::new(r"(?:^|\s)@(\w+)").expect("valid regex");
    re.captures_iter(body)
        .filter_map(|cap| cap.get(1))
        .map(|m| m.as_str().to_lowercase())
        .collect()
}

/// Fire-and-forget mention notification via Postgres LISTEN/NOTIFY channel
/// `comment_mentions`. Payload: JSON `{type, doc_id, comment_id, user_ids}`.
async fn broadcast_mentions(state: &AppState, doc_id: Uuid, comment_id: Uuid, body: &str) {
    let handles = extract_mentions(body);
    if handles.is_empty() {
        return;
    }
    let Some(workspaces) = state.workspaces.clone() else {
        return;
    };
    let Some(ctx) = state.pool.as_ref() else {
        return;
    };
    // We need the workspace_id. Fetch from doc state via the docs store.
    // Actually we need workspace_id for list_members; look it up via the doc.
    let Some(docs) = state.docs.clone() else {
        return;
    };
    let ws_id = match docs.get(doc_id).await {
        Ok(Some(d)) => d.workspace_id,
        Ok(None) => return,
        Err(_) => return,
    };
    let members = match workspaces.list_members(ws_id).await {
        Ok(m) => m,
        Err(_) => return,
    };
    let user_ids: Vec<Uuid> = members
        .into_iter()
        .filter(|m| handles.contains(&m.display_name.to_lowercase()))
        .map(|m| m.user_id)
        .collect();
    if user_ids.is_empty() {
        return;
    }
    let payload = serde_json::json!({
        "type": "mention",
        "doc_id": doc_id,
        "comment_id": comment_id,
        "user_ids": user_ids,
    });
    let payload_str = payload.to_string();
    // Fire and forget — don't fail the request on notify errors.
    let pool = ctx.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("SELECT pg_notify('comment_mentions', $1)")
            .bind(&payload_str)
            .execute(&pool)
            .await;
    });
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// POST /api/docs/:doc_id/comments
async fn create_thread(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", ""),
    };
    let body_req: CreateThreadBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if body_req.body.len() > 4096 {
        return json_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "comment.body_too_large",
            "body must be ≤ 4096 chars",
        );
    }

    // Decode position_y / position_y_end from base64 if present.
    let position_y: Option<Vec<u8>> = match body_req.position_y {
        None => None,
        Some(ref s) => {
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s) {
                Ok(b) => Some(b),
                Err(_) => {
                    return json_err(StatusCode::BAD_REQUEST, "comment.invalid_position_y", "");
                }
            }
        }
    };
    let position_y_end: Option<Vec<u8>> = match body_req.position_y_end {
        None => None,
        Some(ref s) => {
            match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, s) {
                Ok(b) => Some(b),
                Err(_) => {
                    return json_err(
                        StatusCode::BAD_REQUEST,
                        "comment.invalid_position_y_end",
                        "",
                    );
                }
            }
        }
    };

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    match comments
        .create_thread(
            doc_id,
            ctx.user_id,
            &body_req.body,
            position_y,
            position_y_end,
            body_req.anchor_text,
        )
        .await
    {
        Ok(c) => {
            let comment_id = c.id;
            let body_text = c.body.clone();
            let response = (StatusCode::CREATED, Json(c)).into_response();
            broadcast_mentions(&state, doc_id, comment_id, &body_text).await;
            response
        }
        Err(CommentStoreError::BodyTooLong) => {
            json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "create_thread");
            internal()
        }
    }
}

/// POST /api/docs/:doc_id/comments/:thread_id/replies
async fn create_reply(
    State(state): State<AppState>,
    Path((doc_id, thread_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", ""),
    };
    let body_req: CreateReplyBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if body_req.body.len() > 4096 {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "");
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    match comments
        .create_reply(doc_id, thread_id, ctx.user_id, &body_req.body)
        .await
    {
        Ok(c) => {
            let comment_id = c.id;
            let body_text = c.body.clone();
            let response = (StatusCode::CREATED, Json(c)).into_response();
            broadcast_mentions(&state, doc_id, comment_id, &body_text).await;
            response
        }
        Err(CommentStoreError::BodyTooLong) => {
            json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "create_reply");
            internal()
        }
    }
}

/// GET /api/docs/:doc_id/comments
async fn list_comments(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_viewer(&req) {
        return r;
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    match comments.list(doc_id, q.include_resolved).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list_comments");
            internal()
        }
    }
}

/// Reject mutating a comment that does not belong to `doc_id`. `require_doc_role`
/// only authorizes the caller on the path's doc; without this a member of doc A
/// could resolve/react-to/edit a comment in doc B by pairing A's id with B's
/// comment id (cross-document IDOR).
async fn ensure_comment_in_doc(
    comments: &std::sync::Arc<dyn knot_storage::CommentStore>,
    comment_id: Uuid,
    doc_id: Uuid,
) -> Result<(), Response> {
    match comments.get(comment_id).await {
        Ok(c) if c.doc_id == doc_id => Ok(()),
        Ok(_) | Err(CommentStoreError::NotFound) => {
            Err(json_err(StatusCode::NOT_FOUND, "comment.not_found", ""))
        }
        Err(e) => {
            tracing::error!(error=?e, "ensure_comment_in_doc");
            Err(internal())
        }
    }
}

/// POST /api/docs/:doc_id/comments/:thread_id/resolve
async fn resolve_thread(
    State(state): State<AppState>,
    Path((doc_id, thread_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, thread_id, doc_id).await {
        return r;
    }
    match comments.resolve(thread_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(CommentStoreError::NotFound) => json_err(
            StatusCode::NOT_FOUND,
            "comment.not_found",
            "thread not found or not a root",
        ),
        Err(e) => {
            tracing::error!(error=?e, "resolve_thread");
            internal()
        }
    }
}

/// POST /api/docs/:doc_id/comments/:thread_id/unresolve
async fn unresolve_thread(
    State(state): State<AppState>,
    Path((doc_id, thread_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, thread_id, doc_id).await {
        return r;
    }
    match comments.unresolve(thread_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(CommentStoreError::NotFound) => json_err(
            StatusCode::NOT_FOUND,
            "comment.not_found",
            "thread not found or not a root",
        ),
        Err(e) => {
            tracing::error!(error=?e, "unresolve_thread");
            internal()
        }
    }
}

/// POST /api/docs/:doc_id/comments/:comment_id/reactions
async fn add_reaction(
    State(state): State<AppState>,
    Path((doc_id, comment_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 256).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let body_req: ReactionBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if !emoji_allowed(&body_req.emoji) {
        return json_err(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "comment.invalid_emoji",
            "emoji not in allow-list",
        );
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, comment_id, doc_id).await {
        return r;
    }
    match comments
        .add_reaction(comment_id, ctx.user_id, &body_req.emoji)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "add_reaction");
            internal()
        }
    }
}

/// DELETE /api/docs/:doc_id/comments/:comment_id/reactions/:emoji
async fn remove_reaction(
    State(state): State<AppState>,
    Path((doc_id, comment_id, emoji)): Path<(Uuid, Uuid, String)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let Some(comments) = state.comments.clone() else {
        return internal();
    };
    if let Err(r) = ensure_comment_in_doc(&comments, comment_id, doc_id).await {
        return r;
    }
    match comments
        .remove_reaction(comment_id, ctx.user_id, &emoji)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "remove_reaction");
            internal()
        }
    }
}

/// PATCH /api/docs/:doc_id/comments/:comment_id — author only
async fn edit_comment(
    State(state): State<AppState>,
    Path((doc_id, comment_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let bytes = match axum::body::to_bytes(req.into_body(), 8192).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", ""),
    };
    let body_req: EditBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    if body_req.body.len() > 4096 {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "");
    }

    let Some(comments) = state.comments.clone() else {
        return internal();
    };

    // Fetch existing to check authorship.
    let existing = match comments.get(comment_id).await {
        Ok(c) => c,
        Err(CommentStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "edit_comment get");
            return internal();
        }
    };
    if existing.doc_id != doc_id {
        return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
    }
    if existing.author_id != ctx.user_id {
        return json_err(
            StatusCode::FORBIDDEN,
            "comment.not_author",
            "only the author can edit",
        );
    }

    match comments.update_body(comment_id, &body_req.body).await {
        Ok(c) => {
            let comment_id_val = c.id;
            let doc_id_val = c.doc_id;
            let body_text = c.body.clone();
            let response = Json(c).into_response();
            broadcast_mentions(&state, doc_id_val, comment_id_val, &body_text).await;
            response
        }
        Err(CommentStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "comment.not_found", "")
        }
        Err(CommentStoreError::BodyTooLong) => {
            json_err(StatusCode::PAYLOAD_TOO_LARGE, "comment.body_too_large", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "edit_comment update");
            internal()
        }
    }
}

/// DELETE /api/docs/:doc_id/comments/:comment_id — author or workspace owner
async fn delete_comment(
    State(state): State<AppState>,
    Path((doc_id, comment_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let ctx = match require_auth(&req) {
        Some(c) => c.clone(),
        None => return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", ""),
    };

    let Some(comments) = state.comments.clone() else {
        return internal();
    };

    let existing = match comments.get(comment_id).await {
        Ok(c) => c,
        Err(CommentStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "delete_comment get");
            return internal();
        }
    };

    if existing.doc_id != doc_id {
        return json_err(StatusCode::NOT_FOUND, "comment.not_found", "");
    }
    // Allow if author OR workspace owner.
    let is_author = existing.author_id == ctx.user_id;
    let is_workspace_owner = ctx.role == WorkspaceRole::Owner;
    if !is_author && !is_workspace_owner {
        return json_err(
            StatusCode::FORBIDDEN,
            "comment.not_author",
            "only author or workspace owner can delete",
        );
    }

    match comments.delete(comment_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "delete_comment");
            internal()
        }
    }
}

// ---------------------------------------------------------------------------
// Router — mounted inside docs::router doc_id_routes so require_doc_role_mw applies
// ---------------------------------------------------------------------------

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/docs/:id/comments",
            post(create_thread).get(list_comments),
        )
        .route(
            "/api/docs/:id/comments/:thread_id/replies",
            post(create_reply),
        )
        .route(
            "/api/docs/:id/comments/:thread_id/resolve",
            post(resolve_thread),
        )
        .route(
            "/api/docs/:id/comments/:thread_id/unresolve",
            post(unresolve_thread),
        )
        .route(
            "/api/docs/:id/comments/:comment_id/reactions",
            post(add_reaction),
        )
        .route(
            "/api/docs/:id/comments/:comment_id/reactions/:emoji",
            delete(remove_reaction),
        )
        .route(
            "/api/docs/:id/comments/:comment_id",
            patch(edit_comment).delete(delete_comment),
        )
}
