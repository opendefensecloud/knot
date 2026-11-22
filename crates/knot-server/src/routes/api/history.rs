//! Doc history endpoints (editors and owners; also on an archived page):
//! GET  /api/docs/{doc_id}/history/entries      → page history entries (M1.3), keyset-paged
//! GET  /api/docs/{doc_id}/history/entries/{eid}/view?changes=0|1
//!                                              → a past version, rendered (M1.4)
//! POST /api/docs/{doc_id}/history/entries/{eid}/restore?ended_at=<RFC 3339>
//!                                              → revert the live doc in place to
//!                                                that version: a verified CRDT
//!                                                revert (M2, `history_restore`),
//!                                                200 `{"outcome": …}`
//! POST /api/docs/{doc_id}/history/entries/{eid}/restore-as-new
//!      body `{"version_label": …, "ended_at"?: <RFC 3339>}`
//!                                              → copy that version into a new
//!                                                page right after it, as
//!                                                Duplicate does (M2,
//!                                                `crate::duplicate`), 201 with
//!                                                the new page
//!
//! The snapshot routes before the timeline:
//! GET  /api/docs/{doc_id}/history              → `[]` (it no longer lists snapshots)
//! GET  /api/docs/{doc_id}/history/{seq}/markdown → markdown preview of snapshot
//! POST /api/docs/{doc_id}/history/{seq}/restore  → replace live doc with snapshot
//!
//! These three stay through the combined page-history release: a 0.9.14
//! drawer whose list came from an old pod still previews and restores on
//! a new pod during the upgrade, and the seq restore is the operator's
//! workaround for a page without a restorable entry. No client of the
//! release calls them; they go in the release after it. The seq restore
//! still replaces the page through the Markdown round trip
//! (`markdown_round_trip`); the entry restore no longer does.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use knot_crdt::{Engine, YrsEngine};
use knot_docs::DocAccess;
use knot_docs::acl::ResolveError;
use knot_history::{Cursor, DEFAULT_LIMIT, MAX_LIMIT, PlanError, REVERT_MIN_PLAN_TIME, ViewError};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, DocArchived, EffectiveDocRole, require_editor};
use crate::duplicate::VersionCopy;
use crate::history_restore::{
    LiveRoom, RESTORE_DEADLINE, RestoreFailure, restore_answer, revert_attempts,
};
use crate::http_error::json_err;

/// How long an entry restore waits for each of its room calls: the room's
/// acquire, each export and each apply, never past the restore's
/// `RESTORE_DEADLINE` (`history_restore`). The seq restore waits without
/// a bound, as before.
pub(crate) const RESTORE_ROOM_WAIT: Duration = Duration::from_secs(8);

/// Most characters a Restore as new page's `version_label` may have once
/// trimmed: the client sends a formatted date and time.
pub(crate) const VERSION_LABEL_MAX_CHARS: usize = 64;

/// `GET /api/docs/{id}/history`: the snapshot list of the drawer before
/// the timeline. It lists nothing (`[]`), behind its gates, so a 0.9.14
/// bundle that calls it during the upgrade gets JSON instead of the SPA's
/// `index.html` (module doc).
pub async fn list(req: Request) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(Vec::<()>::new()),
    )
        .into_response()
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
    // Present: require_editor rejected the request otherwise.
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(snapshots) = state.snapshots.clone() else {
        return internal();
    };
    if state.rooms_v2.is_none() {
        return internal();
    }

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

    // Normalize the content through the canonical Markdown round trip
    // (same loss profile as Plan 5 export).
    let update_bytes = match markdown_round_trip(&snap.state_bytes) {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!(error = %e, %doc_id, seq, "restore round trip");
            return internal();
        }
    };

    // The restorer rewrote the page, so they are its latest contributor —
    // not the authors of the snapshot's content. The version restored to
    // is named by its snapshot seq and the time it was taken (a page's
    // history shows "Restored by X, to <date>").
    let meta = serde_json::json!({
        "target_snapshot_seq": seq,
        "target_ended_at": snap.created_at.to_rfc3339(),
    });
    // No bound on the room: 0.9.14 drawers still call this during the
    // rollout and would read a 504 as a failure to retry (Rulings).
    send_replace(&state, doc_id, ctx.user_id, update_bytes, meta, None).await
}

/// `state_bytes` (a full Y state) as the update that replaces a page with
/// it through Markdown: applied to a fresh doc, serialized and parsed
/// back. A restore has the loss profile of a Markdown export and import.
fn markdown_round_trip(state_bytes: &[u8]) -> Result<Vec<u8>, String> {
    let engine = YrsEngine;
    let doc = engine.new_doc();
    engine
        .apply_update(&doc, state_bytes)
        .map_err(|e| format!("apply_update: {e:?}"))?;
    let markdown = knot_markdown::to_markdown::serialise(&engine, &doc)
        .map_err(|e| format!("serialise: {e:?}"))?;
    let (_doc, update_bytes) =
        knot_markdown::from_markdown::parse(&markdown).map_err(|e| format!("parse: {e:?}"))?;
    Ok(update_bytes)
}

/// `POST /api/docs/{id}/history/entries/{eid}/restore?ended_at=`: reverts
/// the page in place to the version right after entry `eid` (M2's Restore
/// here): a verified CRDT revert that changes only what differs, planned
/// against the live room's export and applied only to the state it was
/// planned on (`crate::history_restore`). Answers 200
/// `{"outcome": "restored" | "fallback" | "unchanged"}`. Editors and owners
/// only; an archived page is refused by the middleware. When `ended_at`
/// (the version the reader saw) is given and the entry has moved on since,
/// nothing is sent: 409 `history.moved`. The query is read as pairs, so a
/// bad or repeated `ended_at` is a JSON 400.
pub async fn restore_entry(
    State(state): State<AppState>,
    Path((doc_id, eid)): Path<(Uuid, String)>,
    Query(params): Query<Vec<(String, String)>>,
    req: Request,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    // Present: require_editor rejected the request otherwise.
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Ok(entry_id) = eid.parse::<i64>() else {
        return bad_request("entry id");
    };
    let seen = match single(&params, "ended_at") {
        Ok(None) => None,
        Ok(Some(v)) => match DateTime::parse_from_rfc3339(v) {
            Ok(at) => Some(at.with_timezone(&Utc)),
            Err(_) => return bad_request("ended_at"),
        },
        Err(()) => return bad_request("ended_at"),
    };
    let Some(views) = state.history_views.clone() else {
        return internal();
    };
    let deadline = Instant::now() + RESTORE_DEADLINE;
    // Every answer from here on is logged within the restore's span.
    let span = tracing::info_span!("restore", %doc_id, entry_id);
    let answer = |r| span.in_scope(|| restore_answer(r));

    // The principal: the person, the bot, or the user an API key acts for.
    // One restore at a time per principal on this pod, held until the
    // answer (the restore-permit Ruling); not counted when refused.
    let _restoring = match views.restore_permit(ctx.user_id, deadline).await {
        Ok(permit) => permit,
        Err(e) => return view_error(&e),
    };

    // `seen` is compared as an instant (the reader may write the offset
    // either way), before the replay and again on the entry it loaded.
    let restorable = match views
        .restore_target(ctx.user_id, doc_id, entry_id, seen, deadline)
        .await
    {
        Ok(r) => r,
        Err(PlanError::TooSlow | PlanError::OverCap) => {
            return answer(Err(RestoreFailure::TooLarge));
        }
        Err(PlanError::View(e)) => {
            if matches!(e, ViewError::Failed(_) | ViewError::Store(_)) {
                tracing::error!(error = %e, %doc_id, entry_id, "history restore");
            }
            return view_error(&e);
        }
        Err(PlanError::Unverified(reason)) => {
            tracing::warn!(%doc_id, entry_id, reason, "history restore target");
            return answer(Err(RestoreFailure::Unverified));
        }
    };

    let meta = serde_json::json!({
        "target_entry_id": entry_id,
        "target_rev": restorable.end_rev,
        "target_ended_at": restorable.ended_at.to_rfc3339(),
    });
    // A plan with too little time left would be refused anyway; the room
    // is not woken for it (the cancellation Ruling).
    if deadline.saturating_duration_since(Instant::now()) < REVERT_MIN_PLAN_TIME {
        return answer(Err(RestoreFailure::Busy));
    }
    let Some(rooms) = state.rooms_v2.clone() else {
        return answer(Err(RestoreFailure::Failed("no room registry".into())));
    };
    let acquire = async move {
        rooms
            .acquire(doc_id)
            .await
            .map(|h| {
                let tx = h.tx.clone();
                (h, tx)
            })
            .map_err(|e| format!("{e:?}"))
    };
    let wait = RESTORE_ROOM_WAIT.min(deadline.saturating_duration_since(Instant::now()));
    let room = match LiveRoom::acquire_within(acquire, wait).await {
        Ok(room) => room,
        Err(failure) => return answer(Err(failure)),
    };

    let target = Arc::new(restorable.value);
    let ticket = &restorable.ticket;
    let views = &views;
    let plan = |live: Vec<u8>| {
        let target = target.clone();
        async move {
            views
                .plan_revert(ctx.user_id, ticket, target, live, deadline)
                .await
        }
    };
    let result = revert_attempts(
        &room,
        plan,
        deadline,
        restorable.content_epoch,
        doc_id,
        ctx.user_id,
        meta,
    )
    .instrument(span.clone())
    .await;
    answer(result)
}

/// The body of a Restore as new page.
#[derive(Deserialize)]
struct RestoreAsNewRequest {
    /// How the reader names the version, in the copy's title.
    version_label: String,
    /// The version the reader saw (RFC 3339); absent, the entry is copied
    /// as it is now (the `ended_at` Ruling).
    #[serde(default)]
    ended_at: Option<String>,
}

/// `version_label` trimmed, when it is a label: not empty, at most
/// [`VERSION_LABEL_MAX_CHARS`] characters, no control character.
fn version_label(label: &str) -> Option<&str> {
    let label = label.trim();
    let fits = !label.is_empty() && label.chars().count() <= VERSION_LABEL_MAX_CHARS;
    (fits && !label.chars().any(char::is_control)).then_some(label)
}

/// `POST /api/docs/{id}/history/entries/{eid}/restore-as-new` with
/// `{"version_label", "ended_at"?}`: copies the version right after entry
/// `eid` into a new page right after this one, titled "<title> (version of
/// <label>)", through Duplicate (`crate::duplicate`, the restore-as-new
/// Ruling): its checks and page quota first, then the version's replay
/// (under the caller's restore permit, held until the answer), then a
/// duplicate slot within what is left of [`RESTORE_DEADLINE`]. Answers 201
/// with the new page, as Duplicate does. Editors and owners only; an
/// archived page is refused (Duplicate's check).
#[tracing::instrument(skip_all, name = "history.restore_as_new")]
pub async fn restore_as_new(
    State(state): State<AppState>,
    Path((doc_id, eid)): Path<(Uuid, String)>,
    req: Request,
) -> Response {
    use crate::routes::api::docs::{doc_answer, duplicate_busy, duplicate_error, read_json};

    if let Some(r) = require_editor(&req) {
        return r;
    }
    // Present: require_editor rejected the request otherwise.
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    let Ok(entry_id) = eid.parse::<i64>() else {
        return bad_request("entry id");
    };
    let Ok(body) = read_json::<RestoreAsNewRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(label) = version_label(&body.version_label) else {
        return bad_request("version_label");
    };
    let seen = match body.ended_at.as_deref().map(DateTime::parse_from_rfc3339) {
        None => None,
        Some(Ok(at)) => Some(at.with_timezone(&Utc)),
        Some(Err(_)) => return bad_request("ended_at"),
    };
    // Duplicate's answers before the replay: none of them needs it.
    if let Err(e) = crate::duplicate::check(&state, &ctx, role.0, doc_id).await {
        return duplicate_error(e);
    }
    if let Err(response) = crate::quota::enforce_pages(&state, &ctx, 1).await {
        return response;
    }
    let Some(views) = state.history_views.clone() else {
        return internal();
    };
    let deadline = Instant::now() + RESTORE_DEADLINE;
    // One restore at a time per principal on this pod, in place or as a
    // new page, held until the answer (the restore-permit Ruling).
    let _restoring = match views.restore_permit(ctx.user_id, deadline).await {
        Ok(permit) => permit,
        Err(e) => return view_error(&e),
    };
    let version = match views
        .restore_input_by(
            ctx.user_id,
            doc_id,
            entry_id,
            seen,
            deadline,
            knot_history::revert::copy_state,
        )
        .await
    {
        Ok(version) => version,
        Err(e) => {
            if matches!(e, ViewError::Failed(_) | ViewError::Store(_)) {
                tracing::error!(error = %e, %doc_id, entry_id, "history restore as new");
            }
            return view_error(&e);
        }
    };
    // After the replay, so no duplicate slot is held through a wait for a
    // compute permit; within what is left of the deadline, so the copy
    // starts before the router's timeout.
    let wait = state
        .duplicate_busy_wait
        .min(deadline.saturating_duration_since(Instant::now()));
    if wait.is_zero() {
        return duplicate_busy();
    }
    let permit =
        match tokio::time::timeout(wait, state.duplicate_permits.clone().acquire_owned()).await {
            Ok(Ok(permit)) => permit,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "duplicate permits closed");
                return internal();
            }
            Err(_) => return duplicate_busy(),
        };
    let copy = VersionCopy {
        state: version.value,
        entry_id,
        at: version.ended_at,
        content_epoch: version.content_epoch,
        label: label.to_string(),
    };
    // On a task of its own, as Duplicate's: dropping this handler can't
    // stop the copy halfway.
    let answer = crate::duplicate::start(
        state.clone(),
        ctx.clone(),
        role.0,
        doc_id,
        false,
        permit,
        Some(copy),
    );
    match answer.await {
        Ok(Ok(top)) => {
            crate::routes::api::docs::emit_copy_created(&state, ctx.workspace_id, top.id).await;
            doc_answer(&state, &ctx, StatusCode::CREATED, &top).await
        }
        Ok(Err(e)) => duplicate_error(e),
        Err(_) => {
            tracing::error!("restore as new task ended without an answer");
            internal()
        }
    }
}

/// Replaces `doc_id`'s content with `update_bytes` through its room, as a
/// `restore` row by `by` carrying `meta`: 204 once the room stored it.
/// With `room_wait`, the room's acquire, the send and the reply together
/// are bounded by it (504 `restore.timeout`; the replace may still apply);
/// without, they are not.
async fn send_replace(
    state: &AppState,
    doc_id: Uuid,
    by: Uuid,
    update_bytes: Vec<u8>,
    meta: Value,
    room_wait: Option<Duration>,
) -> Response {
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };
    let acquire = async move {
        rooms
            .acquire(doc_id)
            .await
            .map(|h| {
                let tx = h.tx.clone();
                (h, tx)
            })
            .map_err(|e| format!("{e:?}"))
    };
    replace_within(room_wait, acquire, update_bytes, by, meta)
        .instrument(tracing::info_span!("restore", %doc_id))
        .await
}

/// The bounded part of [`send_replace`], over any room: `acquire` yields a
/// handle `H`, kept alive until the reply, and the room's event sender.
/// 500 `internal` when the room cannot be acquired, its channel is closed
/// or the reply is dropped; 500 `restore.apply` when the room refuses the
/// replace; 504 `restore.timeout` when `wait` runs out first.
async fn replace_within<H: Send>(
    wait: Option<Duration>,
    acquire: impl Future<Output = Result<(H, mpsc::Sender<knot_crdt::Event>), String>>,
    update_bytes: Vec<u8>,
    by: Uuid,
    meta: Value,
) -> Response {
    let replace = async move {
        let (room, tx) = match acquire.await {
            Ok(room) => room,
            Err(e) => {
                tracing::error!(error = %e, "room acquire failed");
                return internal();
            }
        };
        let (reply, replied) = oneshot::channel();
        let event = knot_crdt::Event::ReplaceWithMarkdown {
            update_bytes,
            by_user: Some(by),
            source: knot_storage::UpdateSource::Restore,
            meta: Some(meta),
            reply,
        };
        if tx.send(event).await.is_err() {
            return internal();
        }
        let answer = replied.await;
        drop(room);
        match answer {
            Ok(Ok(_rev)) => StatusCode::NO_CONTENT.into_response(),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "restore replace");
                json_err(StatusCode::INTERNAL_SERVER_ERROR, "restore.apply", "")
            }
            Err(_) => internal(),
        }
    };
    let Some(wait) = wait else {
        return replace.await;
    };
    match tokio::time::timeout(wait, replace).await {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(
                wait_ms = wait.as_millis() as u64,
                "restore did not finish in time"
            );
            json_err(
                StatusCode::GATEWAY_TIMEOUT,
                "restore.timeout",
                "the restore did not finish in time; it may still apply",
            )
        }
    }
}

/// `?cursor=&limit=` of the entries list, parsed in the handler so a bad
/// value is a JSON 400 (axum's `Query` answers a non-number in plain text).
#[derive(Debug, Deserialize)]
pub struct EntriesQuery {
    cursor: Option<String>,
    limit: Option<String>,
}

/// `GET /api/docs/{id}/history/entries?cursor=&limit=`: one page of the
/// page's history entries, newest first (`knot_history::list`). Editors
/// and owners only, also on an archived page; never cached.
pub async fn entries(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(query): Query<EntriesQuery>,
    req: Request,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    // Present: require_editor rejected the request otherwise.
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // An empty value (`?cursor=&limit=`) is no value.
    let given = |v: Option<String>| v.filter(|s| !s.is_empty());
    let cursor = match given(query.cursor).map(|c| Cursor::decode(&c)).transpose() {
        Ok(c) => c,
        Err(_) => return bad_request("cursor"),
    };
    let limit = match given(query.limit) {
        None => DEFAULT_LIMIT,
        Some(l) => match l.parse::<i64>() {
            Ok(n) if (1..=MAX_LIMIT).contains(&n) => n,
            _ => return bad_request("limit"),
        },
    };
    let Some(pool) = state.pool.as_ref() else {
        return internal();
    };

    let page = match knot_history::list_entries(pool, doc_id, cursor, limit).await {
        Ok(Some(page)) => page,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "history entries");
            return internal();
        }
    };
    let people = match knot_history::load_people(
        pool,
        &knot_history::referenced_people(&page.entries),
    )
    .await
    {
        Ok(people) => people,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "history entries people");
            return internal();
        }
    };
    let readable = readable_sources(
        &knot_history::referenced_docs(&page.entries),
        state
            .acl
            .as_ref()
            .map(|acl| move |id| acl.access(ctx.workspace_id, id, ctx.user_id, ctx.root_doc_id)),
    )
    .await;

    // The current titles of readable sources, for moved pages' entries.
    // Fails open to no titles: the entry then reads "another page".
    let readable_ids: Vec<Uuid> = readable.iter().copied().collect();
    let titles = match knot_history::load_titles(pool, &readable_ids).await {
        Ok(titles) => titles,
        Err(e) => {
            tracing::warn!(error=?e, %doc_id, "history entries: source titles");
            Default::default()
        }
    };

    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(knot_history::render_page(
            &page, &people, &readable, &titles,
        )),
    )
        .into_response()
}

/// The one value of `key` among `params` (`None` when absent); `Err` when
/// it is given more than once. Other keys are ignored.
pub(crate) fn single<'a>(params: &'a [(String, String)], key: &str) -> Result<Option<&'a str>, ()> {
    let mut values = params
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str());
    let first = values.next();
    match values.next() {
        Some(_) => Err(()),
        None => Ok(first),
    }
}

/// `GET /api/docs/{id}/history/entries/{eid}/view?changes=0|1`: the page
/// as it was right after entry `eid`, as ProseMirror JSON, with the
/// entry's own changes marked when `changes` is `1` (also when absent or
/// empty) and the render can show them. Editors and owners only, also on
/// an archived page (where `can_restore` is false); never cached by the
/// client. The query is read as pairs, so a bad or repeated `changes` is
/// a JSON 400 rather than axum's plain-text one.
pub async fn view_entry(
    State(state): State<AppState>,
    Path((doc_id, eid)): Path<(Uuid, String)>,
    Query(params): Query<Vec<(String, String)>>,
    req: Request,
) -> Response {
    if let Some(r) = require_editor(&req) {
        return r;
    }
    // Present: require_editor rejected the request otherwise.
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Ok(entry_id) = eid.parse::<i64>() else {
        return bad_request("entry id");
    };
    let changes = match single(&params, "changes") {
        Ok(None | Some("" | "1")) => true,
        Ok(Some("0")) => false,
        _ => return bad_request("changes"),
    };
    let can_restore = req.extensions().get::<DocArchived>().is_none();
    let Some(views) = state.history_views.clone() else {
        return internal();
    };

    // The principal: the person, the bot, or the user an API key acts for.
    let view = match views.view(ctx.user_id, doc_id, entry_id, changes).await {
        Ok(view) => view,
        Err(e) => {
            if matches!(e, ViewError::Failed(_) | ViewError::Store(_)) {
                tracing::error!(error = %e, %doc_id, entry_id, "history view");
            }
            return view_error(&e);
        }
    };

    // The cached doc is sent as it is, inside the per-request envelope.
    let prefix = format!(
        "{{\"entry_id\":{},\"kind\":{},\"changes\":{},\"can_restore\":{},\"can_erase\":false,\"doc\":",
        view.entry_id,
        serde_json::Value::from(view.kind.as_str()),
        view.changes,
        can_restore,
    );
    let parts = [
        Bytes::from(prefix),
        view.doc.clone(),
        Bytes::from_static(b"}"),
    ];
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(Body::from_stream(futures::stream::iter(
            parts.map(Ok::<_, Infallible>),
        )))
        .unwrap_or_else(|_| internal())
}

/// The response for a version that was not served.
pub(crate) fn view_error(e: &ViewError) -> Response {
    match e {
        ViewError::NotFound => json_err(
            StatusCode::NOT_FOUND,
            "history.not_found",
            "no such history entry",
        ),
        ViewError::Rebuilding => json_err(
            StatusCode::CONFLICT,
            "history.rebuilding",
            "the page's history is being rebuilt",
        ),
        ViewError::TooLarge => json_err(
            StatusCode::CONFLICT,
            "history.too_large",
            "the version is too large to show",
        ),
        ViewError::Busy => {
            let mut r = json_err(
                StatusCode::SERVICE_UNAVAILABLE,
                "history.busy",
                "history is busy",
            );
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("5"));
            r
        }
        ViewError::Failed(_) | ViewError::TimedOut => json_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "history.view_failed",
            "the version could not be shown",
        ),
        ViewError::Moved => json_err(
            StatusCode::CONFLICT,
            "history.moved",
            "the entry changed since it was read",
        ),
        ViewError::Store(_) => internal(),
    }
}

/// The pages among `ids` (a copy's or a template's source) the reader may
/// read, as `access` resolves the reader's access to each. Fails closed
/// (Rulings): without an ACL cache (`access` is `None`), with no access
/// (`Ok(None)`), with access that cannot read (a viewer on an archived
/// page) or when resolving errs, a page is not readable, and its entry
/// shows `*_hidden: true` instead of its id and title; the list never
/// fails for it.
pub(crate) async fn readable_sources<F, Fut>(ids: &[Uuid], access: Option<F>) -> HashSet<Uuid>
where
    F: Fn(Uuid) -> Fut,
    Fut: Future<Output = Result<Option<DocAccess>, ResolveError>>,
{
    let mut readable = HashSet::new();
    if ids.is_empty() {
        return readable;
    }
    let Some(access) = access else {
        tracing::warn!(
            sources = ids.len(),
            "history entries: no ACL cache, sources hidden"
        );
        return readable;
    };
    for &id in ids {
        match access(id).await {
            Ok(Some(a)) if a.can_read() => {
                readable.insert(id);
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, source_doc_id = %id, "history entries: source access, hidden");
            }
        }
    }
    readable
}

fn bad_request(what: &str) -> Response {
    json_err(
        StatusCode::BAD_REQUEST,
        "history.bad_request",
        &format!("bad {what}"),
    )
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use knot_crdt::Event;
    use knot_storage::{DocStoreError, WorkspaceRole};
    use std::time::Instant;

    /// What a test room does with the replace it receives.
    enum Room {
        /// Keeps the reply without answering.
        Hold,
        /// Answers `Err`.
        Refuse,
        /// Drops the reply.
        Drop,
        /// Answers `Ok` after the delay.
        Apply(Duration),
    }

    /// A room's event sender whose receiver task does what `room` says.
    fn room(room: Room) -> mpsc::Sender<Event> {
        let (tx, mut rx) = mpsc::channel(1);
        tokio::spawn(async move {
            let Some(Event::ReplaceWithMarkdown { reply, .. }) = rx.recv().await else {
                panic!("not a replace");
            };
            match room {
                Room::Hold => {
                    let _held = reply;
                    std::future::pending::<()>().await;
                }
                Room::Refuse => {
                    let _ = reply.send(Err("refused".to_string()));
                }
                Room::Drop => drop(reply),
                Room::Apply(after) => {
                    tokio::time::sleep(after).await;
                    let _ = reply.send(Ok(1));
                }
            }
        });
        tx
    }

    const WAIT: Option<Duration> = Some(Duration::from_millis(200));

    /// [`replace_within`] over `tx` (acquired at once) within `wait`.
    async fn replace(wait: Option<Duration>, tx: mpsc::Sender<Event>) -> Response {
        replace_within(
            wait,
            async move { Ok(((), tx)) },
            vec![0, 0],
            Uuid::nil(),
            serde_json::json!({}),
        )
        .await
    }

    /// The status and error code of `r`.
    async fn status_and_code(r: Response) -> (StatusCode, Option<String>) {
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let code = serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|v| v["error"]["code"].as_str().map(str::to_string));
        (status, code)
    }

    fn error(status: StatusCode, code: &str) -> (StatusCode, Option<String>) {
        (status, Some(code.to_string()))
    }

    #[tokio::test]
    async fn a_room_that_does_not_answer_is_a_504() {
        let started = Instant::now();
        let r = replace(WAIT, room(Room::Hold)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            status_and_code(r).await,
            error(StatusCode::GATEWAY_TIMEOUT, "restore.timeout")
        );

        // The acquire is inside the wait.
        let started = Instant::now();
        let r = replace_within::<()>(
            WAIT,
            std::future::pending(),
            vec![0, 0],
            Uuid::nil(),
            serde_json::json!({}),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            status_and_code(r).await,
            error(StatusCode::GATEWAY_TIMEOUT, "restore.timeout")
        );
    }

    #[tokio::test]
    async fn a_room_that_answers_with_an_error_is_restore_apply() {
        let r = replace(WAIT, room(Room::Refuse)).await;
        assert_eq!(
            status_and_code(r).await,
            error(StatusCode::INTERNAL_SERVER_ERROR, "restore.apply")
        );
    }

    #[tokio::test]
    async fn a_room_that_goes_away_is_internal() {
        let internal = error(StatusCode::INTERNAL_SERVER_ERROR, "internal");

        let (gone, rx) = mpsc::channel(1);
        drop(rx);
        assert_eq!(status_and_code(replace(WAIT, gone).await).await, internal);

        let r = replace(WAIT, room(Room::Drop)).await;
        assert_eq!(status_and_code(r).await, internal);

        let r = replace_within::<()>(
            WAIT,
            async { Err("no room".to_string()) },
            vec![0, 0],
            Uuid::nil(),
            serde_json::json!({}),
        )
        .await;
        assert_eq!(status_and_code(r).await, internal);

        let r = replace(WAIT, room(Room::Apply(Duration::ZERO))).await;
        assert_eq!(status_and_code(r).await, (StatusCode::NO_CONTENT, None));
    }

    /// The seq restore keeps its unbounded wait for 0.9.14 drawers.
    #[tokio::test]
    async fn a_seq_restore_waits_for_a_slow_room() {
        let r = replace(None, room(Room::Apply(Duration::from_millis(300)))).await;
        assert_eq!(status_and_code(r).await, (StatusCode::NO_CONTENT, None));
    }

    #[tokio::test]
    async fn readable_sources_fails_closed() {
        let [a, b, c, d] = [1_u128, 2, 3, 4].map(Uuid::from_u128);
        let access = |id: Uuid| async move {
            let access = |role, archived| DocAccess {
                role,
                archived,
                restriction_root: None,
            };
            match id {
                id if id == a => Ok(Some(access(WorkspaceRole::Editor, false))),
                id if id == b => Ok(Some(access(WorkspaceRole::Viewer, true))),
                id if id == c => Ok(None),
                _ => Err(ResolveError::Docs(DocStoreError::NotFound)),
            }
        };
        let ids = [a, b, c, d];
        assert_eq!(
            readable_sources(&ids, Some(access)).await,
            HashSet::from([a])
        );
        assert_eq!(
            readable_sources(&ids, None::<fn(Uuid) -> std::future::Ready<_>>).await,
            HashSet::new()
        );
    }
}
