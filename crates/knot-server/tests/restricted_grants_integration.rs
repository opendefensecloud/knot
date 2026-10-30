//! Grants handed out on the server's own initiative, for restricted
//! documents (private subtrees):
//! - `restricted::ensure_creator_can_edit`, the creator's automatic `owner`
//!   grant, which must not outlast a parent the creator lost meanwhile
//! - `admin::grant`, the break-glass CLI, which names no acting user
//!
//! See docs/superpowers/specs/2026-10-30-restricted-documents-design.md.

use knot_server::AppState;
use knot_storage::{Grant, WorkspaceRole};
use uuid::Uuid;

/// A workspace with Alice (owner) and Eve (editor), driven through the
/// stores of a fresh `AppState`.
struct Fixture {
    state: AppState,
    workspace_id: Uuid,
    alice: Uuid,
    eve: Uuid,
}

async fn fixture() -> Fixture {
    let pool = knot_test_support::fresh_db().await.pool;
    let state = AppState::with_pool(pool);
    let workspaces = state.workspaces.clone().unwrap();
    let users = state.users.clone().unwrap();
    let workspace_id = workspaces.create("default", "W").await.unwrap().id;
    let alice = users
        .create_local("alice@grants.test", "Alice", "$h$")
        .await
        .unwrap()
        .id;
    let eve = users
        .create_local("eve@grants.test", "Eve", "$h$")
        .await
        .unwrap()
        .id;
    workspaces
        .add_member(workspace_id, alice, WorkspaceRole::Owner)
        .await
        .unwrap();
    workspaces
        .add_member(workspace_id, eve, WorkspaceRole::Editor)
        .await
        .unwrap();
    Fixture {
        state,
        workspace_id,
        alice,
        eve,
    }
}

impl Fixture {
    async fn create(&self, parent_id: Option<Uuid>, title: &str, creator: Uuid) -> Uuid {
        self.state
            .docs
            .as_ref()
            .unwrap()
            .create(self.workspace_id, parent_id, title, title, creator)
            .await
            .unwrap()
            .id
    }

    async fn restrict(&self, doc_id: Uuid, actor: Uuid) {
        self.state
            .docs
            .as_ref()
            .unwrap()
            .set_restricted(self.workspace_id, doc_id, actor, true)
            .await
            .unwrap();
    }

    async fn grant_of(&self, doc_id: Uuid, user_id: Uuid) -> Option<Grant> {
        let principal = format!("user:{user_id}");
        self.state
            .grants
            .as_ref()
            .unwrap()
            .list(doc_id)
            .await
            .unwrap()
            .into_iter()
            .find(|grant| grant.principal == principal)
    }
}

/// Eve passed the parent check while P was open, then Alice restricted P
/// before Eve's page was made. Eve couldn't edit P when the grant was
/// decided, so she gets no owner foothold inside Alice's private subtree.
#[tokio::test(flavor = "multi_thread")]
async fn creator_who_lost_the_parent_gets_no_owner_grant() {
    let fx = fixture().await;
    let parent_id = fx.create(None, "P", fx.alice).await;
    // Eve's pre-check reads P as editable and caches it.
    let acl = fx.state.acl.clone().unwrap();
    let parent_access = acl
        .access(fx.workspace_id, parent_id, fx.eve)
        .await
        .unwrap()
        .unwrap();
    assert!(parent_access.can_write());

    fx.restrict(parent_id, fx.alice).await;
    let child_id = fx.create(Some(parent_id), "C", fx.eve).await;
    knot_server::restricted::ensure_creator_can_edit(&fx.state, fx.workspace_id, child_id, fx.eve)
        .await
        .expect("no error");

    assert_eq!(fx.grant_of(child_id, fx.eve).await, None);
    let child_access = acl.access(fx.workspace_id, child_id, fx.eve).await.unwrap();
    assert_eq!(child_access, None, "Eve can't reach the page inside P");
}

/// The case the grant is for: Eve's access to the restricted P comes from
/// a grant that doesn't inherit, so her new subpage gets her an owner grant.
#[tokio::test(flavor = "multi_thread")]
async fn creator_with_a_non_inheriting_grant_gets_owner() {
    let fx = fixture().await;
    let parent_id = fx.create(None, "P", fx.alice).await;
    fx.restrict(parent_id, fx.alice).await;
    fx.state
        .grants
        .as_ref()
        .unwrap()
        .put(
            fx.workspace_id,
            parent_id,
            &format!("user:{}", fx.eve),
            WorkspaceRole::Editor,
            false,
            fx.alice,
        )
        .await
        .unwrap();

    let child_id = fx.create(Some(parent_id), "C", fx.eve).await;
    knot_server::restricted::ensure_creator_can_edit(&fx.state, fx.workspace_id, child_id, fx.eve)
        .await
        .expect("no error");

    let grant = fx.grant_of(child_id, fx.eve).await.expect("owner grant");
    assert_eq!(grant.role, WorkspaceRole::Owner);
    assert!(grant.inherit);
    let child_access = fx
        .state
        .acl
        .as_ref()
        .unwrap()
        .access(fx.workspace_id, child_id, fx.eve)
        .await
        .unwrap()
        .expect("Eve reaches her page");
    assert!(child_access.can_write());
}

/// At the top level the "parent" is the workspace: Eve, lowered to viewer
/// after the route's role check, gets no owner grant on the page she made.
#[tokio::test(flavor = "multi_thread")]
async fn creator_lowered_to_viewer_gets_no_owner_grant_at_the_top_level() {
    let fx = fixture().await;
    fx.state
        .workspaces
        .as_ref()
        .unwrap()
        .update_role(fx.workspace_id, fx.eve, WorkspaceRole::Viewer)
        .await
        .unwrap();
    let doc_id = fx.create(None, "Top", fx.eve).await;
    knot_server::restricted::ensure_creator_can_edit(&fx.state, fx.workspace_id, doc_id, fx.eve)
        .await
        .expect("no error");
    assert_eq!(fx.grant_of(doc_id, fx.eve).await, None);
}

/// `admin grant` names no acting user: the grant row's `granted_by` is
/// NULL, and the only audit event is `admin.grant` with `break_glass`, not a
/// `doc.grant` that reads as the recipient granting themselves.
#[tokio::test(flavor = "multi_thread")]
async fn admin_grant_is_not_attributed_to_the_recipient() {
    let fx = fixture().await;
    let pool = fx.state.pool.clone().unwrap();
    let doc_id = fx.create(None, "D", fx.alice).await;

    knot_server::admin::grant(&pool, doc_id, "eve@grants.test", "owner", false)
        .await
        .expect("break-glass grant");

    let grant = fx.grant_of(doc_id, fx.eve).await.expect("grant");
    assert_eq!(grant.role, WorkspaceRole::Owner);
    assert_eq!(grant.granted_by, None);
    let events: Vec<(String, Option<Uuid>)> = sqlx::query_as(
        "SELECT action, actor_id FROM audit_events
          WHERE target_id = $1 AND action LIKE '%grant%'",
    )
    .bind(doc_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(events, vec![("admin.grant".to_string(), None)]);

    // A non-member is refused, and nothing is written.
    fx.state
        .workspaces
        .as_ref()
        .unwrap()
        .remove_member(fx.workspace_id, fx.eve)
        .await
        .unwrap();
    let refusal = knot_server::admin::grant(&pool, doc_id, "eve@grants.test", "owner", false)
        .await
        .expect_err("a removed member can't be granted");
    assert!(
        refusal.to_string().contains("not a member"),
        "unexpected error: {refusal}"
    );
    assert_eq!(fx.grant_of(doc_id, fx.eve).await, None);
}
