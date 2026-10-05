//! Doc byline:
//! GET /api/docs/{doc_id}/contributors → 200 {created_by, created_at,
//!     contributors_since, contributors: [{user_id, display_name,
//!     first_edited_at, last_edited_at}]}
//!
//! Readable by every role that can open the doc — the byline sits under the
//! title for viewers too. A contributor is anyone whose change to the page
//! CONTENT was persisted (see `knot_storage::contributors`); renames, moves,
//! permission changes and comments do not count. Names only, never emails.

use axum::{
    Json,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use uuid::Uuid;

use crate::AppState;
use crate::auth::require_viewer;
use crate::http_error::json_err;

pub async fn byline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if let Some(r) = require_viewer(&req) {
        return r;
    }
    let Some(contributors) = state.contributors.clone() else {
        return internal();
    };
    match contributors.byline(doc_id).await {
        Ok(Some(b)) => Json(b).into_response(),
        // The ACL layer already resolved the doc, so this is a hard delete
        // racing the request.
        Ok(None) => json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "contributors byline");
            internal()
        }
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
