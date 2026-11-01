//! Restricted documents (private subtrees): the rules shared by the routes
//! that put a page somewhere, such as create, from-template, import, move
//! and duplicate. See
//! docs/superpowers/specs/2026-10-30-restricted-documents-design.md.

use std::collections::HashSet;

use axum::{http::StatusCode, response::Response};
use knot_docs::rule::{self, AclNode, PrincipalGrant};
use knot_storage::{GrantStoreError, WorkspaceRole};
use uuid::Uuid;

use crate::AppState;
use crate::auth::AuthContext;
use crate::http_error::{doc_archived, json_err};

/// A parent named in a request body, which the caller must be able to edit
/// under. `404 doc.not_found` when it is missing, in another workspace, or
/// unreadable to the caller, so a restricted parent answers like an id no
/// page has. `403 acl.editor_required` when the caller only views it;
/// `409 doc.archived` when it is in the archive.
pub async fn require_editable_parent(
    state: &AppState,
    ctx: &AuthContext,
    parent_id: Uuid,
) -> Result<(), Response> {
    let Some(acl) = state.acl.as_ref() else {
        return Err(internal());
    };
    let access = match acl.access(ctx.workspace_id, parent_id, ctx.user_id).await {
        Ok(Some(access)) if access.can_read() => access,
        Ok(_) => return Err(json_err(StatusCode::NOT_FOUND, "doc.not_found", "")),
        Err(e) => {
            tracing::error!(error=?e, %parent_id, "parent acl");
            return Err(internal());
        }
    };
    if !matches!(access.role, WorkspaceRole::Owner | WorkspaceRole::Editor) {
        return Err(json_err(StatusCode::FORBIDDEN, "acl.editor_required", ""));
    }
    if access.archived {
        return Err(doc_archived());
    }
    Ok(())
}

/// Placement anchors (`after_id`, `before_id`) named in a request body. One
/// the caller can't read answers `422 doc.bad_anchor`, as an id no sibling
/// has does, so placing a page next to it can't tell whether it exists.
pub async fn require_readable_anchors(
    state: &AppState,
    ctx: &AuthContext,
    anchors: &[Option<Uuid>],
) -> Result<(), Response> {
    let Some(acl) = state.acl.as_ref() else {
        return Err(internal());
    };
    for anchor_id in anchors.iter().flatten() {
        match acl
            .effective_role(ctx.workspace_id, *anchor_id, ctx.user_id)
            .await
        {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(json_err(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "doc.bad_anchor",
                    "",
                ));
            }
            Err(e) => {
                tracing::error!(error=?e, %anchor_id, "anchor acl");
                return Err(internal());
            }
        }
    }
    Ok(())
}

/// After `creator` created `doc_id`: when they can't edit it, give them an
/// inheriting `owner` grant on it. That happens inside a restricted subtree
/// when their access to the parent came from a grant that doesn't inherit;
/// without it they would lose the page they just made.
///
/// Only a creator who can still edit the parent gets the grant. The route
/// checked the parent before the create, through the cache; a restriction,
/// grant revocation or role change since then would otherwise turn that
/// lost access into an owner foothold inside the subtree. So both answers
/// come from one fresh snapshot of the chain, the creator's grants on it
/// and their workspace role (at the top level, the role is the parent).
/// When the parent is no longer editable, the page stays where it was made
/// without the grant, as if it had been made just before the change.
pub async fn ensure_creator_can_edit(
    state: &AppState,
    workspace_id: Uuid,
    doc_id: Uuid,
    creator: Uuid,
) -> Result<(), Response> {
    let Some(grants) = state.grants.as_ref() else {
        return Err(internal());
    };
    let (chain, creator_grants, workspace_role) = match grants
        .chain_with_member_role(workspace_id, doc_id, creator)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "creator acl");
            return Err(internal());
        }
    };
    // A parent cycle repeats ids; give nothing there, as `acl::resolve` does.
    let mut seen_ids = HashSet::with_capacity(chain.len());
    if !chain.iter().all(|node| seen_ids.insert(node.id)) {
        return Ok(());
    }
    let path: Vec<AclNode> = chain
        .iter()
        .map(|node| AclNode {
            id: node.id,
            parent_id: node.parent_id,
            restricted: node.restricted,
            archived: node.archived,
        })
        .collect();
    let creator_grants: Vec<PrincipalGrant> = creator_grants
        .iter()
        .map(|grant| PrincipalGrant {
            doc_id: grant.doc_id,
            role: grant.role,
            inherit: grant.inherit,
        })
        .collect();
    let can_write = |path: &[AclNode]| {
        rule::access_along_path(path, &creator_grants, workspace_role)
            .is_some_and(|access| access.can_write())
    };
    // An empty chain: the doc is gone already.
    let Some((_, parent_path)) = path.split_last() else {
        return Ok(());
    };
    if can_write(&path) {
        return Ok(());
    }
    let can_edit_parent = if parent_path.is_empty() {
        matches!(
            workspace_role,
            Some(WorkspaceRole::Owner | WorkspaceRole::Editor)
        )
    } else {
        can_write(parent_path)
    };
    if !can_edit_parent {
        return Ok(());
    }
    match grants
        .put(
            workspace_id,
            doc_id,
            &format!("user:{creator}"),
            WorkspaceRole::Owner,
            true,
            creator,
        )
        .await
    {
        Ok(()) => {}
        // Removed from the workspace since the snapshot: no page to keep.
        Err(GrantStoreError::NotMember) => return Ok(()),
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "creator owner grant");
            return Err(internal());
        }
    }
    // An access to the new doc looked up meanwhile is cached; read the
    // grant from now on.
    crate::archive::evict_local(state, &[doc_id]).await;
    Ok(())
}

fn internal() -> Response {
    json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", "")
}
