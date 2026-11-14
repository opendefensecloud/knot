//! Doc history endpoints (editors and owners; also on an archived page):
//! GET  /api/docs/{doc_id}/history/entries      → page history entries (M1.3), keyset-paged
//! GET  /api/docs/{doc_id}/history/entries/{eid}/view?changes=0|1
//!                                              → a past version, rendered (M1.4)
//!
//! The snapshot routes before the timeline:
//! GET  /api/docs/{doc_id}/history              → `[]` (it no longer lists snapshots)
//! GET  /api/docs/{doc_id}/history/{seq}/markdown → markdown preview of snapshot
//! POST /api/docs/{doc_id}/history/{seq}/restore  → replace live doc with snapshot
//!
//! These three stay through the combined page-history release: a 0.9.13
//! drawer whose list came from an old pod still previews and restores on
//! a new pod during the upgrade, and the seq restore is the operator's
//! workaround for a page without a restorable entry. No client of the
//! release calls them; they go in the release after it.

use std::collections::HashSet;
use std::convert::Infallible;
use std::future::Future;

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_crdt::{Engine, YrsEngine};
use knot_docs::DocAccess;
use knot_docs::acl::ResolveError;
use knot_history::{Cursor, DEFAULT_LIMIT, MAX_LIMIT, ViewError};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, DocArchived, require_editor};
use crate::http_error::json_err;

/// `GET /api/docs/{id}/history`: the snapshot list of the drawer before
/// the timeline. It lists nothing (`[]`), behind its gates, so a 0.9.13
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
    let room = match rooms.acquire(doc_id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "room acquire failed");
            return internal();
        }
    };
    let (tx, rx) = tokio::sync::oneshot::channel();
    if room
        .tx
        .send(knot_crdt::Event::ReplaceWithMarkdown {
            update_bytes,
            // The restorer rewrote the page, so they are its latest
            // contributor — not the authors of the snapshot's content.
            by_user: Some(ctx.user_id),
            // The version restored to, by its snapshot seq and the time it
            // was taken (a page's history shows "Restored by X, to <date>").
            source: knot_storage::UpdateSource::Restore,
            meta: Some(serde_json::json!({
                "target_snapshot_seq": seq,
                "target_ended_at": snap.created_at.to_rfc3339(),
            })),
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
            .map(|acl| move |id| acl.access(ctx.workspace_id, id, ctx.user_id)),
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
    use knot_storage::{DocStoreError, WorkspaceRole};

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
