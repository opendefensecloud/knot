//! Short page links (`https://<host>/<code>`):
//! POST /api/docs/{id}/short-link → 200 { code, url }   get-or-create (live pages)
//! GET  /api/docs/{id}/short-link → 200 { code, url } | 404 short_link.none
//! GET  /api/short-links/{code}   → 200 { doc_id }    | 404 short_link.not_found
//!
//! The two doc routes are mounted in `docs.rs`, behind `require_doc_role_mw`:
//! any reader may create or read a page's code, and an archived page gets
//! no new code (non-GET → 409 doc.archived). A code grants nothing, so the
//! resolve route checks read access itself and answers every refusal —
//! unknown, malformed, restricted, no grant, archived for a viewer — with
//! the same 404, so a code never reveals that a page exists.

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use knot_storage::{ShortLink, is_short_code};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

#[derive(serde::Serialize)]
struct LinkResponse {
    code: String,
    url: String,
}

#[derive(serde::Serialize)]
struct ResolveResponse {
    doc_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/api/short-links/{code}", get(resolve))
}

fn link_response(state: &AppState, link: ShortLink) -> Response {
    let url = format!("{}/{}", state.base_url.trim_end_matches('/'), link.code);
    Json(LinkResponse {
        code: link.code,
        url,
    })
    .into_response()
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

/// The one answer for every code the caller may not follow.
fn not_found() -> Response {
    json_err(StatusCode::NOT_FOUND, "short_link.not_found", "")
}

/// `POST /api/docs/{id}/short-link`. The middleware already checked read
/// access and refused archived pages.
pub async fn get_or_create_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(store) = state.short_links.clone() else {
        return internal();
    };
    match store.get_or_create(doc_id, ctx.user_id).await {
        Ok(link) => link_response(&state, link),
        Err(e) => {
            tracing::error!(error=?e, "short link create");
            internal()
        }
    }
}

/// `GET /api/docs/{id}/short-link`: the page's code, never created here.
pub async fn get_inline(State(state): State<AppState>, Path(doc_id): Path<Uuid>) -> Response {
    let Some(store) = state.short_links.clone() else {
        return internal();
    };
    match store.find_for_doc(doc_id).await {
        Ok(Some(link)) => link_response(&state, link),
        Ok(None) => json_err(StatusCode::NOT_FOUND, "short_link.none", ""),
        Err(e) => {
            tracing::error!(error=?e, "short link get");
            internal()
        }
    }
}

async fn resolve(
    State(state): State<AppState>,
    Path(code): Path<String>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(store), Some(acl)) = (state.short_links.clone(), state.acl.clone()) else {
        return internal();
    };
    if !is_short_code(&code) {
        return not_found();
    }
    let link = match store.resolve(&code).await {
        Ok(Some(link)) => link,
        Ok(None) => return not_found(),
        Err(e) => {
            tracing::error!(error=?e, "short link resolve");
            return internal();
        }
    };
    // `can_read` is false for restricted pages without a grant and for a
    // viewer on an archived page, the same rule `/api/docs/{id}` applies.
    match acl.access(ctx.workspace_id, link.doc_id, ctx.user_id).await {
        Ok(Some(access)) if access.can_read() => Json(ResolveResponse {
            doc_id: link.doc_id.to_string(),
        })
        .into_response(),
        Ok(_) => not_found(),
        Err(e) => {
            tracing::error!(error=?e, "short link acl");
            internal()
        }
    }
}
