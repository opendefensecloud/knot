//! The per-user notification inbox.
//!
//! GET  /api/notifications?filter=unread|all&limit=&cursor=  → { items, next_cursor }
//! GET  /api/notifications/unread_count                      → { count, capped }
//! POST /api/notifications/read  { ids: [] } | { all: true } → 204
//!
//! Rows are addressed to exactly one recipient, so ownership needs no join.
//! Document *access* is a separate question: a notification can outlive the
//! grant that made it visible (a page restricted, archived or unshared
//! since), so the list and the count both leave out rows about docs the
//! caller can't read now. Filtered, never 403'd.

use axum::{
    Json, Router,
    body::Body,
    extract::{Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::json_err;

const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 100;
const UNREAD_CAP: i64 = 100;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/notifications", get(list))
        .route("/api/notifications/unread_count", get(unread_count))
        .route("/api/notifications/read", post(mark_read))
}

#[derive(Deserialize)]
struct ListQuery {
    #[serde(default)]
    filter: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    cursor: Option<i64>,
}

#[derive(Serialize)]
struct NotificationRow {
    id: i64,
    kind: String,
    doc_id: Option<String>,
    doc_title: Option<String>,
    target_kind: String,
    target_id: String,
    actor_display_name: Option<String>,
    data: serde_json::Value,
    created_at: String,
    read: bool,
}

#[derive(Serialize)]
struct ListResponse {
    items: Vec<NotificationRow>,
    next_cursor: Option<i64>,
}

#[derive(Serialize)]
struct CountResponse {
    count: i64,
    capped: bool,
}

#[derive(Deserialize)]
struct ReadBody {
    #[serde(default)]
    ids: Vec<i64>,
    #[serde(default)]
    all: bool,
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

async fn list(State(state): State<AppState>, Query(q): Query<ListQuery>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(notifications), Some(acl)) = (state.notifications.clone(), state.acl.clone()) else {
        return internal();
    };
    let unread_only = q.filter.as_deref() == Some("unread");
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    // Rows about docs the caller can't read are left out in SQL, so a page
    // isn't cut short by them. A cached snapshot may predate a doc and miss
    // it from the hidden set, so the rows are checked again against one
    // that covers their docs.
    let hidden_ids = match acl.visibility(ctx.workspace_id, ctx.user_id).await {
        Ok(visibility) => visibility.hidden_ids(),
        Err(e) => {
            tracing::error!(error=?e, "notifications visibility");
            return internal();
        }
    };
    let rows = match notifications
        .list(
            ctx.user_id,
            ctx.workspace_id,
            &hidden_ids,
            unread_only,
            limit,
            q.cursor,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error=?e, "notifications list");
            return internal();
        }
    };
    let next_cursor = if rows.len() as i64 == limit {
        rows.last().map(|r| r.id)
    } else {
        None
    };
    let listed_doc_ids: Vec<Uuid> = rows.iter().filter_map(|r| r.doc_id).collect();
    let visibility = match acl
        .visibility_covering(ctx.workspace_id, ctx.user_id, &listed_doc_ids)
        .await
    {
        Ok(visibility) => visibility,
        Err(e) => {
            tracing::error!(error=?e, "notifications visibility");
            return internal();
        }
    };

    let mut items = Vec::with_capacity(rows.len());
    for r in rows {
        if r.doc_id.is_some_and(|doc_id| !visibility.readable(doc_id)) {
            continue;
        }
        items.push(NotificationRow {
            id: r.id,
            kind: r.kind,
            doc_id: r.doc_id.map(|d| d.to_string()),
            doc_title: r.doc_title,
            target_kind: r.target_kind,
            target_id: r.target_id,
            actor_display_name: r.actor_display_name,
            data: r.data,
            created_at: r.created_at.to_rfc3339(),
            read: r.read_at.is_some(),
        });
    }

    Json(ListResponse { items, next_cursor }).into_response()
}

/// The badge: unread rows, counted as `list` lists them.
async fn unread_count(State(state): State<AppState>, req: Request) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let (Some(notifications), Some(acl)) = (state.notifications.clone(), state.acl.clone()) else {
        return internal();
    };
    let hidden_ids = match acl.visibility(ctx.workspace_id, ctx.user_id).await {
        Ok(visibility) => visibility.hidden_ids(),
        Err(e) => {
            tracing::error!(error=?e, "notifications visibility");
            return internal();
        }
    };
    match notifications
        .unread_count(ctx.user_id, ctx.workspace_id, &hidden_ids, UNREAD_CAP)
        .await
    {
        Ok(n) => Json(CountResponse {
            count: n,
            capped: is_capped(n, UNREAD_CAP),
        })
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "notifications unread_count");
            internal()
        }
    }
}

/// Whether the badge should show the "+" suffix: `n` hit the `LIMIT` the
/// store's `unread_count` query imposes, so the true count might be
/// higher. Pulled out of the handler so the `true` branch has direct unit
/// coverage — exercising it through the HTTP stack would mean actually
/// creating `UNREAD_CAP` (100) rows for a user, which no existing test does.
fn is_capped(n: i64, cap: i64) -> bool {
    n >= cap
}

async fn mark_read(State(state): State<AppState>, req: Request<Body>) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(notifications) = state.notifications.clone() else {
        return internal();
    };
    let bytes = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::PAYLOAD_TOO_LARGE, "bad_request", ""),
    };
    let body: ReadBody = match serde_json::from_slice(&bytes) {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "bad_request", ""),
    };

    let res = if body.all {
        notifications.mark_all_read(ctx.user_id).await
    } else {
        notifications.mark_read(ctx.user_id, &body.ids).await
    };
    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error=?e, "notifications mark_read");
            internal()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_capped;

    #[test]
    fn not_capped_below_the_limit() {
        assert!(!is_capped(4, 5));
    }

    #[test]
    fn capped_at_and_above_the_limit() {
        assert!(is_capped(5, 5), "count == cap must already read as capped");
        assert!(is_capped(6, 5));
    }
}
