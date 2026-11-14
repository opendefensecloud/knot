//! Page audit log:
//! - GET /api/docs/{id}/audit?category=&cursor=&limit=  one page of the
//!   page's audit events, newest first, and on the first page (no cursor)
//!   its export stats
//!
//! Behind `require_doc_role_mw`; editors and owners of the page (by their
//! effective role on it, never the workspace role) only, also on an
//! archived page. Never cached. The events are `audit_events` rows on the
//! page (`target_kind = 'doc'`); each is shown with a whitelisted `details`
//! object, never its raw `data`, and every person goes through
//! `knot_history::history_author_view`, so an anonymized person is not
//! re-identified and no email is ever sent; a bot is marked `"bot": true`. Event ids and the cursor are
//! opaque tokens ([`EventTokens`]), never the stored ids. See
//! docs/superpowers/specs/2026-11-13-doc-audit-page-design.md.

mod event_token;
mod render;

pub use event_token::EventTokens;

use std::collections::{HashMap, HashSet};

use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use knot_storage::audit::{self, DocAuditRow};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::AppState;
use crate::auth::{ApiPrincipal, AuthContext, require_editor};
use crate::http_error::json_err;
use crate::routes::api::history::readable_sources;

/// Events per page when the query names no limit.
pub const DEFAULT_LIMIT: i64 = 50;
/// The largest `limit` a query may ask for.
pub const MAX_LIMIT: i64 = 200;

/// The `doc.export` action: one row per page whose content left in a file.
pub const EXPORT_ACTION: &str = "doc.export";

/// The `doc.export` data key that holds the id of the API key the export
/// was made with. For operators, like `export_id`: the row's actor stays
/// the key's user, and the log shows only `details.via_api_key`.
pub const API_KEY_ID_FIELD: &str = "api_key_id";

/// The id of the API key that authenticated `req`, `None` for a cookie
/// session. Read it before the handler consumes the request's body.
pub fn api_key_id_of(req: &Request) -> Option<Uuid> {
    req.extensions()
        .get::<ApiPrincipal>()
        .map(|principal| principal.key_id)
}

/// Adds [`API_KEY_ID_FIELD`] to an export's audit `data` (a JSON object)
/// when it was made with an API key.
pub fn with_api_key_id(
    mut audit_data: serde_json::Value,
    api_key_id: Option<Uuid>,
) -> serde_json::Value {
    if let (Some(api_key_id), Some(fields)) = (api_key_id, audit_data.as_object_mut()) {
        fields.insert(API_KEY_ID_FIELD.into(), api_key_id.to_string().into());
    }
    audit_data
}

/// Each event category the log filters by, with the actions in it. An
/// action in none of them (a future one) shows under `all` as `other`.
pub const CATEGORIES: &[(&str, &[&str])] = &[
    ("exports", &[EXPORT_ACTION]),
    (
        "access",
        &[
            "doc.grant",
            "doc.grant.delete",
            "admin.grant",
            "doc.restrict",
            "doc.unrestrict",
        ],
    ),
    (
        "changes",
        &[
            "doc.create",
            "doc.rename",
            "doc.move",
            "doc.duplicate",
            "doc.archive",
            "doc.restore",
            "doc.purge",
            "doc.template.mark",
            "doc.template.unmark",
        ],
    ),
];

/// The category `action` is shown under: one of [`CATEGORIES`], or
/// `other`.
pub fn category_of(action: &str) -> &'static str {
    CATEGORIES
        .iter()
        .find(|(_, actions)| actions.contains(&action))
        .map_or("other", |(category, _)| category)
}

/// The actions of the category `?category=` names: `None` for `all` (or no
/// value), `Err` for a name that is no category.
fn category_actions(category: Option<&str>) -> Result<Option<&'static [&'static str]>, ()> {
    match category {
        None | Some("all") => Ok(None),
        Some(name) => CATEGORIES
            .iter()
            .find(|(category, _)| *category == name)
            .map(|(_, actions)| Some(*actions))
            .ok_or(()),
    }
}

/// `?category=&cursor=&limit=`, parsed in the handler so a bad value is a
/// JSON 400 (axum's `Query` answers a non-number in plain text).
#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    category: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

/// A parsed [`AuditQuery`].
#[derive(Debug, PartialEq)]
struct AuditRequest {
    actions: Option<&'static [&'static str]>,
    /// Only events with a lower id: the stored id of the last event already
    /// shown, opened from the `cursor` token.
    before: Option<i64>,
    limit: i64,
}

/// `query` checked, its cursor opened with `tokens`: an empty value is no
/// value; `Err` carries the message of the 400 (`bad category`, `bad
/// cursor` or `bad limit`).
fn parse_query(query: AuditQuery, tokens: &EventTokens) -> Result<AuditRequest, &'static str> {
    let given = |value: Option<String>| value.filter(|text| !text.is_empty());
    let actions = category_actions(given(query.category).as_deref()).map_err(|()| "category")?;
    let before = match given(query.cursor) {
        None => None,
        Some(text) => match tokens.open(&text) {
            Some(cursor) => Some(cursor),
            None => return Err("cursor"),
        },
    };
    let limit = match given(query.limit) {
        None => DEFAULT_LIMIT,
        Some(text) => match text.parse::<i64>() {
            Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => limit,
            _ => return Err("limit"),
        },
    };
    Ok(AuditRequest {
        actions,
        before,
        limit,
    })
}

/// `GET /api/docs/{id}/audit?category=&cursor=&limit=`.
pub async fn doc_audit(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    Query(query): Query<AuditQuery>,
    req: Request,
) -> Response {
    if let Some(refusal) = require_editor(&req) {
        return refusal;
    }
    // Present: require_editor refused the request otherwise.
    let Some(auth_context) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let tokens = EventTokens::new(&state.session_key, doc_id);
    let audit_request = match parse_query(query, &tokens) {
        Ok(audit_request) => audit_request,
        Err(what) => return bad_request(what),
    };
    let Some(pool) = state.pool.as_ref() else {
        return internal();
    };
    let workspace_id = auth_context.workspace_id;

    let mut rows = match audit::list_doc_events(
        pool,
        workspace_id,
        doc_id,
        audit_request.actions,
        audit_request.before,
        audit_request.limit + 1,
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = ?e, %doc_id, "audit events");
            return internal();
        }
    };
    let next_cursor = if rows.len() as i64 > audit_request.limit {
        rows.truncate(audit_request.limit as usize);
        rows.last().map(|row| tokens.seal(row.id))
    } else {
        None
    };
    // Stats only on the first page, always over every category.
    let stats = if audit_request.before.is_none() {
        match audit::doc_stats(pool, workspace_id, doc_id).await {
            Ok(stats) => Some(stats),
            Err(e) => {
                tracing::error!(error = ?e, %doc_id, "audit stats");
                return internal();
            }
        }
    } else {
        None
    };

    let person_ids = render::referenced_people(&rows, stats.as_ref());
    // `load_people` is the page history's; which of them are bots is read
    // here, so the history's people and views stay as they are.
    let people = match tokio::try_join!(
        knot_history::load_people(pool, &person_ids),
        audit::bots_among(pool, &person_ids),
    ) {
        Ok((people, bots)) => render::AuditPeople { people, bots },
        Err(e) => {
            tracing::error!(error = ?e, %doc_id, "audit people");
            return internal();
        }
    };
    let roots = readable_root_titles(&state, &auth_context, doc_id, &rows).await;

    let events: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| render::event_view(row, doc_id, &tokens, &people, &roots))
        .collect();
    let stats_view = stats
        .as_ref()
        .map(|stats| render::stats_view(stats, &people));
    (
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(json!({
            "events": events,
            "next_cursor": next_cursor,
            "stats": stats_view,
        })),
    )
        .into_response()
}

/// The titles of the subtree export roots named by `rows` (other than the
/// page itself) that the reader may read. A root they cannot read, or one
/// that is gone, is absent, and its rows show `root_hidden`.
async fn readable_root_titles(
    state: &AppState,
    auth_context: &AuthContext,
    doc_id: Uuid,
    rows: &[DocAuditRow],
) -> HashMap<Uuid, String> {
    let root_ids: Vec<Uuid> = rows
        .iter()
        .filter_map(|row| render::subtree_root(row, doc_id))
        .collect::<HashSet<Uuid>>()
        .into_iter()
        .collect();
    if root_ids.is_empty() {
        return HashMap::new();
    }
    let readable = readable_sources(
        &root_ids,
        state
            .acl
            .as_ref()
            .map(|acl| move |id| acl.access(auth_context.workspace_id, id, auth_context.user_id)),
    )
    .await;
    let mut titles = HashMap::new();
    let Some(docs) = state.docs.as_ref() else {
        return titles;
    };
    for root_id in readable {
        match docs.get(root_id).await {
            Ok(Some(root)) => {
                titles.insert(root_id, root.title);
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, %root_id, "audit: export root title, hidden");
            }
        }
    }
    titles
}

fn bad_request(what: &str) -> Response {
    json_err(
        StatusCode::BAD_REQUEST,
        "audit.bad_request",
        &format!("bad {what}"),
    )
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every audit action the codebase writes on a page (`target_kind =
    /// 'doc'`). A new writer adds its action here and to a category.
    const DOC_ACTIONS_WRITTEN: &[&str] = &[
        "doc.create",
        "doc.rename",
        "doc.move",
        "doc.duplicate",
        "doc.archive",
        "doc.restore",
        "doc.purge",
        "doc.template.mark",
        "doc.template.unmark",
        "doc.restrict",
        "doc.unrestrict",
        "doc.grant",
        "doc.grant.delete",
        "admin.grant",
        "doc.export",
    ];

    #[test]
    fn every_written_action_is_in_exactly_one_category() {
        for action in DOC_ACTIONS_WRITTEN {
            let holders: Vec<&str> = CATEGORIES
                .iter()
                .filter(|(_, actions)| actions.contains(action))
                .map(|(category, _)| *category)
                .collect();
            assert_eq!(holders.len(), 1, "{action} is in {holders:?}");
        }
        let listed: usize = CATEGORIES.iter().map(|(_, actions)| actions.len()).sum();
        assert_eq!(
            listed,
            DOC_ACTIONS_WRITTEN.len(),
            "a category lists an action nobody writes"
        );
        assert_eq!(category_of("doc.export"), "exports");
        assert_eq!(category_of("admin.grant"), "access");
        assert_eq!(category_of("doc.purge"), "changes");
        assert_eq!(category_of("doc.someday"), "other");
    }

    fn query(category: Option<&str>, cursor: Option<&str>, limit: Option<&str>) -> AuditQuery {
        AuditQuery {
            category: category.map(str::to_owned),
            cursor: cursor.map(str::to_owned),
            limit: limit.map(str::to_owned),
        }
    }

    #[test]
    fn the_query_is_checked() {
        let tokens = EventTokens::new(b"test-key", Uuid::new_v4());
        let parse_query = |query| super::parse_query(query, &tokens);
        let cursor = tokens.seal(12);
        assert_eq!(
            parse_query(query(None, None, None)),
            Ok(AuditRequest {
                actions: None,
                before: None,
                limit: DEFAULT_LIMIT
            })
        );
        assert_eq!(
            parse_query(query(Some(""), Some(""), Some(""))),
            Ok(AuditRequest {
                actions: None,
                before: None,
                limit: DEFAULT_LIMIT
            })
        );
        assert_eq!(
            parse_query(query(Some("exports"), Some(&cursor), Some("200"))),
            Ok(AuditRequest {
                actions: Some(&[EXPORT_ACTION][..]),
                before: Some(12),
                limit: 200
            })
        );
        assert_eq!(
            parse_query(query(Some("all"), None, Some("1")))
                .unwrap()
                .actions,
            None
        );
        for (bad, what) in [
            (query(Some("foo"), None, None), "category"),
            (query(None, Some("abc"), None), "cursor"),
            (query(None, Some("12"), None), "cursor"),
            (query(None, Some("0"), None), "cursor"),
            (query(None, Some("-3"), None), "cursor"),
            (query(None, None, Some("0")), "limit"),
            (query(None, None, Some("201")), "limit"),
            (query(None, None, Some("ten")), "limit"),
        ] {
            assert_eq!(parse_query(bad), Err(what));
        }
    }
}
