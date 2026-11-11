//! Doc byline:
//! GET /api/docs/{doc_id}/contributors → 200 {created_by: {id, display_name,
//!     former}, created_at, contributors_since, contributors: [{user_id,
//!     display_name, former, first_edited_at, last_edited_at}]}
//!
//! Readable by every role that can open the doc — the byline sits under the
//! title for viewers too. A contributor is anyone whose change to the page
//! CONTENT was persisted (see `knot_storage::contributors`); renames, moves,
//! permission changes and comments do not count. Names only, never emails.
//! Every person is shown through `knot_history::history_author_view`, so a
//! former member (anonymized in history, M4) comes back as `former: true`
//! with a null id and name, never their id, email or avatar.

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
    let (Some(contributors), Some(pool)) = (state.contributors.clone(), state.pool.as_ref()) else {
        return internal();
    };
    let b = match contributors.byline(doc_id).await {
        Ok(Some(b)) => b,
        // The ACL layer already resolved the doc, so this is a hard delete
        // racing the request.
        Ok(None) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "contributors byline");
            return internal();
        }
    };
    let ids: Vec<Uuid> = std::iter::once(b.created_by.id)
        .chain(b.contributors.iter().map(|c| c.user_id))
        .collect();
    match knot_history::load_people(pool, &ids).await {
        Ok(people) => Json(knot_history::byline_view(&b, &people)).into_response(),
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "contributors byline people");
            internal()
        }
    }
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
