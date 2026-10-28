//! Documents API:
//! - GET    /api/docs            flat list (alive only)
//! - POST   /api/docs            body: {title?, parent_id?, after_id?}
//! - GET    /api/docs/{id}        metadata + effective_role (+ archive state)
//! - PATCH  /api/docs/{id}        rename / icon
//! - POST   /api/docs/{id}/move   reparent / reorder
//! - POST   /api/docs/{id}/duplicate  body: {include_subpages?}; copy right after it (editor+)
//! - DELETE /api/docs/{id}        move to the archive, with its subtree (editor+)
//! - POST   /api/docs/{id}/restore  bring an archive root back (editor+)
//! - POST   /api/docs/{id}/purge  delete an archive root for good (owner)
//! - GET    /api/workspace/archive  archive roots, newest first (editor+)

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use knot_storage::{Document, PersonRef, Placement, WorkspaceRole};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::archive::{PurgeError, PurgeTrigger};
use crate::auth::{AuthContext, DocArchived, EffectiveDocRole, require_doc_role_mw};
use crate::duplicate::{BUSY_WAIT, DuplicateError, MAX_DUPLICATE_BLOB_BYTES, MAX_DUPLICATE_PAGES};
use crate::http_error::{doc_archived, json_err};

#[derive(Serialize)]
struct DocResponse {
    id: String,
    workspace_id: String,
    parent_id: Option<String>,
    title: String,
    sort_key: String,
    icon: Option<String>,
    created_by: String,
    archived: bool,
    is_template: bool,
}

fn to_response(d: &Document) -> DocResponse {
    DocResponse {
        id: d.id.to_string(),
        workspace_id: d.workspace_id.to_string(),
        parent_id: d.parent_id.map(|u| u.to_string()),
        title: d.title.clone(),
        sort_key: d.sort_key.clone(),
        icon: d.icon.clone(),
        created_by: d.created_by.to_string(),
        archived: d.archived_at.is_some(),
        is_template: d.is_template,
    }
}

pub fn router(state: AppState) -> Router<AppState> {
    let doc_id_routes: Router<AppState> = Router::new()
        .route("/api/docs/{id}", get(get_one).patch(rename).delete(archive))
        .route("/api/docs/{id}/move", post(move_doc))
        .route("/api/docs/{id}/duplicate", post(duplicate))
        .route(crate::auth::ARCHIVE_RESTORE_ROUTE, post(restore))
        .route(crate::auth::ARCHIVE_PURGE_ROUTE, post(purge))
        .route(
            "/api/docs/{id}/markdown",
            get(crate::routes::api::markdown::export_inline)
                .post(crate::routes::api::markdown::import_inline),
        )
        .route(
            "/api/docs/{id}/grants",
            get(crate::routes::api::grants::list_inline),
        )
        .route(
            "/api/docs/{id}/grants/{principal}",
            put(crate::routes::api::grants::put_inline)
                .delete(crate::routes::api::grants::delete_inline),
        )
        // History endpoints share the same :id param and ACL layer.
        .route(
            "/api/docs/{id}/history",
            get(crate::routes::api::history::list),
        )
        .route(
            "/api/docs/{id}/history/{seq}/markdown",
            get(crate::routes::api::history::preview_markdown),
        )
        .route(
            "/api/docs/{id}/history/{seq}/restore",
            post(crate::routes::api::history::restore),
        )
        .merge(crate::routes::api::comments::routes())
        .merge(crate::routes::api::export::routes())
        .route(
            "/api/docs/{id}/contributors",
            get(crate::routes::api::contributors::byline),
        )
        .route("/api/docs/{id}/template", post(set_template_inline))
        .route(
            "/api/docs/from-template/{id}",
            post(create_from_template_inline),
        )
        .layer(middleware::from_fn_with_state(state, require_doc_role_mw));
    let list_routes: Router<AppState> = Router::new()
        .route("/api/docs", get(list).post(create))
        .route("/api/workspace/templates", get(list_templates))
        .route("/api/workspace/archive", get(list_archive));
    list_routes.merge(doc_id_routes)
}

async fn list(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.list_alive(ctx.workspace_id).await {
        Ok(list) => Json(list.iter().map(to_response).collect::<Vec<_>>()).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct CreateRequest {
    title: Option<String>,
    parent_id: Option<Uuid>,
    after_id: Option<Uuid>,
}

#[tracing::instrument(skip_all, name = "docs.create")]
async fn create(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if ctx.role == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<CreateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let title = body.title.unwrap_or_else(|| "Untitled".into());
    // No anchor: first at its level, where "New page" puts it.
    let placement = body.after_id.map_or(Placement::First, Placement::After);

    match docs
        .create_placed(
            ctx.workspace_id,
            body.parent_id,
            &title,
            placement,
            ctx.user_id,
        )
        .await
    {
        Ok(d) => (StatusCode::CREATED, Json(to_response(&d))).into_response(),
        // The key collided with one written outside placement, such as by
        // an older release during an upgrade. Retrying can succeed.
        Err(knot_storage::DocStoreError::Conflict) => {
            json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "")
        }
        // The parent is in the archive.
        Err(knot_storage::DocStoreError::Archived) => doc_archived(),
        // The parent is not a page of this workspace.
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(knot_storage::DocStoreError::BadAnchor) => bad_anchor(),
        Err(e) => {
            tracing::error!(error=?e, "create");
            internal()
        }
    }
}

async fn get_one(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let doc = match docs.get(doc_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, "get");
            return internal();
        }
    };
    #[derive(Serialize)]
    struct GetResponse {
        #[serde(flatten)]
        doc: DocResponse,
        effective_role: String,
        #[serde(flatten)]
        archive: ArchiveInfo,
    }
    let archive = match archive_info(&state, &doc).await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!(error = %e, "get archive info");
            return internal();
        }
    };
    Json(GetResponse {
        doc: to_response(&doc),
        effective_role: role.0.as_str().into(),
        archive,
    })
    .into_response()
}

/// What the archived-doc banner needs; every field is `null` on a live doc.
#[derive(Serialize, Default)]
struct ArchiveInfo {
    archived_at: Option<chrono::DateTime<chrono::Utc>>,
    archived_by: Option<PersonRef>,
    purge_at: Option<chrono::DateTime<chrono::Utc>>,
    archive_root_id: Option<Uuid>,
    archive_root_title: Option<String>,
}

async fn archive_info(state: &AppState, doc: &Document) -> Result<ArchiveInfo, String> {
    let (Some(archived_at), Some(root_id)) = (doc.archived_at, doc.archive_root_id) else {
        return Ok(ArchiveInfo::default());
    };
    let users = state.users.clone().ok_or("users not configured")?;
    let docs = state.docs.clone().ok_or("docs not configured")?;
    let root = if root_id == doc.id {
        Some(doc.clone())
    } else {
        docs.get(root_id).await.map_err(|e| e.to_string())?
    };
    let archived_by = match doc.archived_by {
        Some(user_id) => users
            .find_by_id(user_id)
            .await
            .map_err(|e| e.to_string())?
            .map(|user| PersonRef {
                id: user.id,
                display_name: user.display_name,
            }),
        None => None,
    };
    Ok(ArchiveInfo {
        archived_at: Some(archived_at),
        archived_by,
        // A descendant goes when its root does.
        purge_at: Some(
            root.as_ref()
                .and_then(|r| r.archived_at)
                .unwrap_or(archived_at)
                + crate::archive::retention(state),
        ),
        archive_root_id: Some(root_id),
        archive_root_title: root.map(|r| r.title),
    })
}

#[derive(Deserialize)]
struct PatchRequest {
    title: Option<String>,
    icon: Option<String>,
}

#[tracing::instrument(skip_all, name = "docs.rename")]
async fn rename(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<PatchRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let cur = match docs.get(doc_id).await {
        Ok(Some(d)) => d,
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, "rename get");
            return internal();
        }
    };
    let title = body.title.as_deref().unwrap_or(&cur.title);
    match docs
        .rename(
            ctx.workspace_id,
            doc_id,
            ctx.user_id,
            title,
            body.icon.as_deref(),
        )
        .await
    {
        Ok(d) => Json(to_response(&d)).into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "rename");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct MoveRequest {
    parent_id: Option<Uuid>,
    after_id: Option<Uuid>,
    before_id: Option<Uuid>,
}

async fn move_doc(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    if role.0 == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<MoveRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };

    // Spec §6.1: `parent_id` in the request body is the *target* parent. An
    // explicit `null` (or omitted field, since serde collapses both to
    // `None`) moves the doc to the workspace root. `move_placed` will surface
    // `DocStoreError::NotFound` if the document doesn't exist, or if the
    // target parent is not a page of this workspace, so we skip a pre-flight
    // existence check here.
    let new_parent = body.parent_id;
    let placement = match (body.after_id, body.before_id) {
        (Some(after), _) => Placement::After(after),
        (None, Some(before)) => Placement::Before(before),
        (None, None) => Placement::Last,
    };

    match docs
        .move_placed(ctx.workspace_id, doc_id, ctx.user_id, new_parent, placement)
        .await
    {
        Ok(d) => Json(to_response(&d)).into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(knot_storage::DocStoreError::Conflict) => {
            json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "")
        }
        Err(knot_storage::DocStoreError::Cycle) => {
            json_err(StatusCode::CONFLICT, "doc.move_cycle", "")
        }
        // The doc or the destination parent is in the archive.
        Err(knot_storage::DocStoreError::Archived) => doc_archived(),
        Err(knot_storage::DocStoreError::BadAnchor) => bad_anchor(),
        Err(e) => {
            tracing::error!(error=?e, "move");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct DuplicateRequest {
    /// Copy the page's live subpages too. Off unless asked for.
    #[serde(default)]
    include_subpages: bool,
}

/// Copy a page right after it, with its subpages when asked (spec D). See
/// `crate::duplicate`.
#[tracing::instrument(skip_all, name = "docs.duplicate")]
async fn duplicate(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    let Ok(body) = read_json::<DuplicateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    // Before the wait for a permit: these answers need none, so a busy
    // server doesn't turn them into 503s.
    if let Err(e) = crate::duplicate::check(&state, &ctx, role.0, doc_id).await {
        return duplicate_error(e);
    }
    // Taken before the copy starts, and held until its refresh ends, so no
    // more than the permitted copies are held in memory at once.
    let permit = tokio::time::timeout(
        state.duplicate_busy_wait,
        state.duplicate_permits.clone().acquire_owned(),
    )
    .await;
    let permit = match permit {
        Ok(Ok(permit)) => permit,
        Ok(Err(e)) => {
            tracing::error!(error = %e, "duplicate permits closed");
            return internal();
        }
        Err(_) => return duplicate_busy(),
    };
    // On a task of its own (P16): dropping this handler, on the request
    // timeout or when the client goes away, can't stop a duplicate halfway.
    // The answer comes once the copy is stored, before its refresh.
    let answer = crate::duplicate::start(state, ctx, role.0, doc_id, body.include_subpages, permit);
    match answer.await {
        Ok(Ok(top)) => (StatusCode::CREATED, Json(to_response(&top))).into_response(),
        Ok(Err(e)) => duplicate_error(e),
        Err(_) => {
            tracing::error!("duplicate task ended without an answer");
            internal()
        }
    }
}

/// `503 doc.duplicate_busy`: every duplicate permit stayed taken for the
/// busy wait. A retry is worth it once a copy may have ended: after
/// [`BUSY_WAIT`], the time a duplicate waits for a permit.
fn duplicate_busy() -> Response {
    let mut response = json_err(
        StatusCode::SERVICE_UNAVAILABLE,
        "doc.duplicate_busy",
        "too many copies at once",
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(BUSY_WAIT.as_secs()));
    response
}

fn duplicate_error(e: DuplicateError) -> Response {
    match e {
        DuplicateError::EditorRequired => {
            json_err(StatusCode::FORBIDDEN, "acl.editor_required", "")
        }
        DuplicateError::NotFound => json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        DuplicateError::IsTemplate => {
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "doc.is_template", "")
        }
        DuplicateError::Archived => doc_archived(),
        DuplicateError::TooLarge => json_err(
            StatusCode::PAYLOAD_TOO_LARGE,
            "doc.duplicate_too_large",
            &format!(
                "over {MAX_DUPLICATE_PAGES} pages or {} MiB of files",
                MAX_DUPLICATE_BLOB_BYTES / (1024 * 1024)
            ),
        ),
        // `DocStoreError::DuplicateConflict` and a file object missing from
        // the store: the source was moved or archived, or a file it shows
        // was purged or lost, while it was copied.
        DuplicateError::Conflict => json_err(StatusCode::CONFLICT, "doc.duplicate_conflict", ""),
        DuplicateError::SortKeyConflict => {
            json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "")
        }
        DuplicateError::Internal(e) => {
            tracing::error!(error = %e, "duplicate");
            internal()
        }
    }
}

#[tracing::instrument(skip_all, name = "docs.archive")]
async fn archive(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    // Whoever can edit a doc can move it to the archive (#37: the sidebar
    // offered Delete to editors, but this required owner).
    if role.0 == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.archive(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(archived_ids) => {
            crate::archive::evict_local(&state, &archived_ids).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(knot_storage::DocStoreError::Archived) => doc_archived(),
        Err(e) => {
            tracing::error!(error=?e, "archive");
            internal()
        }
    }
}

async fn restore(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    // Anyone who could archive it can undo that.
    if role.0 == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.restore(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(restored_ids) => {
            crate::archive::evict_local(&state, &restored_ids).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(knot_storage::DocStoreError::NotArchiveRoot) => {
            json_err(StatusCode::CONFLICT, "doc.not_archive_root", "")
        }
        // A purge has started deleting its attachments.
        Err(knot_storage::DocStoreError::Purging) => {
            json_err(StatusCode::CONFLICT, "doc.purging", "")
        }
        // The key it would get at the top level was taken by a writer
        // outside placement, such as an older release during an upgrade.
        // Retrying can succeed.
        Err(knot_storage::DocStoreError::Conflict) => {
            json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "")
        }
        // It was restored, moved and archived again while this ran.
        // Retrying can succeed.
        Err(knot_storage::DocStoreError::RestoreConflict) => {
            json_err(StatusCode::CONFLICT, "doc.restore_conflict", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "restore");
            internal()
        }
    }
}

#[tracing::instrument(skip_all, name = "docs.purge")]
async fn purge(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    // Purging can't be undone; only owners may do it ahead of retention.
    if role.0 != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    if req.extensions().get::<DocArchived>().is_none() {
        return json_err(StatusCode::CONFLICT, "doc.not_archived", "");
    }
    match crate::archive::purge_doc(
        &state,
        ctx.workspace_id,
        doc_id,
        Some(ctx.user_id),
        PurgeTrigger::Manual,
    )
    .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(PurgeError::NotFound) => json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(PurgeError::NotArchived) => json_err(StatusCode::CONFLICT, "doc.not_archived", ""),
        Err(PurgeError::NotArchiveRoot) => {
            json_err(StatusCode::CONFLICT, "doc.not_archive_root", "")
        }
        Err(e) => {
            tracing::error!(error = %e, %doc_id, "purge");
            internal()
        }
    }
}

/// One row of `GET /api/workspace/archive`.
#[derive(Serialize)]
struct ArchiveEntry {
    id: Uuid,
    title: String,
    parent_id: Option<Uuid>,
    parent_title: Option<String>,
    subpage_count: i64,
    archived_at: chrono::DateTime<chrono::Utc>,
    archived_by: Option<PersonRef>,
    purge_at: chrono::DateTime<chrono::Utc>,
    /// A purge has started and not finished (it is retried hourly).
    purging: bool,
}

async fn list_archive(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // Viewers never see archived docs, so the archive isn't theirs to browse.
    if ctx.role == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let retention = crate::archive::retention(&state);
    match docs.list_archive_roots(ctx.workspace_id).await {
        Ok(roots) => Json(
            roots
                .into_iter()
                .map(|root| ArchiveEntry {
                    id: root.id,
                    title: root.title,
                    parent_id: root.parent_id,
                    parent_title: root.parent_title,
                    subpage_count: root.subpage_count,
                    archived_at: root.archived_at,
                    archived_by: root
                        .archived_by
                        .zip(root.archived_by_name)
                        .map(|(id, display_name)| PersonRef { id, display_name }),
                    purge_at: root.archived_at + retention,
                    purging: root.purging,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list_archive");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct SetTemplateRequest {
    is_template: bool,
}

#[tracing::instrument(skip_all, name = "docs.set_template")]
async fn set_template_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    // Templates are workspace-level affordances; only owners flip the flag.
    if role.0 != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Ok(body) = read_json::<SetTemplateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs
        .set_template(ctx.workspace_id, doc_id, ctx.user_id, body.is_template)
        .await
    {
        Ok(d) => Json(to_response(&d)).into_response(),
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "set_template");
            internal()
        }
    }
}

async fn list_templates(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    match docs.list_templates(ctx.workspace_id).await {
        Ok(list) => Json(list.iter().map(to_response).collect::<Vec<_>>()).into_response(),
        Err(e) => {
            tracing::error!(error=?e, "list_templates");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct FromTemplateRequest {
    title: Option<String>,
    parent_id: Option<Uuid>,
}

/// Create a new doc whose content is a clean markdown-clone of the
/// template. Intentionally drops the source template's comments,
/// history, and CRDT lineage — templates exist to be instantiated, not
/// linked. ACL: read on the template (enforced by the route layer);
/// write on the destination parent is enforced inline below.
#[tracing::instrument(skip_all, name = "docs.from_template")]
async fn create_from_template_inline(
    State(state): State<AppState>,
    Path(template_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    // The route layer gives us read access on the template; that's the
    // gate for "can use as a template". Writing into the workspace tree
    // requires editor+.
    if ctx.role == WorkspaceRole::Viewer {
        return json_err(StatusCode::FORBIDDEN, "acl.editor_required", "");
    }
    let Ok(body) = read_json::<FromTemplateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };

    // Pull the template's current markdown via the shared
    // export-from-room path so we get exactly what the user sees.
    let md = match crate::routes::api::markdown::export_markdown_only(&state, template_id).await {
        Ok(text) => text,
        Err(e) => {
            tracing::error!(error=?e, %template_id, "from_template: refresh");
            return internal();
        }
    };

    // Default title falls back to the template's title + " copy".
    let title = match body.title {
        Some(t) if !t.trim().is_empty() => t,
        _ => match docs.get(template_id).await {
            Ok(Some(d)) => format!("{} copy", d.title),
            _ => "Untitled".to_string(),
        },
    };

    // At the end of the destination parent's children.
    let new_doc = match docs
        .create_placed(
            ctx.workspace_id,
            body.parent_id,
            &title,
            Placement::Last,
            ctx.user_id,
        )
        .await
    {
        Ok(d) => d,
        Err(knot_storage::DocStoreError::Conflict) => {
            return json_err(StatusCode::CONFLICT, "doc.sort_key_conflict", "");
        }
        Err(knot_storage::DocStoreError::Archived) => return doc_archived(),
        // The parent is not a page of this workspace.
        Err(knot_storage::DocStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "from_template create");
            return internal();
        }
    };

    // Import the template's markdown into the new doc via the room
    // actor (mirrors POST /api/docs/{id}/markdown).
    let update_bytes = match knot_markdown::from_markdown::parse(&md) {
        Ok((_doc, bytes)) => bytes,
        Err(e) => {
            tracing::warn!(error=?e, "from_template parse");
            return json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.parse", "");
        }
    };
    let Some(rooms) = state.rooms_v2.clone() else {
        return internal();
    };
    let room = match rooms.acquire(new_doc.id).await {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error=?e, doc_id=%new_doc.id, "room acquire failed");
            return internal();
        }
    };
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
        Ok(Ok(_)) => (StatusCode::CREATED, Json(to_response(&new_doc))).into_response(),
        Ok(Err(e)) => {
            tracing::warn!(error=?e, "from_template apply");
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "markdown.apply", "")
        }
        Err(_) => internal(),
    }
}

async fn read_json<T: serde::de::DeserializeOwned>(req: Request) -> Result<T, ()> {
    let bytes = axum::body::to_bytes(req.into_body(), 64 * 1024)
        .await
        .map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

/// 422 `doc.bad_anchor`: `after_id` or `before_id` is not a live,
/// non-template page under the target parent, or it is the moved doc.
fn bad_anchor() -> Response {
    json_err(StatusCode::UNPROCESSABLE_ENTITY, "doc.bad_anchor", "")
}
