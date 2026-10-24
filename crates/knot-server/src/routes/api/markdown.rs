//! GET  /api/docs/{id}/markdown    → text/markdown export
//! POST /api/docs/{id}/markdown    → cold-import markdown as a y-update
//!
//! The room actor is the exclusive owner of the live `DocHandle`, so we only
//! ask it for an encoded state snapshot (`Event::ExportState`) and perform
//! the (potentially expensive) markdown serialization here in the handler
//! against a transient doc. This keeps the actor responsive for editor
//! traffic and avoids polluting the `Engine` trait with a markdown concern.
//!
//! For import we parse the markdown to a y-update in the handler (pure
//! transform) and hand the bytes to the room, which applies + persists +
//! fans out to local connections.
//!
//! Import takes `?mode=append` (the default) or `?mode=replace`. Append
//! hands the parsed bytes to `Event::ApplyUpdate`, which Yjs *merges* into
//! the live fragment — right for `from_template`, which always targets a
//! fresh doc, wrong for importing over a page that already has content
//! (you get both, concatenated). Replace goes through
//! `Event::ReplaceWithMarkdown`, which clears the fragment and applies in a
//! single transaction. The default stays `append` so existing callers are
//! unaffected; the doc page's import control asks for `replace`.

use axum::{
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_crdt::{Engine, YrsEngine};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

/// Errors `refresh_markdown_and_index` can return. Distinguishing the
/// kinds lets callers decide whether to surface them: the export
/// endpoint maps every variant to 500, while the patch endpoint logs
/// and ignores because the index refresh is bookkeeping rather than the
/// critical path.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("room actor unreachable for {0}")]
    RoomUnreachable(Uuid),
    #[error("room actor returned an error: {0}")]
    Actor(String),
    #[error("yrs apply: {0}")]
    Apply(String),
    #[error("markdown serialise: {0}")]
    Serialise(String),
    #[error("reading stored state: {0}")]
    Storage(String),
}

/// Why [`current_snapshot`] could not copy a page's state, and why
/// [`live_snapshot`] could not once it had the room.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SnapshotError {
    #[error("room actor unreachable for {0}")]
    RoomUnreachable(Uuid),
    #[error("room actor: {0}")]
    Actor(knot_crdt::EngineError),
    #[error(transparent)]
    Apply(knot_crdt::EngineError),
    #[error("reading stored state: {0}")]
    Storage(String),
}

/// Why [`live_snapshot`] could not copy a page's state: it opens the room
/// first.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LiveSnapshotError {
    #[error("server state missing rooms registry")]
    NoRooms,
    #[error("room acquire failed for {doc_id}: {source}")]
    Acquire {
        doc_id: Uuid,
        source: knot_crdt::AcquireError,
    },
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Keeps `RefreshError`'s variants and messages as they were before the
/// snapshot step was shared (Debug-formatted engine errors).
impl From<SnapshotError> for RefreshError {
    fn from(e: SnapshotError) -> Self {
        match e {
            SnapshotError::RoomUnreachable(doc_id) => Self::RoomUnreachable(doc_id),
            SnapshotError::Actor(e) => Self::Actor(format!("{e:?}")),
            SnapshotError::Apply(e) => Self::Apply(format!("{e:?}")),
            SnapshotError::Storage(e) => Self::Storage(e),
        }
    }
}

/// Copy the page's live state into a transient doc, with the room's seq at
/// the time of the copy. The room actor only encodes its state
/// (`Event::ExportState`), so it keeps serving editors while the caller
/// reads the copy. Used by the DOCX/ODT export, which opens the room.
pub(crate) async fn live_snapshot(
    state: &AppState,
    doc_id: Uuid,
) -> Result<(knot_crdt::DocHandle, i64), LiveSnapshotError> {
    let rooms = state.rooms_v2.clone().ok_or(LiveSnapshotError::NoRooms)?;
    let room = rooms
        .acquire(doc_id)
        .await
        .map_err(|source| LiveSnapshotError::Acquire { doc_id, source })?;
    Ok(room_snapshot(&room, doc_id).await?)
}

/// The page's current state in a transient doc, with its seq, without
/// booting a room: rooms are never evicted when idle, so a reader that
/// booted them would keep every page it read in memory (P18).
///
/// - If a room is running here, its state, as [`live_snapshot`] copies it.
/// - Otherwise the stored state, loaded the way a room loads it (latest
///   snapshot, then the updates after it), with the latest seq applied.
///
/// Used by the markdown refresh, and so by its background sweep.
pub(crate) async fn current_snapshot(
    state: &AppState,
    doc_id: Uuid,
) -> Result<(knot_crdt::DocHandle, i64), SnapshotError> {
    if let Some(room) = state.rooms_v2.as_ref().and_then(|r| r.get_active(doc_id)) {
        return room_snapshot(&room, doc_id).await;
    }
    let pool = state
        .pool
        .as_ref()
        .ok_or_else(|| SnapshotError::Storage("database pool not configured".into()))?;
    stored_snapshot(pool, doc_id).await
}

/// The page's stored state, loaded the way a room loads it (latest
/// snapshot, then the updates after it), with the latest seq applied. Read
/// errors fail the snapshot instead of leaving content out, so a refresh
/// never caches a page with part of it missing. Also the offline export
/// check's load (`export_check::hydrate`).
pub(crate) async fn stored_snapshot(
    pool: &knot_storage::Pool,
    doc_id: Uuid,
) -> Result<(knot_crdt::DocHandle, i64), SnapshotError> {
    use knot_storage::{SnapshotStore, UpdatesStore};

    let engine = YrsEngine;
    let doc = engine.new_doc();
    let mut seq = 0;
    let latest = knot_storage::PgSnapshotStore::new(pool.clone())
        .latest(doc_id)
        .await
        .map_err(|e| SnapshotError::Storage(e.to_string()))?;
    if let Some(snap) = latest {
        engine
            .apply_update(&doc, &snap.state_bytes)
            .map_err(SnapshotError::Apply)?;
        seq = snap.snapshot_seq;
    }
    let updates = knot_storage::PgUpdatesStore::new(pool.clone())
        .since(doc_id, seq)
        .await
        .map_err(|e| SnapshotError::Storage(e.to_string()))?;
    for update in updates {
        engine
            .apply_update(&doc, &update.update_bytes)
            .map_err(SnapshotError::Apply)?;
        seq = seq.max(update.seq);
    }
    Ok((doc, seq))
}

/// Copy a running room's state into a transient doc, with its seq.
async fn room_snapshot(
    room: &knot_crdt::RoomHandle,
    doc_id: Uuid,
) -> Result<(knot_crdt::DocHandle, i64), SnapshotError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    if room
        .tx
        .send(knot_crdt::Event::ExportState(tx))
        .await
        .is_err()
    {
        return Err(SnapshotError::RoomUnreachable(doc_id));
    }
    let (state_bytes, seq) = match rx.await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(SnapshotError::Actor(e)),
        Err(_) => return Err(SnapshotError::RoomUnreachable(doc_id)),
    };
    let engine = YrsEngine;
    let transient = engine.new_doc();
    engine
        .apply_update(&transient, &state_bytes)
        .map_err(SnapshotError::Apply)?;
    Ok((transient, seq))
}

/// Re-render markdown from the doc's current state ([`current_snapshot`]),
/// write to the cache, and re-run the task indexer. Used after any mutation
/// that should be reflected on `/tasks` (markdown export, full-doc import
/// via ApplyUpdate/ReplaceWithMarkdown, individual task patch).
///
/// Best-effort: cache-put + indexer failures are logged but never
/// propagated. The Result reports only the steps before the cache write
/// (state export + markdown serialise), because failures there mean
/// callers got no usable text to return. A failed task write marks the
/// cache row stale, so the refresh sweep tries the page again.
pub async fn refresh_markdown_and_index(
    state: &AppState,
    doc_id: Uuid,
) -> Result<String, RefreshError> {
    refresh_markdown_inner(state, doc_id, TaskIndex::Edit).await
}

/// Export the doc to markdown WITHOUT re-running the task indexer.
/// Used by the from-template flow so cloning a template doesn't
/// trigger a write to the template's own task rows.
///
/// The cache row it writes is marked stale: the template's task rows may
/// be an older renderer's, and only a refresh that writes them may mark the
/// page current. The sweep then refreshes the template in full.
pub async fn export_markdown_only(state: &AppState, doc_id: Uuid) -> Result<String, RefreshError> {
    refresh_markdown_inner(state, doc_id, TaskIndex::Skip).await
}

/// [`refresh_markdown_and_index`] for a page nobody edited: the refresh
/// sweep's. It writes only the task rows whose values change, so a healthy
/// page's tasks keep their `updated_at` and `/tasks` doesn't reshuffle.
pub(crate) async fn refresh_markdown_quietly(
    state: &AppState,
    doc_id: Uuid,
) -> Result<String, RefreshError> {
    refresh_markdown_inner(state, doc_id, TaskIndex::Quiet).await
}

/// What a refresh does with the page's task rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskIndex {
    /// Leaves them as they are, and so marks the cache row stale.
    Skip,
    /// Rewrites them all, as after an edit (`upsert_for_doc`).
    Edit,
    /// Writes only the rows whose values change (`sync_for_doc_quiet`).
    Quiet,
}

async fn refresh_markdown_inner(
    state: &AppState,
    doc_id: Uuid,
    task_index: TaskIndex,
) -> Result<String, RefreshError> {
    let (transient, seq) = current_snapshot(state, doc_id).await?;
    let text = match knot_markdown::serialise_with_report(&YrsEngine, &transient) {
        Ok((md, unknown)) => {
            record_unknown_nodes(doc_id, &unknown);
            md
        }
        Err(e) => return Err(RefreshError::Serialise(format!("{e:?}"))),
    };
    // Only a refresh that writes the task rows marks the page current; one
    // that leaves them (from-template) marks it stale, as the rows may be
    // an older renderer's.
    let version = if task_index == TaskIndex::Skip {
        knot_storage::STALE_RENDERER_VERSION
    } else {
        knot_markdown::RENDERER_VERSION
    };
    // A render of an older seq than the cached one writes nothing, and its
    // tasks would be older too, so the reindex is skipped. When the write
    // fails, whether it is older is unknown: reindex, as before the guard.
    let newest = match state.markdown_cache.clone() {
        Some(cache) => cache
            .put_rendered(doc_id, seq, &text, version)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error=?e, "md cache put failed");
                true
            }),
        None => true,
    };
    if !newest || task_index == TaskIndex::Skip {
        return Ok(text);
    }
    let (Some(tasks), Some(docs)) = (state.tasks.as_deref(), state.docs.as_deref()) else {
        return Ok(text);
    };
    // When the task rows weren't written, the row says the current renderer
    // wrote the page while its task rows are older: mark it stale again, so
    // the sweep tries the page again.
    if !index_tasks(tasks, docs, doc_id, &transient, task_index).await
        && let Some(cache) = state.markdown_cache.as_ref()
        && let Err(e) = cache.mark_stale(doc_id).await
    {
        tracing::warn!(error=?e, %doc_id, "md cache mark stale failed");
    }
    Ok(text)
}

/// Write the page's task rows, read from `page`, as `task_index` says.
/// Returns whether they were written; failures are logged.
async fn index_tasks(
    tasks: &dyn knot_storage::TaskStore,
    docs: &dyn knot_storage::DocStore,
    doc_id: Uuid,
    page: &knot_crdt::DocHandle,
    task_index: TaskIndex,
) -> bool {
    // From the page itself, so that listing index k is the item the
    // `/tasks` toggle with index k flips (spec A11, A12).
    let extracted = knot_markdown::tasks::extract_tasks_from_doc(page);
    let inputs: Vec<knot_storage::DocTaskInput> = extracted
        .into_iter()
        .map(|t| knot_storage::DocTaskInput {
            item_index: t.item_index,
            text: t.text,
            assignee_user_id: t.assignee_user_id,
            checked: t.checked,
            due_at: t.due_at,
        })
        .collect();
    let workspace_id = match docs.get(doc_id).await {
        Ok(Some(doc)) => doc.workspace_id,
        Ok(None) => {
            tracing::warn!(%doc_id, "task reindex: doc not found");
            return false;
        }
        Err(e) => {
            tracing::warn!(error=?e, %doc_id, "task reindex: doc lookup failed");
            return false;
        }
    };
    let written = if task_index == TaskIndex::Quiet {
        tasks
            .sync_for_doc_quiet(workspace_id, doc_id, &inputs)
            .await
    } else {
        tasks.upsert_for_doc(workspace_id, doc_id, &inputs).await
    };
    match written {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(error=?e, %doc_id, "task reindex failed");
            false
        }
    }
}

/// Warns about and counts the nodes a page's export wrote as their text
/// because the schema doesn't name them (`unknown`, their tags, from
/// `serialise_with_report`). The tags are page content, so they go to the
/// log, not into metric labels.
fn record_unknown_nodes(doc_id: Uuid, unknown: &[String]) {
    if unknown.is_empty() {
        return;
    }
    metrics::counter!("knot_markdown_unknown_nodes_total").increment(unknown.len() as u64);
    let mut tags = unknown.to_vec();
    tags.sort();
    tags.dedup();
    tracing::warn!(
        %doc_id,
        count = unknown.len(),
        ?tags,
        "markdown export wrote unknown nodes as text"
    );
}

pub(super) async fn export_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    }
    let text = match refresh_markdown_and_index(&state, doc_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "md export refresh");
            return internal();
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/markdown; charset=utf-8")
        .body(Body::from(text))
        .unwrap()
}

/// Query for [`import_inline`].
///
/// Deliberately typed as `Option<String>` rather than a serde enum: an enum
/// would make an unknown value fail deserialization, and axum would answer
/// with its own plain-text 400 instead of our JSON error envelope.
#[derive(serde::Deserialize)]
pub(super) struct ImportQuery {
    #[serde(default)]
    mode: Option<String>,
}

pub(super) async fn import_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(q): Query<ImportQuery>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 == knot_storage::WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    // `append` is the default because it is what shipped: `from_template`
    // and any existing API client rely on the merge. Only the doc page's
    // import control asks for `replace`.
    let replace = match q.mode.as_deref() {
        None | Some("append") => false,
        Some("replace") => true,
        Some(_) => return json_err(StatusCode::BAD_REQUEST, "markdown.bad_mode", ""),
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };

    let body = match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let text = match std::str::from_utf8(&body) {
        Ok(s) => s.to_string(),
        Err(_) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.not_utf8", ""),
    };

    // Parse markdown to a y-update via knot_markdown. The parse function
    // builds a fresh transient doc and hands us the initial state update
    // bytes; we drop the doc and pass the bytes to the room.
    let update_bytes = match knot_markdown::from_markdown::parse(&text) {
        Ok((_doc, bytes)) => bytes,
        Err(e) => {
            tracing::warn!(error=?e, "md import parse");
            return json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.parse", "");
        }
    };

    let room = match rooms.acquire(doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "room acquire failed");
            return internal();
        }
    };
    // The two events reply over differently-typed channels (`EngineError`
    // vs `String`), so they cannot share one oneshot — normalise to
    // `Result<i64, String>` here.
    let applied: Result<i64, String> = if replace {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if room
            .tx
            .send(knot_crdt::Event::ReplaceWithMarkdown {
                update_bytes,
                by_user: Some(ctx.user_id),
                reply: tx,
            })
            .await
            .is_err()
        {
            return internal();
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => return internal(),
        }
    } else {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if room
            .tx
            .send(knot_crdt::Event::ApplyUpdate {
                update_bytes,
                by_user: Some(ctx.user_id),
                reply: tx,
            })
            .await
            .is_err()
        {
            return internal();
        }
        match rx.await {
            Ok(Ok(seq)) => Ok(seq),
            Ok(Err(e)) => Err(format!("{e:?}")),
            Err(_) => return internal(),
        }
    };

    match applied {
        Ok(_seq) => {
            let _ = refresh_markdown_and_index(&state, doc_id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, %doc_id, replace, "md import apply");
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.apply", "")
        }
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A counter's running total.
    #[derive(Default)]
    struct Total(AtomicU64);

    impl metrics::CounterFn for Total {
        fn increment(&self, value: u64) {
            self.0.fetch_add(value, Ordering::Relaxed);
        }
        fn absolute(&self, value: u64) {
            self.0.store(value, Ordering::Relaxed);
        }
    }

    /// Remembers the name of every counter registered with it, and adds up
    /// what they count.
    #[derive(Default)]
    struct Counters {
        names: Mutex<Vec<String>>,
        total: Arc<Total>,
    }

    impl metrics::Recorder for Counters {
        fn describe_counter(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_gauge(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn describe_histogram(
            &self,
            _: metrics::KeyName,
            _: Option<metrics::Unit>,
            _: metrics::SharedString,
        ) {
        }
        fn register_counter(
            &self,
            key: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Counter {
            self.names.lock().unwrap().push(key.name().to_owned());
            metrics::Counter::from_arc(self.total.clone())
        }
        fn register_gauge(&self, _: &metrics::Key, _: &metrics::Metadata<'_>) -> metrics::Gauge {
            metrics::Gauge::noop()
        }
        fn register_histogram(
            &self,
            _: &metrics::Key,
            _: &metrics::Metadata<'_>,
        ) -> metrics::Histogram {
            metrics::Histogram::noop()
        }
    }

    #[test]
    fn unknown_nodes_are_counted() {
        let counters = Counters::default();
        metrics::with_local_recorder(&counters, || {
            record_unknown_nodes(Uuid::nil(), &[]);
            record_unknown_nodes(Uuid::nil(), &["callout".to_owned(), "mention".to_owned()]);
        });
        assert_eq!(
            *counters.names.lock().unwrap(),
            ["knot_markdown_unknown_nodes_total"]
        );
        assert_eq!(counters.total.0.load(Ordering::Relaxed), 2);
    }
}
