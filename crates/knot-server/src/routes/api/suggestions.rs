//! Change suggestions on documents, and their reply threads.
//!
//! POST   /api/docs/{id}/suggestions                       { kind, position_y, … } → 201 (Editor+)
//! GET    /api/docs/{id}/suggestions?include_resolved      → 200 [SuggestionWithReplies]
//!
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/accept   { seen_text } → 200 (Owner)
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/reject   → 200 (Owner)
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/withdraw → 200 (author)
//!
//! POST   /api/docs/{id}/suggestions/{suggestion_id}/replies            { body, mentions? } → 201
//! PATCH  /api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id} { body, mentions? } → 200 (author)
//!        (both: 422 `suggestion.empty_body` for an empty body, 422 `suggestion.too_long` over 4 096 chars)
//! DELETE /api/docs/{id}/suggestions/{suggestion_id}/replies/{reply_id} → 204 (author or owner)
//!
//! Mounted inside `docs::router`'s doc-id routes, so `require_doc_role_mw`
//! has already resolved the caller's effective role (and refused writes to an
//! archived doc). A suggestion never touches the Y.Doc here; only accept
//! does, and it does so through the doc's room
//! (`knot_crdt::Event::ApplySuggestion`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

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
use knot_crdt::{ApplySuggestionError, ApplySuggestionRequest};
use knot_storage::suggestions::{
    MAX_CONTEXT_CHARS, MAX_REASON_CHARS, MAX_REPLY_CHARS, MAX_TEXT_CHARS,
};
use knot_storage::{
    NewSuggestion, Suggestion, SuggestionKind, SuggestionReply, SuggestionStatus, SuggestionStore,
    SuggestionStoreError, WorkspaceRole,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use yrs::StickyIndex;
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole, require_viewer};
use crate::http_error::json_err;
use crate::routes::api::comments::{internal, require_editor};
use crate::suggestion_notifications::{
    ReplyWrite, emit_suggestion_created, emit_suggestion_decided, emit_suggestion_reply,
};

/// Request cap for a new suggestion (the plan's 256 KiB). It covers every
/// text limit at 4 UTF-8 bytes per character, which is what `JSON.stringify`
/// sends. A client that `\uXXXX\uXXXX`-escapes every astral character at
/// every limit (12 bytes per character, ~285 KiB) does not fit and gets 413.
const MAX_CREATE_BYTES: usize = 256 * 1024;

/// Request cap for a reply create or edit: 4 096 characters, each escaped as
/// a surrogate pair (`\uXXXX\uXXXX`, 12 bytes), plus room for `mentions`.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// How long accept waits for the room to take and apply the change, counted
/// from the claim (it covers queueing the event on a full room mailbox as
/// well as the reply). Well under [`crate::suggestions_sweep::STUCK_AFTER`],
/// so the sweep never looks at a row whose accept is still waiting on its
/// room.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(60);

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
struct AcceptBody {
    /// The "Now" text the owner's card showed (empty for an insertion). The
    /// room applies only if the range still holds exactly this text.
    seen_text: String,
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
    /// User ids the client's mention picker resolved in the edited body.
    #[serde(default)]
    mentions: Vec<Uuid>,
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

/// 422 for an empty reply body, refused before the store is called.
fn empty_body() -> Response {
    json_err(
        StatusCode::UNPROCESSABLE_ENTITY,
        "suggestion.empty_body",
        "",
    )
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
        SuggestionStoreError::EmptyBody => empty_body(),
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

/// Decode a base64 anchor and check it is exactly one RelativePosition:
/// `decode_v1` ignores bytes after a valid position, so the bytes must also
/// re-encode to themselves (no trailing garbage stored with the row).
fn decode_anchor(b64: &str) -> Option<Vec<u8>> {
    let bytes = STANDARD.decode(b64).ok()?;
    let pos = StickyIndex::decode_v1(&bytes).ok()?;
    (pos.encode_v1() == bytes).then_some(bytes)
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

/// The room's name for a stored suggestion kind.
fn crdt_kind(kind: SuggestionKind) -> knot_crdt::SuggestionKind {
    match kind {
        SuggestionKind::Replace => knot_crdt::SuggestionKind::Replace,
        SuggestionKind::Insert => knot_crdt::SuggestionKind::Insert,
        SuggestionKind::Delete => knot_crdt::SuggestionKind::Delete,
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
            emit_suggestion_created(&state, ctx.workspace_id, &s).await;
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

/// POST /api/docs/{id}/suggestions/{suggestion_id}/accept — Owner
///
/// Claims the row (`pending → applying`), has the doc's room apply the
/// change in one transaction (which also records the suggestion as applied
/// in the doc), then marks the row `accepted`. A refusal from the room
/// releases the row back to `pending`. When the outcome is unknown (the room
/// timed out, went away or could not persist) the row stays `applying` and
/// `suggestions_sweep` settles it from the doc's applied marker.
async fn accept(
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
    // `seen_text` is at most a range's text, so the create cap covers it.
    let body: AcceptBody = match read_json(req, MAX_CREATE_BYTES).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if over(&body.seen_text, MAX_TEXT_CHARS) {
        return too_long();
    }

    // The room first: a failure here must leave the row untouched.
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };
    let room = match rooms.acquire(doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "accept_suggestion: room acquire failed");
            return internal();
        }
    };

    let claimed = match store.claim_for_accept(suggestion_id, ctx.user_id).await {
        Ok(s) => s,
        Err(e) => return store_err(e, "claim_suggestion"),
    };
    // One deadline from the claim (which set `updated_at`) for both queueing
    // the event and the reply, so accept settles well under `STUCK_AFTER`.
    let deadline = tokio::time::Instant::now() + ACCEPT_TIMEOUT;
    let request = ApplySuggestionRequest {
        suggestion_id,
        kind: crdt_kind(claimed.kind),
        start: claimed.position_y,
        end: claimed.position_y_end,
        seen_text: body.seen_text,
        proposed_text: claimed.proposed_text,
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    let sent = tokio::time::timeout_at(
        deadline,
        room.tx.send(knot_crdt::Event::ApplySuggestion {
            request,
            // The suggester wrote the words, so the edit is theirs.
            by_user: Some(claimed.author_id),
            reply: tx,
        }),
    )
    .await;
    match sent {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            // The room is gone and never got the event, so nothing was applied.
            release_after_refusal(&store, suggestion_id).await;
            return internal();
        }
        Err(_) => {
            // `mpsc::Sender::send` is cancel safe: a send that timed out
            // waiting for mailbox space was never queued, so nothing was
            // applied.
            tracing::error!(%suggestion_id, "accept_suggestion: room mailbox full until the deadline");
            release_after_refusal(&store, suggestion_id).await;
            return internal();
        }
    }
    let outcome = match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => {
            tracing::error!(%suggestion_id, "accept_suggestion: room dropped the reply; left for the sweep");
            return internal();
        }
        Err(_) => {
            tracing::error!(%suggestion_id, "accept_suggestion: room timed out; left for the sweep");
            return internal();
        }
    };

    match outcome {
        Ok(_) | Err(ApplySuggestionError::AlreadyApplied) => {
            finish(&state, &store, &ctx, doc_id, suggestion_id).await
        }
        Err(ApplySuggestionError::AnchorLost) => {
            release_after_refusal(&store, suggestion_id).await;
            json_err(StatusCode::CONFLICT, "suggestion.anchor_lost", "")
        }
        Err(ApplySuggestionError::Changed) => {
            release_after_refusal(&store, suggestion_id).await;
            json_err(StatusCode::CONFLICT, "suggestion.changed", "")
        }
        Err(ApplySuggestionError::Decode(e)) => {
            tracing::warn!(%suggestion_id, error=%e, "accept_suggestion: anchor does not decode");
            release_after_refusal(&store, suggestion_id).await;
            json_err(StatusCode::BAD_REQUEST, "suggestion.invalid_anchor", "")
        }
        Err(ApplySuggestionError::Persist(e)) => {
            // The room evicted itself; whether the change is durable is
            // for the sweep to read from storage.
            tracing::error!(%suggestion_id, error=%e, "accept_suggestion: persist failed; left for the sweep");
            internal()
        }
    }
}

/// Mark an applied suggestion `accepted` and answer 200 with it. A row that
/// is no longer `applying`/`pending` was most likely finished already by the
/// sweep; it is answered as 200 when it is `accepted` too.
async fn finish(
    state: &AppState,
    store: &Arc<dyn SuggestionStore>,
    ctx: &AuthContext,
    doc_id: Uuid,
    suggestion_id: Uuid,
) -> Response {
    let finished = match store.finish_accept(suggestion_id, ctx.user_id).await {
        Ok(s) => s,
        Err(SuggestionStoreError::NotPending) => match store.get(suggestion_id).await {
            Ok(s) if s.status == SuggestionStatus::Accepted => s,
            Ok(s) => {
                tracing::error!(
                    %suggestion_id,
                    status = s.status.as_str(),
                    "accept_suggestion: applied in the doc but the row moved elsewhere"
                );
                return internal();
            }
            Err(e) => return store_err(e, "accept_suggestion: re-read"),
        },
        Err(e) => return store_err(e, "finish_accept"),
    };
    emit_suggestion_decided(state, ctx.workspace_id, &finished).await;
    notify_suggestion_change(state, doc_id);
    Json(finished).into_response()
}

/// Return a claimed row to `pending` after the room refused (or never got)
/// the change. A failure is only logged: the row stays `applying`, and the
/// sweep releases it once it finds no applied marker in the doc.
async fn release_after_refusal(store: &Arc<dyn SuggestionStore>, suggestion_id: Uuid) {
    if let Err(e) = store.release(suggestion_id).await {
        tracing::error!(error=?e, %suggestion_id, "accept_suggestion: release failed; left for the sweep");
    }
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
            emit_suggestion_decided(&state, ctx.workspace_id, &s).await;
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
    let suggestion = match ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let body: ReplyBody = match read_json(req, MAX_REPLY_BYTES).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    if body.body.is_empty() {
        return empty_body();
    }
    if over(&body.body, MAX_REPLY_CHARS) {
        return too_long();
    }
    match store
        .create_reply(suggestion_id, ctx.user_id, &body.body)
        .await
    {
        Ok(r) => {
            emit_suggestion_reply(
                &state,
                ctx.workspace_id,
                &suggestion,
                &r,
                ReplyWrite::Created,
                &body.mentions,
            )
            .await;
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
    let suggestion = match ensure_suggestion_in_doc(&store, suggestion_id, doc_id).await {
        Ok(s) => s,
        Err(r) => return r,
    };
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
    if body.body.is_empty() {
        return empty_body();
    }
    if over(&body.body, MAX_REPLY_CHARS) {
        return too_long();
    }
    match store.update_reply(reply_id, &body.body).await {
        Ok(r) => {
            // The dedupe key makes a name already mentioned a no-op, so
            // only names new to the body notify.
            emit_suggestion_reply(
                &state,
                ctx.workspace_id,
                &suggestion,
                &r,
                ReplyWrite::Edited,
                &body.mentions,
            )
            .await;
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
            "/api/docs/{id}/suggestions/{suggestion_id}/accept",
            post(accept),
        )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decode_anchor_takes_yjs_anchors_and_refuses_trailing_bytes() {
        // Made by yjs 13.6 through the client's encoder (the same fixtures as
        // `knot_crdt::suggestions`): item-anchored start and end, and a point
        // anchored on the XmlText itself with either assoc.
        for fixture in ["00070400", "00070541", "02070141", "02070100"] {
            let b64 = STANDARD.encode(hex(fixture));
            assert_eq!(decode_anchor(&b64), Some(hex(fixture)), "{fixture}");
        }
        let mut trailing = hex("00070400");
        trailing.push(0x00);
        assert_eq!(decode_anchor(&STANDARD.encode(trailing)), None);
        assert_eq!(decode_anchor(&STANDARD.encode(b"position_bytes")), None);
    }
}
