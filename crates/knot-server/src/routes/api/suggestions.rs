//! Change suggestions on documents, and their reply threads.
//!
//! POST   /api/docs/{id}/suggestions                       { kind, position_y, … } → 201 (Editor+)
//! GET    /api/docs/{id}/suggestions?include_resolved      → 200 [SuggestionWithReplies]
//!
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/reject   → 200 (Owner)
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/withdraw → 200 (author)
//!
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/replies            { body, mentions? } → 201
//! PATCH  /api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id} { body } → 200 (author)
//! DELETE /api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id} → 204 (author or owner)
//!
//! Mounted inside `docs::router`'s doc-id routes, so `require_doc_role_mw`
//! has already resolved the caller's effective role (and refused writes to an
//! archived doc). A suggestion never touches the Y.Doc here; only accept does.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{patch, post},
};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use knot_storage::suggestions::{
    MAX_CONTEXT_CHARS, MAX_REASON_CHARS, MAX_REPLY_CHARS, MAX_TEXT_CHARS,
};
use knot_storage::{
    NewSuggestion, Suggestion, SuggestionKind, SuggestionReply, SuggestionStore,
    SuggestionStoreError, WorkspaceRole,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use yrs::StickyIndex;
use yrs::updates::decoder::Decode;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole, require_viewer};
use crate::http_error::json_err;
use crate::routes::api::comments::{internal, require_editor};

/// Request cap for a new suggestion (the plan's 256 KiB). It covers every
/// text limit at 4 UTF-8 bytes per character, which is what `JSON.stringify`
/// sends. A client that `\uXXXX\uXXXX`-escapes every astral character at
/// every limit (12 bytes per character, ~285 KiB) does not fit and gets 413.
const MAX_CREATE_BYTES: usize = 256 * 1024;

/// Request cap for a reply create or edit: 4 096 characters, each escaped as
/// a surrogate pair (`\uXXXX\uXXXX`, 12 bytes), plus room for `mentions`.
const MAX_REPLY_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateBody {
    kind: SuggestionKind,
    /// Base64 Yjs RelativePosition of the range start.
    position_y: String,
    /// Base64 Yjs RelativePosition of the range end (equal to the start for
    /// an insertion).
    position_y_end: String,
    original_text: String,
    #[serde(default)]
    context_before: String,
    proposed_text: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ReplyBody {
    body: String,
    /// User ids the client's mention picker resolved.
    #[serde(default)]
    mentions: Vec<Uuid>,
}

#[derive(Deserialize)]
struct EditReplyBody {
    body: String,
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_resolved: bool,
}

/// One entry of the list response: the suggestion's fields, flattened, plus
/// its non-deleted replies, oldest first.
#[derive(Serialize)]
struct SuggestionWithReplies {
    #[serde(flatten)]
    suggestion: Suggestion,
    replies: Vec<SuggestionReply>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The caller's session, set by `require_session_mw`.
fn auth(req: &Request<Body>) -> Option<AuthContext> {
    req.extensions().get::<AuthContext>().cloned()
}

/// Whether the caller's effective role on the doc is Owner. The effective
/// doc role, not the workspace role: inside a restricted doc a workspace
/// owner may hold a lesser grant.
fn is_doc_owner(req: &Request<Body>) -> bool {
    matches!(
        req.extensions().get::<EffectiveDocRole>(),
        Some(EffectiveDocRole(WorkspaceRole::Owner))
    )
}

/// Handler-side gate for owner-only decisions: 401 without a session, 403
/// `acl.no_grant` without a role, 403 `acl.owner_required` below Owner.
pub(crate) fn require_owner(req: &Request<Body>) -> Option<Response> {
    if let Some(r) = require_viewer(req) {
        return Some(r);
    }
    if !is_doc_owner(req) {
        return Some(json_err(StatusCode::FORBIDDEN, "acl.owner_required", ""));
    }
    None
}

fn not_found() -> Response {
    json_err(StatusCode::NOT_FOUND, "suggestion.not_found", "")
}

fn not_pending() -> Response {
    json_err(StatusCode::CONFLICT, "suggestion.not_pending", "")
}

fn not_author() -> Response {
    json_err(StatusCode::FORBIDDEN, "suggestion.not_author", "")
}

fn too_long() -> Response {
    json_err(StatusCode::UNPROCESSABLE_ENTITY, "suggestion.too_long", "")
}

fn body_too_large() -> Response {
    json_err(
        StatusCode::PAYLOAD_TOO_LARGE,
        "suggestion.body_too_large",
        "",
    )
}

/// Map a store error from a status transition or reply write to its
/// response; `what` names the handler in the log line of an unexpected one.
fn store_err(e: SuggestionStoreError, what: &str) -> Response {
    match e {
        SuggestionStoreError::NotFound => not_found(),
        SuggestionStoreError::NotPending => not_pending(),
        SuggestionStoreError::TooLong => too_long(),
        SuggestionStoreError::EmptyBody => {
            json_err(StatusCode::BAD_REQUEST, "suggestion.invalid", "")
        }
        SuggestionStoreError::Sqlx(e) => {
            tracing::error!(error=?e, "{what}");
            internal()
        }
    }
}

/// Read and parse a JSON body capped at `limit` bytes: 413
/// `suggestion.body_too_large` over the cap, 400 `bad_request` when it does
/// not parse.
async fn read_json<T: serde::de::DeserializeOwned>(
    req: Request<Body>,
    limit: usize,
) -> Result<T, Response> {
    let bytes = axum::body::to_bytes(req.into_body(), limit)
        .await
        .map_err(|_| body_too_large())?;
    serde_json::from_slice(&bytes).map_err(|_| json_err(StatusCode::BAD_REQUEST, "bad_request", ""))
}

/// Fetch `suggestion_id` and check it belongs to `doc_id`. `require_doc_role`
/// only authorizes the caller on the path's doc; without this a member of
/// doc A could decide a suggestion of doc B by pairing A's id with B's
/// suggestion id (cross-document IDOR). 404 `suggestion.not_found` either way.
pub(crate) async fn ensure_suggestion_in_doc(
    store: &Arc<dyn SuggestionStore>,
    suggestion_id: Uuid,
    doc_id: Uuid,
) -> Result<Suggestion, Response> {
    match store.get(suggestion_id).await {
        Ok(s) if s.doc_id == doc_id => Ok(s),
        Ok(_) | Err(SuggestionStoreError::NotFound) => Err(not_found()),
        Err(e) => {
            tracing::error!(error=?e, "ensure_suggestion_in_doc");
            Err(internal())
        }
    }
}

/// Fetch `reply_id` and check it belongs to `suggestion_id` (which
/// [`ensure_suggestion_in_doc`] has tied to the path's doc). 404
/// `suggestion.not_found` when it is missing, deleted or elsewhere.
async fn ensure_reply_in_suggestion(
    store: &Arc<dyn SuggestionStore>,
    reply_id: Uuid,
    suggestion_id: Uuid,
) -> Result<SuggestionReply, Response> {
    match store.get_reply(reply_id).await {
        Ok(r) if r.suggestion_id == suggestion_id => Ok(r),
        Ok(_) | Err(SuggestionStoreError::NotFound) => Err(not_found()),
        Err(e) => {
            tracing::error!(error=?e, "ensure_reply_in_suggestion");
            Err(internal())
        }
    }
}

/// Decode a base64 anchor and check yrs reads it as a RelativePosition.
fn decode_anchor(b64: &str) -> Option<Vec<u8>> {
    let bytes = STANDARD.decode(b64).ok()?;
    StickyIndex::decode_v1(&bytes).ok()?;
    Some(bytes)
}

/// Whether the texts fit the kind: a replace changes a non-empty range to a
/// different non-empty text, a delete empties a non-empty range, an insert
/// adds non-empty text at a point. The proposed text is one line.
fn kind_matches(kind: SuggestionKind, original: &str, proposed: &str) -> bool {
    if proposed.contains(['\n', '\r']) {
        return false;
    }
    match kind {
        SuggestionKind::Replace => {
            !original.is_empty() && !proposed.is_empty() && original != proposed
        }
        SuggestionKind::Delete => !original.is_empty() && proposed.is_empty(),
        SuggestionKind::Insert => original.is_empty() && !proposed.is_empty(),
    }
}

fn over(s: &str, max: usize) -> bool {
    s.chars().count() > max
}

/// Fire-and-forget: tell any active room for `doc_id` that its suggestions
/// changed, so connected clients refetch. Shares the comments' channel; the
/// `topic` tells the client which query to invalidate, and
/// `comments_listener` forwards the payload verbatim.
pub(crate) fn notify_suggestion_change(state: &AppState, doc_id: Uuid) {
    let Some(pool) = state.pool.as_ref().cloned() else {
        return;
    };
    let payload =
        serde_json::json!({ "doc_id": doc_id.to_string(), "topic": "suggestions" }).to_string();
    tokio::spawn(async move {
        let _ = sqlx::query("SELECT pg_notify('doc_comments', $1)")
            .bind(&payload)
            .execute(&pool)
            .await;
    });
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// POST /api/docs/{id}/suggestions
async fn create(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(ctx) = auth(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let body: CreateBody = match read_json(req, MAX_CREATE_BYTES).await {
        Ok(b) => b,
        Err(r) => return r,
    };

    let (Some(position_y), Some(position_y_end)) = (
        decode_anchor(&body.position_y),
        decode_anchor(&body.position_y_end),
    ) else {
        return json_err(StatusCode::BAD_REQUEST, "suggestion.invalid_anchor", "");
    };
    if !kind_matches(body.kind, &body.original_text, &body.proposed_text) {
        return json_err(StatusCode::BAD_REQUEST, "suggestion.invalid", "");
    }
    let reason = body.reason.filter(|r| !r.is_empty());
    if over(&body.original_text, MAX_TEXT_CHARS)
        || over(&body.proposed_text, MAX_TEXT_CHARS)
        || over(&body.context_before, MAX_CONTEXT_CHARS)
        || reason.as_deref().is_some_and(|r| over(r, MAX_REASON_CHARS))
    {
        return too_long();
    }

    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    let new = NewSuggestion {
        doc_id,
        author_id: ctx.user_id,
        kind: body.kind,
        position_y,
        position_y_end,
        original_text: body.original_text,
        context_before: body.context_before,
        proposed_text: body.proposed_text,
        reason,
    };
    match store.create(&new).await {
        Ok(s) => {
            // Task 6: notify the page's owners (`suggestion` kind).
            notify_suggestion_change(&state, doc_id);
            (StatusCode::CREATED, Json(s)).into_response()
        }
        Err(e) => store_err(e, "create_suggestion"),
    }
}

/// GET /api/docs/{id}/suggestions
async fn list(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ListQuery>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_viewer(&req) {
        return r;
    }
    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    let (suggestions, replies) = match tokio::try_join!(
        store.list(doc_id, q.include_resolved),
        store.list_replies(doc_id)
    ) {
        Ok(both) => both,
        Err(e) => {
            tracing::error!(error=?e, "list_suggestions");
            return internal();
        }
    };
    let mut by_suggestion: HashMap<Uuid, Vec<SuggestionReply>> = HashMap::new();
    for r in replies {
        by_suggestion.entry(r.suggestion_id).or_default().push(r);
    }
    let out: Vec<SuggestionWithReplies> = suggestions
        .into_iter()
        .map(|s| SuggestionWithReplies {
            replies: by_suggestion.remove(&s.id).unwrap_or_default(),
            suggestion: s,
        })
        .collect();
    Json(out).into_response()
}

/// POST /api/docs/{id}/suggestions/{suggestion_id}/reject — Owner
async fn reject(
    State(state): State<AppState>,
    Path((doc_id, suggestion_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_owner(&req) {
        return r;
    }
    let Some(ctx) = auth(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    if let Err(r) = ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        return r;
    }
    match store.reject(suggestion_id, ctx.user_id).await {
        Ok(s) => {
            // Task 6: notify the author (`suggestion_decided` kind).
            notify_suggestion_change(&state, doc_id);
            Json(s).into_response()
        }
        Err(e) => store_err(e, "reject_suggestion"),
    }
}

/// POST /api/docs/{id}/suggestions/{suggestion_id}/withdraw — author only.
/// Like comment edit/delete, it also needs Editor+ on the doc: an author
/// later downgraded to Viewer gets 403 `acl.editor_required`.
async fn withdraw(
    State(state): State<AppState>,
    Path((doc_id, suggestion_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(ctx) = auth(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    let existing = match ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    if existing.author_id != ctx.user_id {
        return not_author();
    }
    match store.withdraw(suggestion_id).await {
        Ok(s) => {
            notify_suggestion_change(&state, doc_id);
            Json(s).into_response()
        }
        Err(e) => store_err(e, "withdraw_suggestion"),
    }
}

/// POST /api/docs/{id}/suggestions/{suggestion_id}/replies — Editor+
async fn create_reply(
    State(state): State<AppState>,
    Path((doc_id, suggestion_id)): Path<(Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(ctx) = auth(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    if let Err(r) = ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        return r;
    }
    let body: ReplyBody = match read_json(req, MAX_REPLY_BYTES).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if over(&body.body, MAX_REPLY_CHARS) {
        return too_long();
    }
    // Task 6: `reply` / `mention` notifications use these picker ids.
    let _mentions = body.mentions;
    match store
        .create_reply(suggestion_id, ctx.user_id, &body.body)
        .await
    {
        Ok(r) => {
            notify_suggestion_change(&state, doc_id);
            (StatusCode::CREATED, Json(r)).into_response()
        }
        Err(e) => store_err(e, "create_suggestion_reply"),
    }
}

/// PATCH /api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id} — author only
async fn edit_reply(
    State(state): State<AppState>,
    Path((doc_id, suggestion_id, reply_id)): Path<(Uuid, Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(ctx) = auth(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    if let Err(r) = ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        return r;
    }
    let existing = match ensure_reply_in_suggestion(&store, reply_id, suggestion_id).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if existing.author_id != ctx.user_id {
        return not_author();
    }
    let body: EditReplyBody = match read_json(req, MAX_REPLY_BYTES).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if over(&body.body, MAX_REPLY_CHARS) {
        return too_long();
    }
    match store.update_reply(reply_id, &body.body).await {
        Ok(r) => {
            // Task 6: `mention` notifications for names new to the body.
            notify_suggestion_change(&state, doc_id);
            Json(r).into_response()
        }
        Err(e) => store_err(e, "edit_suggestion_reply"),
    }
}

/// DELETE /api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id} — author or doc owner
async fn delete_reply(
    State(state): State<AppState>,
    Path((doc_id, suggestion_id, reply_id)): Path<(Uuid, Uuid, Uuid)>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    let Some(ctx) = auth(&req) else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(store) = state.suggestions.clone() else {
        return internal();
    };
    if let Err(r) = ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        return r;
    }
    let existing = match ensure_reply_in_suggestion(&store, reply_id, suggestion_id).await {
        Ok(r) => r,
        Err(r) => return r,
    };
    if existing.author_id != ctx.user_id && !is_doc_owner(&req) {
        return not_author();
    }
    match store.delete_reply(reply_id).await {
        Ok(()) => {
            notify_suggestion_change(&state, doc_id);
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => store_err(e, "delete_suggestion_reply"),
    }
}

// ---------------------------------------------------------------------------
// Router — mounted inside docs::router doc_id_routes so require_doc_role_mw applies
// ---------------------------------------------------------------------------

/// The suggestion routes, all under `/api/docs/{id}/suggestions`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/docs/{id}/suggestions", post(create).get(list))
        .route(
            "/api/docs/{id}/suggestions/{suggestion_id}/reject",
            post(reject),
        )
        .route(
            "/api/docs/{id}/suggestions/{suggestion_id}/withdraw",
            post(withdraw),
        )
        .route(
            "/api/docs/{id}/suggestions/{suggestion_id}/replies",
            post(create_reply),
        )
        .route(
            "/api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id}",
            patch(edit_reply).delete(delete_reply),
        )
}
