//! GET /api/workspace/tasks — current user's open tasks across the workspace.
//!
//! Indexed eagerly by the task extractor that runs on each markdown export.
//! Returns rich rows (incl. doc title) so the /tasks page can render a flat
//! list without a separate doc lookup.

use axum::{
    Router,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/workspace/tasks", get(list_mine))
        .route("/api/docs/:doc_id/tasks/:item_index", patch(patch_checked))
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    #[serde(default)]
    include_completed: bool,
}

#[derive(Debug, Serialize)]
struct TaskRow {
    id: String,
    doc_id: String,
    doc_title: String,
    item_index: i32,
    text: String,
    checked: bool,
    completed_at: Option<String>,
    /// "Due by" timestamp lifted from the task's inline datetime chip
    /// when it followed an explicit "by"/"due" cue. Null otherwise.
    due_at: Option<String>,
    updated_at: String,
}

async fn list_mine(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(tasks) = state.tasks.clone() else {
        return internal();
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };

    let rows = match tasks
        .list_for_assignee(ctx.workspace_id, ctx.user_id, q.include_completed)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error=?e, "tasks list_for_assignee");
            return internal();
        }
    };

    // Hydrate doc titles in one shot. Tasks are usually scoped to a small
    // set of docs, so a per-row lookup is fine for v1; if it bites we can
    // switch to a JOIN inside the storage layer.
    let mut out: Vec<TaskRow> = Vec::with_capacity(rows.len());
    for t in rows {
        let title = match docs.get(t.doc_id).await {
            Ok(Some(d)) => d.title,
            _ => "(deleted)".to_string(),
        };
        out.push(TaskRow {
            id: t.id,
            doc_id: t.doc_id.to_string(),
            doc_title: title,
            item_index: t.item_index,
            text: t.text,
            checked: t.checked,
            completed_at: t.completed_at.map(|d| d.to_rfc3339()),
            due_at: t.due_at.map(|d| d.to_rfc3339()),
            updated_at: t.updated_at.to_rfc3339(),
        });
    }

    axum::Json(out).into_response()
}

#[derive(Debug, Deserialize)]
struct PatchBody {
    checked: bool,
}

async fn patch_checked(
    State(state): State<AppState>,
    Path((doc_id, item_index)): Path<(Uuid, i32)>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(acl) = state.acl.clone() else {
        return internal();
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };
    // Editor+ on the parent doc; checking off a task is an edit.
    match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(knot_storage::WorkspaceRole::Owner | knot_storage::WorkspaceRole::Editor)) => {}
        Ok(_) => return json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""),
        Err(_) => return internal(),
    }
    let body_bytes = match axum::body::to_bytes(req.into_body(), 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };
    let body: PatchBody = match serde_json::from_slice(&body_bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    let room = rooms.acquire(doc_id).await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    if room
        .tx
        .send(knot_crdt::Event::PatchTaskChecked {
            item_index,
            checked: body.checked,
            by_user: Some(ctx.user_id),
            reply: tx,
        })
        .await
        .is_err()
    {
        return internal();
    }
    match rx.await {
        Ok(Ok(_)) => {
            // No synchronous reindex here: the room actor's persist
            // path already notifies the reindex worker via dirty_tx
            // (see knot-server::reindex), which picks this doc up on
            // the next tick. Doing both meant two extracts per check.
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(Err(e)) => {
            if e.starts_with("no task at index") {
                return json_err(StatusCode::NOT_FOUND, "task.not_found", "");
            }
            tracing::warn!(error=?e, "patch task");
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "task.patch", "")
        }
        Err(_) => internal(),
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
