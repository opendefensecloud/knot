//! Doc history endpoints:
//! GET  /api/docs/{doc_id}/history              → snapshot metadata list (newest-first)
//! GET  /api/docs/{doc_id}/history/entries      → page history entries (M1.3), keyset-paged
//! GET  /api/docs/{doc_id}/history/{seq}/markdown → markdown preview of snapshot
//! POST /api/docs/{doc_id}/history/{seq}/restore  → replace live doc with snapshot

use std::collections::HashSet;
use std::future::Future;

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_crdt::{Engine, YrsEngine};
use knot_docs::DocAccess;
use knot_docs::acl::ResolveError;
use knot_history::{Cursor, DEFAULT_LIMIT, MAX_LIMIT};
use serde::Deserialize;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, require_editor};
use crate::http_error::json_err;

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

    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(knot_history::render_page(&page, &people, &readable)),
    )
        .into_response()
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
