//! Documents API:
//! - GET    /api/docs            flat list (alive, readable only; per-doc role)
//! - POST   /api/docs            body: {title?, parent_id?, after_id?}
//! - GET    /api/docs/{id}        metadata + effective_role (+ archive state)
//! - PATCH  /api/docs/{id}        rename / icon
//! - PUT    /api/docs/{id}/restricted  body: {restricted}; make it a restriction boundary (owner)
//! - GET    /api/docs/{id}/access  who can see it, for the permissions dialog (any reader)
//! - POST   /api/docs/{id}/move   reparent / reorder
//! - POST   /api/docs/{id}/duplicate  body: {include_subpages?}; copy right after it (editor+ here and on the parent)
//! - DELETE /api/docs/{id}        move to the archive, with its subtree (editor+)
//! - POST   /api/docs/{id}/restore  bring an archive root back (editor+)
//! - POST   /api/docs/{id}/purge  delete an archive root for good (owner)
//! - GET    /api/workspace/archive  archive roots, newest first (editor+)

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use knot_docs::Visibility;
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
    /// A restriction boundary (only explicit grants give access below it).
    restricted: bool,
}

/// `d` as stored, parent included. Never sent as is: a parent may be a
/// hidden restricted folder. Responses go through [`caller_doc_response`] (one
/// doc) or [`listed_response`] (listings), which null a parent the caller
/// can't read.
fn unfiltered_response(d: &Document) -> DocResponse {
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
        restricted: d.restricted,
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
        .route("/api/docs/{id}/restricted", put(set_restricted_inline))
        .route(
            "/api/docs/{id}/access",
            get(crate::routes::api::grants::access_inline),
        )
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

/// One sidebar entry: a doc the caller can read, with their role on it.
#[derive(Serialize)]
struct ListedDoc {
    #[serde(flatten)]
    doc: DocResponse,
    /// The caller's role on this doc (grants can raise it above the
    /// workspace role, and restrictions replace the workspace role).
    effective_role: &'static str,
    /// In a restricted subtree: the doc or an ancestor is a boundary.
    within_restricted: bool,
    /// The doc has a parent the caller can't read. `parent_id` is then
    /// `null`, so the hidden parent's id never reaches the client, and the
    /// sidebar shows the doc under "Shared with you".
    parent_hidden: bool,
}

/// The caller's access to every doc of their workspace, known to cover
/// `listed_docs` and their parents (a parent the snapshot doesn't know
/// would be taken for hidden). Listings filter through it; on error they
/// answer 500 rather than an unfiltered list.
async fn caller_visibility(
    state: &AppState,
    ctx: &AuthContext,
    listed_docs: &[Document],
) -> Result<Arc<Visibility>, Response> {
    let covered_ids: Vec<Uuid> = listed_docs
        .iter()
        .flat_map(|doc| std::iter::once(doc.id).chain(doc.parent_id))
        .collect();
    visibility_covering_ids(state, ctx, &covered_ids).await
}

/// The caller's access to every doc of their workspace, known to cover
/// `doc_ids` where they are in it. 500 on error.
pub(super) async fn visibility_covering_ids(
    state: &AppState,
    ctx: &AuthContext,
    doc_ids: &[Uuid],
) -> Result<Arc<Visibility>, Response> {
    let Some(acl) = state.acl.as_ref() else {
        return Err(internal());
    };
    acl.visibility_covering(ctx.workspace_id, ctx.user_id, doc_ids)
        .await
        .map_err(|e| {
            tracing::error!(error=?e, "visibility");
            internal()
        })
}

/// 409 `doc.hidden_descendants` when the caller can't read one of
/// `subtree_ids`: archiving or purging them would hide or destroy pages
/// they can't see. An archived page they only view counts, since viewers
/// can't read archived pages. Says only that *something* is hidden, never
/// what. Ids outside the workspace are not the caller's to judge, and are
/// skipped.
async fn refuse_hidden_descendants(
    state: &AppState,
    ctx: &AuthContext,
    subtree_ids: &[Uuid],
) -> Result<(), Response> {
    let visibility = visibility_covering_ids(state, ctx, subtree_ids).await?;
    let any_hidden = subtree_ids
        .iter()
        .any(|doc_id| visibility.covers(*doc_id) && !visibility.readable(*doc_id));
    if any_hidden {
        return Err(json_err(StatusCode::CONFLICT, "doc.hidden_descendants", ""));
    }
    Ok(())
}

/// `doc` as a listing shows it to the caller. A parent they can't read
/// is nulled, so its id never reaches the client; the flag says so.
fn listed_response(doc: &Document, visibility: &Visibility) -> (DocResponse, bool) {
    let parent_hidden = doc
        .parent_id
        .is_some_and(|parent_id| !visibility.readable(parent_id));
    let mut doc_response = unfiltered_response(doc);
    if parent_hidden {
        doc_response.parent_id = None;
    }
    (doc_response, parent_hidden)
}

/// `doc` as a single-doc answer shows it to the caller: every route that
/// answers with one doc builds it here. A parent they can't read is
/// nulled, so its id never reaches the client (a page shared from inside a
/// hidden restricted folder must not name it). 500 on error.
async fn caller_doc_response(
    state: &AppState,
    ctx: &AuthContext,
    doc: &Document,
) -> Result<DocResponse, Response> {
    let mut doc_response = unfiltered_response(doc);
    let Some(parent_id) = doc.parent_id else {
        return Ok(doc_response);
    };
    let Some(acl) = state.acl.as_ref() else {
        return Err(internal());
    };
    match acl
        .effective_role(ctx.workspace_id, parent_id, ctx.user_id)
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => doc_response.parent_id = None,
        Err(e) => {
            tracing::error!(error=?e, "doc response parent acl");
            return Err(internal());
        }
    }
    Ok(doc_response)
}

/// [`caller_doc_response`] as a JSON answer with `status`.
async fn doc_answer(
    state: &AppState,
    ctx: &AuthContext,
    status: StatusCode,
    doc: &Document,
) -> Response {
    match caller_doc_response(state, ctx, doc).await {
        Ok(doc_response) => (status, Json(doc_response)).into_response(),
        Err(response) => response,
    }
}

async fn list(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let alive_docs = match docs.list_alive(ctx.workspace_id).await {
        Ok(alive_docs) => alive_docs,
        Err(e) => {
            tracing::error!(error=?e, "list");
            return internal();
        }
    };
    let visibility = match caller_visibility(&state, &ctx, &alive_docs).await {
        Ok(visibility) => visibility,
        Err(response) => return response,
    };
    let listed: Vec<ListedDoc> = alive_docs
        .iter()
        .filter_map(|doc| {
            let role = visibility.effective_role(doc.id)?;
            let access = visibility.access(doc.id)?;
            let (doc_response, parent_hidden) = listed_response(doc, &visibility);
            Some(ListedDoc {
                doc: doc_response,
                effective_role: role.as_str(),
                within_restricted: access.restriction_root.is_some(),
                parent_hidden,
            })
        })
        .collect();
    Json(listed).into_response()
}

/// Where a create, from-template or move puts a page: under a parent, the
/// caller must be able to edit there (see
/// `restricted::require_editable_parent`); at the top level, their
/// workspace role must be editor or owner.
async fn require_editable_destination(
    state: &AppState,
    ctx: &AuthContext,
    parent_id: Option<Uuid>,
) -> Result<(), Response> {
    match parent_id {
        Some(parent_id) => crate::restricted::require_editable_parent(state, ctx, parent_id).await,
        None if ctx.role == WorkspaceRole::Viewer => {
            Err(json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""))
        }
        None => Ok(()),
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
    let Ok(body) = read_json::<CreateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    if let Err(response) = require_editable_destination(&state, &ctx, body.parent_id).await {
        return response;
    }
    if let Err(response) =
        crate::restricted::require_readable_anchors(&state, &ctx, &[body.after_id]).await
    {
        return response;
    }
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
        Ok(d) => {
            if let Err(response) = crate::restricted::ensure_creator_can_edit(
                &state,
                ctx.workspace_id,
                d.id,
                ctx.user_id,
            )
            .await
            {
                return response;
            }
            doc_answer(&state, &ctx, StatusCode::CREATED, &d).await
        }
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
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(role) = req.extensions().get::<EffectiveDocRole>().copied() else {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    };
    let (Some(docs), Some(acl)) = (state.docs.clone(), state.acl.clone()) else {
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
        /// In a restricted subtree: the doc or an ancestor is a boundary.
        within_restricted: bool,
        /// The nearest boundary, when the caller can read it.
        restriction_root_id: Option<String>,
        #[serde(flatten)]
        archive: ArchiveInfo,
    }
    // Cached by the middleware's lookup a moment ago.
    let access = match acl.access(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(Some(access)) => access,
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(e) => {
            tracing::error!(error=?e, "get acl");
            return internal();
        }
    };
    let mut archive = match archive_info(&state, &doc).await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!(error = %e, "get archive info");
            return internal();
        }
    };
    // Ids and titles of docs the caller can't read are never sent: a page
    // shared from inside a hidden restricted folder must not name it.
    let readable = |other_id: Uuid| {
        let acl = acl.clone();
        async move {
            acl.effective_role(ctx.workspace_id, other_id, ctx.user_id)
                .await
                .map(|other_role| other_role.is_some())
        }
    };
    let doc_response = match caller_doc_response(&state, &ctx, &doc).await {
        Ok(doc_response) => doc_response,
        Err(response) => return response,
    };
    let restriction_root_id = match access.restriction_root {
        Some(root_id) if root_id == doc_id => Some(root_id),
        Some(root_id) => match readable(root_id).await {
            Ok(true) => Some(root_id),
            Ok(false) => None,
            Err(e) => {
                tracing::error!(error=?e, "get restriction root acl");
                return internal();
            }
        },
        None => None,
    };
    if let Some(root_id) = archive.archive_root_id.filter(|root_id| *root_id != doc_id) {
        match readable(root_id).await {
            Ok(true) => {}
            Ok(false) => {
                archive.archive_root_id = None;
                archive.archive_root_title = None;
            }
            Err(e) => {
                tracing::error!(error=?e, "get archive root acl");
                return internal();
            }
        }
    }
    Json(GetResponse {
        doc: doc_response,
        effective_role: role.0.as_str().into(),
        within_restricted: access.restriction_root.is_some(),
        restriction_root_id: restriction_root_id.map(|root_id| root_id.to_string()),
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
        Ok(d) => doc_answer(&state, &ctx, StatusCode::OK, &d).await,
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
    // `None`) moves the doc to the workspace root. A target parent the
    // caller can't read answers like one no page has (`doc.not_found`), and
    // so does an anchor (`doc.bad_anchor`).
    let new_parent = body.parent_id;
    if let Err(response) = require_editable_destination(&state, &ctx, new_parent).await {
        return response;
    }
    if let Err(response) =
        crate::restricted::require_readable_anchors(&state, &ctx, &[body.after_id, body.before_id])
            .await
    {
        return response;
    }
    let placement = match (body.after_id, body.before_id) {
        (Some(after), _) => Placement::After(after),
        (None, Some(before)) => Placement::Before(before),
        (None, None) => Placement::Last,
    };
    // Into a restricted subtree, out of one, or between two: owners only.
    // Storage decides whether the move crosses, under the move's lock.
    let allow_boundary_cross = role.0 == WorkspaceRole::Owner;

    match docs
        .move_placed(
            ctx.workspace_id,
            doc_id,
            ctx.user_id,
            new_parent,
            placement,
            allow_boundary_cross,
        )
        .await
    {
        Ok(d) => {
            // The invalidation evicts the subtree on every replica,
            // asynchronously; evict here too, so a move into or out of a
            // restricted subtree shows in this replica's next answer.
            crate::archive::evict_local_subtree(&state, doc_id).await;
            doc_answer(&state, &ctx, StatusCode::OK, &d).await
        }
        Err(knot_storage::DocStoreError::RestrictionBoundary) => {
            json_err(StatusCode::FORBIDDEN, "acl.restricted_move", "")
        }
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
    let answer = crate::duplicate::start(
        state.clone(),
        ctx.clone(),
        role.0,
        doc_id,
        body.include_subpages,
        permit,
    );
    match answer.await {
        Ok(Ok(top)) => doc_answer(&state, &ctx, StatusCode::CREATED, &top).await,
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
        DuplicateError::Destination(response) => response,
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
    // Archived descendants count too: a purge of this doc would take them.
    let descendant_ids = match docs.descendant_ids(doc_id).await {
        Ok(descendant_ids) => descendant_ids,
        Err(e) => {
            tracing::error!(error=?e, "archive descendants");
            return internal();
        }
    };
    if let Err(response) = refuse_hidden_descendants(&state, &ctx, &descendant_ids).await {
        return response;
    }
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
        // Its parent is archived, and the top level it would go to is
        // outside the restricted subtree it was in.
        Err(knot_storage::DocStoreError::RestoreRestrictedParentArchived) => json_err(
            StatusCode::CONFLICT,
            "doc.restore_restricted_parent_archived",
            "",
        ),
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
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let subtree_ids = match docs.subtree_ids(doc_id).await {
        Ok(subtree_ids) => subtree_ids,
        Err(e) => {
            tracing::error!(error=?e, "purge subtree");
            return internal();
        }
    };
    if let Err(response) = refuse_hidden_descendants(&state, &ctx, &subtree_ids).await {
        return response;
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
    /// The readable subpages archived together with it.
    subpage_count: usize,
    archived_at: chrono::DateTime<chrono::Utc>,
    archived_by: Option<PersonRef>,
    purge_at: chrono::DateTime<chrono::Utc>,
    /// A purge has started and not finished (it is retried hourly).
    purging: bool,
    /// The parent exists but the caller can't read it; `parent_id` and
    /// `parent_title` are then null, as they are at the top level.
    parent_hidden: bool,
    /// The caller's role on the page, which on a restricted page need not
    /// be the workspace role (purge takes an owner).
    effective_role: &'static str,
    /// The archived subpages the caller can open, for links to them.
    subpage_ids: Vec<Uuid>,
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
    let roots = match docs.list_archive_roots(ctx.workspace_id).await {
        Ok(roots) => roots,
        Err(e) => {
            tracing::error!(error=?e, "list_archive");
            return internal();
        }
    };
    let covered_ids: Vec<Uuid> = roots
        .iter()
        .flat_map(|root| {
            std::iter::once(root.id)
                .chain(root.parent_id)
                .chain(root.subpage_ids.iter().copied())
        })
        .collect();
    let visibility = match visibility_covering_ids(&state, &ctx, &covered_ids).await {
        Ok(visibility) => visibility,
        Err(response) => return response,
    };
    // Only what the caller could restore, and nothing named that they
    // can't read: a parent may be a hidden restricted folder.
    let entries: Vec<ArchiveEntry> = roots
        .into_iter()
        .filter_map(|root| {
            let role = visibility
                .access(root.id)
                .map(|access| access.role)
                .filter(|role| matches!(role, WorkspaceRole::Owner | WorkspaceRole::Editor))?;
            Some((root, role))
        })
        .map(|(root, role)| {
            let parent_readable = root
                .parent_id
                .is_some_and(|parent_id| visibility.readable(parent_id));
            let openable_subpage_ids: Vec<Uuid> = root
                .subpage_ids
                .iter()
                .copied()
                .filter(|subpage_id| visibility.readable(*subpage_id))
                .collect();
            ArchiveEntry {
                id: root.id,
                title: root.title,
                parent_id: root.parent_id.filter(|_| parent_readable),
                parent_title: root.parent_title.filter(|_| parent_readable),
                subpage_count: openable_subpage_ids.len(),
                archived_at: root.archived_at,
                archived_by: root
                    .archived_by
                    .zip(root.archived_by_name)
                    .map(|(id, display_name)| PersonRef { id, display_name }),
                purge_at: root.archived_at + retention,
                purging: root.purging,
                parent_hidden: root.parent_id.is_some() && !parent_readable,
                effective_role: role.as_str(),
                subpage_ids: openable_subpage_ids,
            }
        })
        .collect();
    Json(entries).into_response()
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
        Ok(d) => doc_answer(&state, &ctx, StatusCode::OK, &d).await,
        Err(knot_storage::DocStoreError::NotFound) => {
            json_err(StatusCode::NOT_FOUND, "doc.not_found", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "set_template");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct SetRestrictedRequest {
    restricted: bool,
}

/// `PUT /api/docs/{id}/restricted`: make the doc a restriction boundary, or
/// stop it being one. Owner on the doc only; see
/// `DocStore::set_restricted` for what restricting also does.
#[tracing::instrument(skip_all, name = "docs.set_restricted")]
async fn set_restricted_inline(
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
    if role.0 != WorkspaceRole::Owner {
        return json_err(StatusCode::FORBIDDEN, "acl.owner_required", "");
    }
    let Ok(body) = read_json::<SetRestrictedRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let doc = match docs
        .set_restricted(ctx.workspace_id, doc_id, ctx.user_id, body.restricted)
        .await
    {
        Ok(d) => d,
        Err(knot_storage::DocStoreError::NotFound) => {
            return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
        }
        Err(e) => {
            tracing::error!(error=?e, "set_restricted");
            return internal();
        }
    };
    // The invalidation evicts the subtree on every replica, asynchronously;
    // evict here too so this replica reads its own write right away.
    crate::archive::evict_local_subtree(&state, doc_id).await;
    doc_answer(&state, &ctx, StatusCode::OK, &doc).await
}

async fn list_templates(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(docs) = state.docs.clone() else {
        return internal();
    };
    let templates = match docs.list_templates(ctx.workspace_id).await {
        Ok(templates) => templates,
        Err(e) => {
            tracing::error!(error=?e, "list_templates");
            return internal();
        }
    };
    let visibility = match caller_visibility(&state, &ctx, &templates).await {
        Ok(visibility) => visibility,
        Err(response) => return response,
    };
    let listed: Vec<DocResponse> = templates
        .iter()
        .filter(|template| visibility.readable(template.id))
        .map(|template| listed_response(template, &visibility).0)
        .collect();
    Json(listed).into_response()
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
/// edit on the destination parent, or the workspace role at the top level,
/// is enforced inline below.
#[tracing::instrument(skip_all, name = "docs.from_template")]
async fn create_from_template_inline(
    State(state): State<AppState>,
    Path(template_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Ok(body) = read_json::<FromTemplateRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    // The route layer gives us read access on the template; that's the
    // gate for "can use as a template". Writing into the tree requires
    // editor+ where the copy goes.
    if let Err(response) = require_editable_destination(&state, &ctx, body.parent_id).await {
        return response;
    }
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
    if let Err(response) = crate::restricted::ensure_creator_can_edit(
        &state,
        ctx.workspace_id,
        new_doc.id,
        ctx.user_id,
    )
    .await
    {
        return response;
    }

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
        Ok(Ok(_)) => doc_answer(&state, &ctx, StatusCode::CREATED, &new_doc).await,
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
