//! Resolve the effective role for (doc_id, user_id).
//!
//! Algorithm:
//! 1. Look up the user's workspace role (Owner > Editor > Viewer > non-member).
//! 2. Walk grants from the doc up to root via GrantStore::list_inherited.
//!    The doc's own grants are always considered; ancestors only contribute
//!    grants with inherit=true.
//! 3. Filter to grants matching `user:<user_id>` and take the max role.
//! 4. Return the max of (workspace_role, grant_role), or None if neither
//!    yields a role (user is not a workspace member AND has no explicit
//!    grant on the doc or its ancestors).

use knot_storage::{
    DocStore, DocStoreError, GrantStore, GrantStoreError, WorkspaceRole, WorkspaceStore,
    WorkspaceStoreError,
};
use thiserror::Error;
use uuid::Uuid;

pub type EffectiveRole = WorkspaceRole;

/// What a user may do with one doc: their effective role plus whether the
/// doc is in the archive. Archived docs are read-only for editors and
/// owners and invisible to viewers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocAccess {
    pub role: WorkspaceRole,
    pub archived: bool,
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

fn max(a: WorkspaceRole, b: WorkspaceRole) -> WorkspaceRole {
    if rank(a) >= rank(b) { a } else { b }
}

pub async fn resolve(
    workspaces: &dyn WorkspaceStore,
    grants: &dyn GrantStore,
    docs: &dyn DocStore,
    workspace_id: Uuid,
    doc_id: Uuid,
    user_id: Uuid,
) -> Result<Option<DocAccess>, ResolveError> {
    // Tenancy guard: the document must belong to the caller's workspace.
    // Without this, `get_member_role(workspace_id, user_id)` returns the
    // caller's role in their OWN workspace for ANY doc_id — granting a member
    // of one workspace read/write access to documents in every other workspace
    // by UUID. We re-assert the doc's workspace here (the cache key is
    // (doc_id, user_id), and a doc maps to exactly one workspace, so this does
    // not poison other users' entries).
    let archived = match docs.get(doc_id).await? {
        Some(doc) if doc.workspace_id == workspace_id => doc.archived_at.is_some(),
        _ => return Ok(None),
    };
    let workspace_role = workspaces.get_member_role(workspace_id, user_id).await?;
    let principal = format!("user:{user_id}");
    let inherited = grants.list_inherited(workspace_id, doc_id).await?;
    let grant_role = inherited
        .into_iter()
        .filter(|g| g.principal == principal)
        .map(|g| g.role)
        .reduce(max);
    let role = match (workspace_role, grant_role) {
        (None, None) => None,
        (Some(w), None) => Some(w),
        (None, Some(g)) => Some(g),
        (Some(w), Some(g)) => Some(max(w, g)),
    };
    Ok(role.map(|role| DocAccess { role, archived }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use knot_storage::{
        DocStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    };

    async fn ctx() -> (PgWorkspaceStore, PgGrantStore, PgDocStore, Uuid, Uuid) {
        let pool = knot_test_support::fresh_db().await.pool;

        let ws_s = PgWorkspaceStore::new(pool.clone());
        let us = PgUserStore::new(pool.clone());
        let ds = PgDocStore::new(pool.clone());
        let gs = PgGrantStore::new(pool);
        let w = ws_s.create("default", "W").await.unwrap();
        let u = us.create_local("a@x.test", "A", "$h$").await.unwrap();
        ws_s.add_member(w.id, u.id, WorkspaceRole::Viewer)
            .await
            .unwrap();
        (ws_s, gs, ds, w.id, u.id)
    }

    #[test]
    fn archived_docs_are_read_only_and_hidden_from_viewers() {
        use WorkspaceRole::{Editor, Owner, Viewer};
        let access = |role, archived| DocAccess { role, archived };
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
                archived: false
            })
        );
        assert_eq!(
            gone_access,
            Some(DocAccess {
                role: WorkspaceRole::Viewer,
                archived: true
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
}
