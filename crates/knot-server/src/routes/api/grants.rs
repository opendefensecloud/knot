//! Document grants API:
//! - GET    /api/docs/{id}/grants
//! - PUT    /api/docs/{id}/grants/{principal}   body: {role, inherit}
//! - DELETE /api/docs/{id}/grants/{principal}
//! - GET    /api/docs/{id}/access   who can see the doc, and through which grant
//!
//! Mounted into the docs router so the shared `require_doc_role_mw` layer
//! covers these routes too.

use std::collections::HashMap;

use axum::{
    Json,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use knot_storage::{GrantStoreError, WorkspaceRole};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::AppState;
use crate::auth::{AuthContext, EffectiveDocRole};
use crate::http_error::json_err;

#[derive(Serialize)]
struct GrantResponse {
    principal: String,
    role: String,
    inherit: bool,
}

pub(super) async fn list_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    if req.extensions().get::<AuthContext>().is_none() {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    }
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    }
    let Some(grants) = state.grants.clone() else {
        return internal();
    };
    match grants.list(doc_id).await {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|g| GrantResponse {
                    principal: g.principal,
                    role: g.role.as_str().into(),
                    inherit: g.inherit,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => {
            tracing::error!(error=?e, "grants list");
            internal()
        }
    }
}

#[derive(Deserialize)]
struct PutGrantRequest {
    role: String,
    inherit: bool,
}

pub(super) async fn put_inline(
    State(state): State<AppState>,
    Path((doc_id, principal)): Path<(Uuid, String)>,
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
    let Ok(body) = read_json::<PutGrantRequest>(req).await else {
        return json_err(StatusCode::BAD_REQUEST, "bad_request", "");
    };
    let Some(new_role) = WorkspaceRole::parse(&body.role) else {
        return json_err(StatusCode::UNPROCESSABLE_ENTITY, "grant.invalid_role", "");
    };
    if !is_valid_principal(&principal) {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "grant.invalid_principal",
            "",
        );
    }
    // `group:` principals pass the format check but ACL resolution only honors
    // `user:` grants — a group grant would silently grant nothing. Reject at
    // creation rather than store a no-op. (delete still accepts them so any
    // legacy group grant can be removed.)
    if principal.starts_with("group:") {
        return json_err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "grant.group_unsupported",
            "group grants are not supported; grant individual users",
        );
    }
    let Some(grants) = state.grants.clone() else {
        return internal();
    };
    match grants
        .put(
            ctx.workspace_id,
            doc_id,
            &principal,
            new_role,
            body.inherit,
            ctx.user_id,
        )
        .await
    {
        Ok(()) => {
            // The invalidation evicts the subtree on every replica,
            // asynchronously; evict here too, so the grantee's next request
            // to this replica sees the grant.
            crate::archive::evict_local_subtree(&state, doc_id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(GrantStoreError::LastOwner) => last_owner(),
        // Grants of people outside the workspace give nothing (ACL
        // resolution requires membership), and would come back to life on a
        // re-invite. The store checks inside the grant's transaction, so a
        // concurrent removal can't slip in between.
        Err(GrantStoreError::NotMember) => {
            json_err(StatusCode::UNPROCESSABLE_ENTITY, "grant.not_member", "")
        }
        Err(e) => {
            tracing::error!(error=?e, "grants put");
            internal()
        }
    }
}

pub(super) async fn delete_inline(
    State(state): State<AppState>,
    Path((doc_id, principal)): Path<(Uuid, String)>,
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
    let Some(grants) = state.grants.clone() else {
        return internal();
    };
    match grants
        .delete(ctx.workspace_id, doc_id, &principal, ctx.user_id)
        .await
    {
        Ok(()) => {
            // As for put: this replica stops serving the revoked access now.
            crate::archive::evict_local_subtree(&state, doc_id).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Err(GrantStoreError::LastOwner) => last_owner(),
        Err(e) => {
            tracing::error!(error=?e, "grants delete");
            internal()
        }
    }
}

/// `GET /api/docs/{id}/access`: what the permissions dialog shows.
#[derive(Serialize)]
struct AccessResponse {
    /// The doc itself is a restriction boundary.
    restricted: bool,
    /// The nearest boundary, the doc or an ancestor; with its title. Both
    /// `null` outside a restricted subtree, and when the caller can't read
    /// the boundary.
    restriction_root_id: Option<String>,
    restriction_root_title: Option<String>,
    people: Vec<AccessPerson>,
}

/// A grant that applies to the doc.
#[derive(Serialize)]
struct AccessPerson {
    principal: String,
    role: String,
    inherit: bool,
    /// The doc the grant is on (this one, or an ancestor it inherits from),
    /// and its title. `null` when the caller can't read that doc.
    source_doc_id: Option<String>,
    source_title: Option<String>,
}

/// The grants that apply to the doc: its own, then the inheriting grants
/// of its ancestors, nearest first. Inside a restricted subtree the walk
/// stops at the boundary, whose own grants are the last ones counted.
/// Any reader may ask; docs they can't read are not named.
pub(super) async fn access_inline(
    State(state): State<AppState>,
    Path(doc_id): Path<Uuid>,
    req: Request,
) -> Response {
    let Some(ctx) = req.extensions().get::<AuthContext>().cloned() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth.session_required", "");
    };
    if req.extensions().get::<EffectiveDocRole>().is_none() {
        return json_err(StatusCode::FORBIDDEN, "acl.no_grant", "");
    }
    let (Some(grants), Some(docs), Some(acl)) =
        (state.grants.clone(), state.docs.clone(), state.acl.clone())
    else {
        return internal();
    };
    // An empty principal holds no grants: this loads only the chain.
    let chain = match grants
        .chain_for_principal(ctx.workspace_id, doc_id, "")
        .await
    {
        Ok((chain, _)) => chain,
        Err(e) => {
            tracing::error!(error=?e, "access chain");
            return internal();
        }
    };
    // Depth 0 is the doc; the chain comes root first.
    let depth_of: HashMap<Uuid, usize> = chain
        .iter()
        .rev()
        .enumerate()
        .map(|(depth, node)| (node.id, depth))
        .collect();
    let Some(doc_node) = chain.last() else {
        return json_err(StatusCode::NOT_FOUND, "doc.not_found", "");
    };
    let boundary = chain.iter().rev().find(|node| node.restricted);
    let deepest_counted = boundary.map_or(usize::MAX, |node| depth_of[&node.id]);
    let mut applying: Vec<(usize, knot_storage::Grant)> =
        match grants.list_inherited(ctx.workspace_id, doc_id).await {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|grant| {
                    let depth = *depth_of.get(&grant.doc_id)?;
                    (depth <= deepest_counted).then_some((depth, grant))
                })
                .collect(),
            Err(e) => {
                tracing::error!(error=?e, "access grants");
                return internal();
            }
        };
    applying.sort_by(|(depth_a, grant_a), (depth_b, grant_b)| {
        depth_a
            .cmp(depth_b)
            .then_with(|| grant_a.principal.cmp(&grant_b.principal))
    });

    // Title of each doc named in the answer, when the caller can read it.
    let mut readable_titles: HashMap<Uuid, Option<String>> = HashMap::new();
    let named_ids = boundary
        .map(|node| node.id)
        .into_iter()
        .chain(applying.iter().map(|(_, grant)| grant.doc_id));
    for named_id in named_ids {
        if readable_titles.contains_key(&named_id) {
            continue;
        }
        let title = match acl
            .effective_role(ctx.workspace_id, named_id, ctx.user_id)
            .await
        {
            Ok(Some(_)) => match docs.get(named_id).await {
                Ok(named_doc) => named_doc.map(|named_doc| named_doc.title),
                Err(e) => {
                    tracing::error!(error=?e, "access title");
                    return internal();
                }
            },
            Ok(None) => None,
            Err(e) => {
                tracing::error!(error=?e, "access acl");
                return internal();
            }
        };
        readable_titles.insert(named_id, title);
    }
    let named = |named_id: Uuid| {
        readable_titles
            .get(&named_id)
            .cloned()
            .flatten()
            .map(|title| (named_id.to_string(), title))
            .unzip()
    };
    let (restriction_root_id, restriction_root_title) =
        boundary.map_or((None, None), |node| named(node.id));
    let people = applying
        .into_iter()
        .map(|(_, grant)| {
            let (source_doc_id, source_title) = named(grant.doc_id);
            AccessPerson {
                principal: grant.principal,
                role: grant.role.as_str().into(),
                inherit: grant.inherit,
                source_doc_id,
                source_title,
            }
        })
        .collect();
    Json(AccessResponse {
        restricted: doc_node.restricted,
        restriction_root_id,
        restriction_root_title,
        people,
    })
    .into_response()
}

/// 409 `grant.last_owner`: a restricted doc keeps at least one owner.
fn last_owner() -> Response {
    json_err(
        StatusCode::CONFLICT,
        "grant.last_owner",
        "a restricted page needs at least one owner",
    )
}

fn is_valid_principal(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix("user:") {
        return Uuid::parse_str(rest).is_ok();
    }
    if let Some(rest) = s.strip_prefix("group:") {
        return !rest.is_empty();
    }
    false
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
