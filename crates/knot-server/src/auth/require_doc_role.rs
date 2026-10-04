//! Route-scoped middleware: parses doc_id from path, resolves the caller's
//! effective role via the AclCache, and inserts the role into request
//! extensions for the downstream handler.
//!
//! On success: inserts `EffectiveDocRole(role)` and calls next.
//! On no AuthContext: 401 auth.session_required.
//! On no role (non-member with no grant): 403 acl.no_grant.

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use knot_storage::WorkspaceRole;
use serde::Deserialize;
use uuid::Uuid;

use super::context::AuthContext;
use crate::AppState;
use crate::http_error::json_err;

#[derive(Debug, Clone, Copy)]
pub struct EffectiveDocRole(pub WorkspaceRole);

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
    let role = match acl
        .effective_role(ctx.workspace_id, doc_id, ctx.user_id)
        .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return json_err(StatusCode::FORBIDDEN, "acl.no_grant", ""),
        Err(e) => {
            tracing::error!(error=?e, "acl resolve");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "");
        }
    };
    req.extensions_mut().insert(EffectiveDocRole(role));
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
