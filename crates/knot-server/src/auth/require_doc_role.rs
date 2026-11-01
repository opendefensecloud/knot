//! Route-scoped middleware: parses doc_id from path, resolves the caller's
//! effective role via the AclCache, and inserts the role into request
//! extensions for the downstream handler.
//!
//! On success: inserts `EffectiveDocRole(role)` and calls next.
//! On no AuthContext: 401 auth.session_required.
//! On no role (non-member with no grant): 403 acl.no_grant.
//!
//! Archived docs (#37): viewers get 404 doc.not_found; for everyone else
//! the doc is read-only, so any non-GET request is 409 doc.archived except
//! the restore and purge routes. Handlers behind this layer need no archive
//! check of their own.

use axum::{
    body::Body,
    extract::{MatchedPath, Path, Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::Response,
};
use knot_storage::WorkspaceRole;
use serde::Deserialize;
use uuid::Uuid;

use super::context::AuthContext;
use crate::AppState;
use crate::http_error::{doc_archived, json_err};

#[derive(Debug, Clone, Copy)]
pub struct EffectiveDocRole(pub WorkspaceRole);

/// Present in request extensions when the doc is in the archive.
#[derive(Debug, Clone, Copy)]
pub struct DocArchived;

/// The only writes an archived doc accepts.
pub const ARCHIVE_RESTORE_ROUTE: &str = "/api/docs/{id}/restore";
pub const ARCHIVE_PURGE_ROUTE: &str = "/api/docs/{id}/purge";

// Named extractor so axum is happy on routes that carry extra path params
// (e.g. `/api/docs/{id}/grants/{principal}`). A plain `Path<Uuid>` would fail
// with "Expected 1 but got 2" because positional extraction requires the
// arity to match; a struct field looks up by name.
#[derive(Deserialize)]
pub struct DocIdParam {
    id: Uuid,
}

pub async fn require_doc_role_mw(
    State(state): State<AppState>,
    Path(DocIdParam { id: doc_id }): Path<DocIdParam>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    let Some(acl) = state.acl.clone() else {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
    };
    let access = match acl.access(ctx.workspace_id, doc_id, ctx.user_id).await {
        Ok(Some(access)) => access,
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(e) => {
            tracing::error!(error=?e, "acl resolve");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
        }
    };
    if !access.can_read() {
        return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
    }
    if access.archived {
        let route = req
            .extensions()
            .get::<MatchedPath>()
            .map(MatchedPath::as_str);
        let archive_action = matches!(route, Some(ARCHIVE_RESTORE_ROUTE | ARCHIVE_PURGE_ROUTE));
        if !matches!(*req.method(), Method::GET | Method::HEAD) && !archive_action {
            return doc_archived();
        }
        req.extensions_mut().insert(DocArchived);
    }
    req.extensions_mut().insert(EffectiveDocRole(access.role));
    next.run(req).await
}

/// Handler-side gate for doc sub-resources any role may read (Viewer and
/// up). Expects `EffectiveDocRole` already set by `require_doc_role_mw`:
/// 401 `auth.session_required` without a session, 403 `acl.no_grant`
/// without a role on the doc.
pub fn require_viewer(req: &Request<Body>) -> Option<Response> {
    if req.extensions().get::<AuthContext>().is_none() {
        return Some(json_err(
            StatusCode::UNAUTHORIZED,
            "auth.session_required",
            "",
        ));
    }
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return Some(json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""));
    }
    None
}
