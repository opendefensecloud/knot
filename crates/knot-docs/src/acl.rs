//! Resolve the effective access of one user to one document.
//!
//! Algorithm:
//! 1. Look up the user's workspace role. A non-member gets no access, even
//!    with grants left behind on the doc or its ancestors.
//! 2. Load the doc's ancestor chain, root first, together with the user's
//!    grants on it via `GrantStore::chain_for_principal`.
//! 3. Fold the chain with the restricted-documents rule
//!    (`rule::access_along_path`): outside a restricted subtree the role is
//!    the max of the workspace role, the user's grants on the doc and their
//!    inherit=true grants on ancestors; from a restricted doc downwards only
//!    grants on that boundary and below count.

use std::collections::HashSet;

use knot_storage::{
    DocStore, DocStoreError, GrantStore, GrantStoreError, WorkspaceRole, WorkspaceStore,
    WorkspaceStoreError,
};
use thiserror::Error;
use uuid::Uuid;

use crate::rule::{self, AclNode, PrincipalGrant};

pub type EffectiveRole = WorkspaceRole;

/// What a user may do with one doc: their effective role plus whether the
/// doc is in the archive. Archived docs are read-only for editors and
/// owners and invisible to viewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocAccess {
    pub role: WorkspaceRole,
    pub archived: bool,
    /// The nearest restricted ancestor-or-self: `Some` exactly when the doc
    /// lies in a restricted subtree.
    pub restriction_root: Option<Uuid>,
}

impl DocAccess {
    /// Viewers lose sight of a doc once it is archived.
    pub fn can_read(self) -> bool {
        !(self.archived && self.role == WorkspaceRole::Viewer)
    }

    /// Nobody edits an archived doc; it has to be restored first.
    pub fn can_write(self) -> bool {
        !self.archived && matches!(self.role, WorkspaceRole::Owner | WorkspaceRole::Editor)
    }
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("workspace: {0}")]
    Workspace(#[from] WorkspaceStoreError),
    #[error("grants: {0}")]
    Grants(#[from] GrantStoreError),
    #[error("docs: {0}")]
    Docs(#[from] DocStoreError),
}

fn rank(r: WorkspaceRole) -> u8 {
    match r {
        WorkspaceRole::Owner => 3,
        WorkspaceRole::Editor => 2,
        WorkspaceRole::Viewer => 1,
    }
}

/// The higher of two roles (Owner > Editor > Viewer).
pub(crate) fn max(a: WorkspaceRole, b: WorkspaceRole) -> WorkspaceRole {
    if rank(a) >= rank(b) { a } else { b }
}

/// The user's access to `doc_id`, or `None` when they have none.
///
/// `_docs` is no longer read: the chain query loads every flag the rule
/// needs. It stays in the signature so callers do not change.
pub async fn resolve(
    workspaces: &dyn WorkspaceStore,
    grants: &dyn GrantStore,
    _docs: &dyn DocStore,
    workspace_id: Uuid,
    doc_id: Uuid,
    user_id: Uuid,
) -> Result<Option<DocAccess>, ResolveError> {
    // Tenancy guard: the document must belong to the caller's workspace.
    // Without this, `get_member_role(workspace_id, user_id)` returns the
    // caller's role in their OWN workspace for ANY doc_id — granting a member
    // of one workspace read/write access to documents in every other workspace
    // by UUID. The chain query filters every step by `workspace_id`, so a
    // missing or foreign doc yields an empty chain and no access. The cache
    // key is (workspace_id, doc_id, user_id), so a `None` cached for a
    // foreign workspace never shadows the doc's real workspace.
    let principal = format!("user:{user_id}");
    let (chain, principal_grants) = grants
        .chain_for_principal(workspace_id, doc_id, &principal)
        .await?;
    if chain.is_empty() {
        return Ok(None);
    }
    // A parent cycle makes the walk repeat ids until its depth cap. The
    // batched evaluator gives such docs no access, so fail closed here too.
    let mut seen_ids = HashSet::with_capacity(chain.len());
    if !chain.iter().all(|node| seen_ids.insert(node.id)) {
        return Ok(None);
    }
    let workspace_role = workspaces.get_member_role(workspace_id, user_id).await?;
    let path: Vec<AclNode> = chain
        .into_iter()
        .map(|node| AclNode {
            id: node.id,
            parent_id: node.parent_id,
            restricted: node.restricted,
            archived: node.archived,
        })
        .collect();
    let principal_grants: Vec<PrincipalGrant> = principal_grants
        .into_iter()
        .map(|grant| PrincipalGrant {
            doc_id: grant.doc_id,
            role: grant.role,
            inherit: grant.inherit,
        })
        .collect();
    Ok(rule::access_along_path(
        &path,
        &principal_grants,
        workspace_role,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use knot_storage::{
        DocStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    };

    async fn ctx() -> (PgWorkspaceStore, PgGrantStore, PgDocStore, Uuid, Uuid) {
        let (ws_s, gs, ds, ws, user, _pool) = ctx_with_pool().await;
        (ws_s, gs, ds, ws, user)
    }

    async fn ctx_with_pool() -> (
        PgWorkspaceStore,
        PgGrantStore,
        PgDocStore,
        Uuid,
        Uuid,
        sqlx::PgPool,
    ) {
        let pool = knot_test_support::fresh_db().await.pool;

        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool.clone());
        let w = ws_s.create("default", "W").await.unwrap();
        let u = us.create_local("a@x.test", "A", "$h$").await.unwrap();
        ws_s.add_member(w.id, u.id, WorkspaceRole::Viewer)
            .await
            .unwrap();
        (ws_s, gs, ds, w.id, u.id, pool)
    }

    async fn mark_restricted(pool: &sqlx::PgPool, doc_id: Uuid) {
        sqlx::query("UPDATE documents SET restricted = true WHERE id = $1")
            .bind(doc_id)
            .execute(pool)
            .await
            .unwrap();
    }

    #[test]
    fn archived_docs_are_read_only_and_hidden_from_viewers() {
        use WorkspaceRole::{Editor, Owner, Viewer};
        let access = |role, archived| DocAccess {
            role,
            archived,
            restriction_root: None,
        };
        assert!(access(Owner, false).can_write());
        assert!(access(Editor, false).can_write());
        assert!(!access(Viewer, false).can_write());
        assert!(access(Viewer, false).can_read());
        for role in [Owner, Editor, Viewer] {
            assert!(
                !access(role, true).can_write(),
                "{role:?} wrote an archived doc"
            );
        }
        assert!(access(Owner, true).can_read());
        assert!(access(Editor, true).can_read());
        assert!(!access(Viewer, true).can_read());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn archived_doc_reports_archived() {
        let (ws_s, gs, ds, ws, user) = ctx().await;
        let live = ds.create(ws, None, "Live", "m", user).await.unwrap();
        let gone = ds.create(ws, None, "Gone", "n", user).await.unwrap();
        ds.archive(ws, gone.id, user).await.unwrap();

        let live_access = resolve(&ws_s, &gs, &ds, ws, live.id, user).await.unwrap();
        let gone_access = resolve(&ws_s, &gs, &ds, ws, gone.id, user).await.unwrap();

        assert_eq!(
            live_access,
            Some(DocAccess {
                role: WorkspaceRole::Viewer,
                archived: false,
                restriction_root: None,
            })
        );
        assert_eq!(
            gone_access,
            Some(DocAccess {
                role: WorkspaceRole::Viewer,
                archived: true,
                restriction_root: None,
            })
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn workspace_role_used_when_no_grant() {
        let (ws_s, gs, ds, ws, user) = ctx().await;
        let d = ds.create(ws, None, "X", "m", user).await.unwrap();
        let r = resolve(&ws_s, &gs, &ds, ws, d.id, user).await.unwrap();
        assert_eq!(r.map(|a| a.role), Some(WorkspaceRole::Viewer));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn explicit_grant_upgrades_role() {
        let (ws_s, gs, ds, ws, user) = ctx().await;
        let d = ds.create(ws, None, "X", "m", user).await.unwrap();
        gs.put(
            ws,
            d.id,
            &format!("user:{user}"),
            WorkspaceRole::Owner,
            true,
            user,
        )
        .await
        .unwrap();
        let r = resolve(&ws_s, &gs, &ds, ws, d.id, user).await.unwrap();
        assert_eq!(r.map(|a| a.role), Some(WorkspaceRole::Owner));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ancestor_inherit_propagates() {
        let (ws_s, gs, ds, ws, user) = ctx().await;
        let root = ds.create(ws, None, "R", "m", user).await.unwrap();
        let child = ds.create(ws, Some(root.id), "C", "m", user).await.unwrap();
        gs.put(
            ws,
            root.id,
            &format!("user:{user}"),
            WorkspaceRole::Editor,
            true,
            user,
        )
        .await
        .unwrap();
        let r = resolve(&ws_s, &gs, &ds, ws, child.id, user).await.unwrap();
        assert_eq!(r.map(|a| a.role), Some(WorkspaceRole::Editor));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn non_member_with_no_grant_is_none() {
        let (ws_s, gs, ds, ws, owner) = ctx().await;
        let d = ds.create(ws, None, "X", "m", owner).await.unwrap();
        let other = Uuid::new_v4();
        let r = resolve(&ws_s, &gs, &ds, ws, d.id, other).await.unwrap();
        assert_eq!(r, None);
    }

    /// Regression: a member of workspace A must NOT inherit access to a
    /// document that lives in workspace B, even though `get_member_role`
    /// returns their (valid) role within workspace A. The doc-tenancy guard
    /// in `resolve` closes this cross-tenant read.
    #[tokio::test(flavor = "multi_thread")]
    async fn cross_workspace_doc_is_denied() {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool);

        // Workspace A with an Owner member.
        let a = ws_s.create("wsa", "A").await.unwrap();
        let user_a = us.create_local("a@x.test", "A", "$h$").await.unwrap();
        ws_s.add_member(a.id, user_a.id, WorkspaceRole::Owner)
            .await
            .unwrap();

        // Workspace B with its own owner and a private document.
        let b = ws_s.create("wsb", "B").await.unwrap();
        let owner_b = us.create_local("b@x.test", "B", "$h$").await.unwrap();
        ws_s.add_member(b.id, owner_b.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let doc_b = ds
            .create(b.id, None, "Secret", "m", owner_b.id)
            .await
            .unwrap();

        // user_a (Owner of A, NOT a member of B) asks for doc_b using their
        // own workspace A — must be denied by the tenancy guard.
        let r = resolve(&ws_s, &gs, &ds, a.id, doc_b.id, user_a.id)
            .await
            .unwrap();
        assert_eq!(r, None, "cross-workspace read must be denied");

        // Sanity: even scoped to B, a non-member of B has no access.
        let r2 = resolve(&ws_s, &gs, &ds, b.id, doc_b.id, user_a.id)
            .await
            .unwrap();
        assert_eq!(r2, None, "non-member of B has no access even in B's scope");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn restricted_doc_hides_from_workspace_owner() {
        let (ws_s, gs, ds, ws, creator, pool) = ctx_with_pool().await;
        let workspace_owner = PgUserStore::new(pool.clone())
            .create_local("owner@x.test", "O", "$h$")
            .await
            .unwrap();
        ws_s.add_member(ws, workspace_owner.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        let boundary = ds.create(ws, None, "Secret", "m", creator).await.unwrap();
        let child = ds
            .create(ws, Some(boundary.id), "Inside", "m", creator)
            .await
            .unwrap();
        mark_restricted(&pool, boundary.id).await;

        for doc_id in [boundary.id, child.id] {
            let access = resolve(&ws_s, &gs, &ds, ws, doc_id, workspace_owner.id)
                .await
                .unwrap();
            assert_eq!(access, None, "workspace owner must not see {doc_id}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn grant_inside_restricted_subtree_gives_access() {
        let (ws_s, gs, ds, ws, user, pool) = ctx_with_pool().await;
        let boundary = ds.create(ws, None, "Secret", "m", user).await.unwrap();
        let child = ds
            .create(ws, Some(boundary.id), "Inside", "m", user)
            .await
            .unwrap();
        mark_restricted(&pool, boundary.id).await;
        gs.put(
            ws,
            boundary.id,
            &format!("user:{user}"),
            WorkspaceRole::Viewer,
            true,
            user,
        )
        .await
        .unwrap();

        for doc_id in [boundary.id, child.id] {
            let access = resolve(&ws_s, &gs, &ds, ws, doc_id, user).await.unwrap();
            assert_eq!(
                access,
                Some(DocAccess {
                    role: WorkspaceRole::Viewer,
                    archived: false,
                    restriction_root: Some(boundary.id),
                })
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn removed_member_with_grant_gets_none() {
        let (ws_s, gs, ds, ws, user) = ctx().await;
        let doc = ds.create(ws, None, "X", "m", user).await.unwrap();
        gs.put(
            ws,
            doc.id,
            &format!("user:{user}"),
            WorkspaceRole::Owner,
            true,
            user,
        )
        .await
        .unwrap();
        ws_s.remove_member(ws, user).await.unwrap();

        let access = resolve(&ws_s, &gs, &ds, ws, doc.id, user).await.unwrap();
        assert_eq!(access, None, "a grant alone must not outlive membership");
    }
}
