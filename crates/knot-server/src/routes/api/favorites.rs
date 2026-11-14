//! The caller's favorite pages in this workspace.
//!
//! GET    /api/me/favorites            → [{ doc_id, title, icon, favorited_at }]
//! PUT    /api/me/favorites/{doc_id}   → 204 (star; idempotent)
//! DELETE /api/me/favorites/{doc_id}   → 204 (unstar; idempotent)
//!
//! A star is a private preference: nothing is audited. The list leaves out
//! pages the caller can't read now (restricted, archived, templates) rather
//! than refusing; the rows stay, so a favorite comes back with access. PUT
//! checks access with the same codes and in the same order as
//! `require_doc_role_mw`. DELETE only touches the caller's own row and
//! checks nothing, so a page that became hidden or archived can still be
//! unstarred; that is also why these routes are not under `/api/docs/{id}`.

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
};
use knot_storage::{AddOutcome, FAVORITES_LIMIT};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::{doc_archived, json_err, json_err_with_details};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/me/favorites", get(list))
        .route("/api/me/favorites/{doc_id}", put(add).delete(remove))
}

#[derive(Serialize)]
struct FavoriteRow {
    doc_id: Uuid,
    title: String,
    icon: Option<String>,
    favorited_at: String,
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

async fn list(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(favorites), Some(acl)) = (state.favorites.clone(), state.acl.clone()) else {
        return internal();
    };

    // Pages the caller can't read are left out in SQL. A cached snapshot
    // may predate a page and miss it from the hidden set, so the rows are
    // checked again against one that covers them.
    let hidden_ids = match acl.visibility(ctx.workspace_id, ctx.user_id).await {
        Ok(visibility) => visibility.hidden_ids(),
        Err(e) => {
            tracing::error!(error=?e, "favorites visibility");
            return internal();
        }
    };
    let rows = match favorites
        .list(ctx.user_id, ctx.workspace_id, &hidden_ids)
        .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error=?e, "favorites list");
            return internal();
        }
    };
    let listed_doc_ids: Vec<Uuid> = rows.iter().map(|r| r.doc_id).collect();
    let visibility = match acl
        .visibility_covering(ctx.workspace_id, ctx.user_id, &listed_doc_ids)
        .await
    {
        Ok(visibility) => visibility,
        Err(e) => {
            tracing::error!(error=?e, "favorites visibility");
            return internal();
        }
    };

    let items: Vec<FavoriteRow> = rows
        .into_iter()
        .filter(|r| visibility.readable(r.doc_id))
        .map(|r| FavoriteRow {
            doc_id: r.doc_id,
            title: r.title,
            icon: r.icon,
            favorited_at: r.created_at.to_rfc3339(),
        })
        .collect();
    Json(items).into_response()
}

async fn add(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(favorites), Some(acl), Some(docs)) = (
        state.favorites.clone(),
        state.acl.clone(),
        state.docs.clone(),
    ) else {
        return internal();
    };

    // Same codes, same order as `require_doc_role_mw`.
    let access = match acl.access(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(Some(access)) => access,
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(e) => {
            tracing::error!(error=?e, "favorites acl");
            return internal();
        }
    };
    if !access.can_read() {
        return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
    }
    if access.archived {
        return doc_archived();
    }

    // The ACL said the doc exists in this workspace; a purge racing this
    // request reads as unknown.
    match docs.get(doc_id).await {
        Ok(Some(doc)) if doc.workspace_id == ctx.workspace_id => {
            if doc.is_template {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "favorite.not_a_page", "");
            }
        }
        Ok(_) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(e) => {
            tracing::error!(error=?e, "favorites doc");
            return internal();
        }
    }

    match favorites.add(ctx.user_id, ctx.workspace_id, doc_id).await {
        Ok(AddOutcome::Added | AddOutcome::AlreadyThere) => StatusCode::NO_CONTENT.into_response(),
        Ok(AddOutcome::LimitReached) => json_err_with_details(
            StatusCode::CONFLICT,
            "favorite.limit_reached",
            "",
            json!({ "limit": FAVORITES_LIMIT }),
        ),
        Err(e) => {
            tracing::error!(error=?e, "favorites add");
            internal()
        }
    }
}

async fn remove(State(state): State<AppState>, Path(doc_id): Path<Uuid>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(favorites) = state.favorites.clone() else {
        return internal();
    };
    match favorites
        .remove(ctx.user_id, ctx.workspace_id, doc_id)
        .await
    {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "favorites remove");
            internal()
        }
    }
}
