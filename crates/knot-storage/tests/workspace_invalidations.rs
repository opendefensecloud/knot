//! Workspace-wide ACL invalidations: a member's role change, or their
//! removal, changes their access to every doc at once, so each records one
//! invalidation without a doc in the same transaction, which every replica's
//! listener turns into a full cache eviction and a re-check of every socket.

use knot_storage::{
    PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore, invalidations,
};
use uuid::Uuid;

/// The reasons of the workspace-wide invalidations recorded for
/// `workspace_id`, oldest first.
async fn workspace_wide_reasons(pool: &sqlx::PgPool, workspace_id: Uuid) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT reason FROM acl_invalidations
         WHERE workspace_id = $1 AND doc_id IS NULL
         ORDER BY id",
    )
    .bind(workspace_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn role_change_and_removal_record_a_workspace_wide_invalidation() {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let workspace = workspaces.create("default", "W").await.unwrap();
    let member = PgUserStore::new(pool.clone())
        .create_local("m@x.test", "M", "$h$")
        .await
        .unwrap();
    workspaces
        .add_member(workspace.id, member.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    workspaces
        .update_role(workspace.id, member.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    assert_eq!(
        workspace_wide_reasons(&pool, workspace.id).await,
        vec!["member-role".to_string()],
        "a role change"
    );

    // A member without a single grant: no doc-scoped invalidation names
    // anything, so only the workspace-wide one reaches their cached access
    // and their open sockets.
    workspaces
        .remove_member(workspace.id, member.id)
        .await
        .unwrap();
    assert_eq!(
        workspace_wide_reasons(&pool, workspace.id).await,
        vec!["member-role".to_string(), "member-remove".to_string()],
        "+ a removal"
    );
}

/// The NOTIFY carries the workspace, marked so the listener can tell it
/// from a doc id.
#[tokio::test(flavor = "multi_thread")]
async fn workspace_wide_invalidation_notifies_with_the_workspace_payload() {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspace_id = Uuid::new_v4();
    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen("acl_invalidate").await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    invalidations::record_workspace_in_tx(&mut tx, workspace_id, "member-role")
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let notification = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
        .await
        .expect("no NOTIFY")
        .unwrap();
    assert_eq!(
        invalidations::parse_payload(notification.payload()),
        Some(invalidations::Invalidated::Workspace(workspace_id))
    );
    assert_eq!(
        notification.payload(),
        invalidations::workspace_payload(workspace_id)
    );
}

#[test]
fn payloads_parse_back() {
    let doc_id = Uuid::new_v4();
    let workspace_id = Uuid::new_v4();
    assert_eq!(
        invalidations::parse_payload(&doc_id.to_string()),
        Some(invalidations::Invalidated::Doc(doc_id))
    );
    assert_eq!(
        invalidations::parse_payload(&invalidations::workspace_payload(workspace_id)),
        Some(invalidations::Invalidated::Workspace(workspace_id))
    );
    assert_eq!(invalidations::parse_payload("not-a-uuid"), None);
    assert_eq!(invalidations::parse_payload("workspace:nope"), None);
}
