use knot_storage::{
    DocStore, GrantStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};

async fn setup() -> (PgWorkspaceStore, PgUserStore, uuid::Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;

    let ws = PgWorkspaceStore::new(pool.clone());
    let users = PgUserStore::new(pool);
    let w = ws.create("default", "Workspace").await.unwrap();
    (ws, users, w.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn list_update_remove_members() {
    let (ws, users, ws_id) = setup().await;
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    ws.add_member(ws_id, alice.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws.add_member(ws_id, bob.id, WorkspaceRole::Viewer)
        .await
        .unwrap();

    let members = ws.list_members(ws_id).await.unwrap();
    assert_eq!(members.len(), 2);
    assert!(
        members
            .iter()
            .any(|m| m.email == "alice@x.test" && m.role == WorkspaceRole::Owner)
    );

    ws.update_role(ws_id, bob.id, WorkspaceRole::Editor)
        .await
        .unwrap();
    let role = ws.get_member_role(ws_id, bob.id).await.unwrap();
    assert_eq!(role, Some(WorkspaceRole::Editor));

    ws.remove_member(ws_id, bob.id).await.unwrap();
    assert_eq!(ws.list_members(ws_id).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn count_owners_tracks_role_changes() {
    let (ws, users, ws_id) = setup().await;
    let a = users.create_local("a@x.test", "A", "$h$").await.unwrap();
    let b = users.create_local("b@x.test", "B", "$h$").await.unwrap();
    ws.add_member(ws_id, a.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    ws.add_member(ws_id, b.id, WorkspaceRole::Viewer)
        .await
        .unwrap();
    assert_eq!(ws.count_owners(ws_id).await.unwrap(), 1);
    ws.update_role(ws_id, b.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    assert_eq!(ws.count_owners(ws_id).await.unwrap(), 2);
}

/// Removing a member deletes every `user:` grant they hold in that workspace,
/// including the only owner grant of a restricted doc, and records one
/// `member-remove` invalidation per affected doc. Other principals' grants
/// and the member's grants in another workspace are untouched.
#[tokio::test(flavor = "multi_thread")]
async fn remove_member_deletes_their_grants_and_invalidates() {
    let pool = knot_test_support::fresh_db().await.pool;
    let workspaces = PgWorkspaceStore::new(pool.clone());
    let users = PgUserStore::new(pool.clone());
    let docs = PgDocStore::new(pool.clone());
    let grants = PgGrantStore::new(pool.clone());

    let workspace = workspaces.create("default", "W").await.unwrap();
    let other_workspace = workspaces.create("other", "Other").await.unwrap();
    let alice = users
        .create_local("alice@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users
        .create_local("bob@x.test", "Bob", "$h$")
        .await
        .unwrap();
    for workspace_id in [workspace.id, other_workspace.id] {
        workspaces
            .add_member(workspace_id, alice.id, WorkspaceRole::Owner)
            .await
            .unwrap();
        workspaces
            .add_member(workspace_id, bob.id, WorkspaceRole::Editor)
            .await
            .unwrap();
    }

    let shared_doc = docs
        .create(workspace.id, None, "Shared", "m", alice.id)
        .await
        .unwrap();
    let bob_only_doc = docs
        .create(workspace.id, None, "Bob's", "n", bob.id)
        .await
        .unwrap();
    let foreign_doc = docs
        .create(other_workspace.id, None, "Foreign", "m", alice.id)
        .await
        .unwrap();
    let bob_principal = format!("user:{}", bob.id);
    let alice_principal = format!("user:{}", alice.id);
    grants
        .put(
            workspace.id,
            shared_doc.id,
            &bob_principal,
            WorkspaceRole::Editor,
            true,
            alice.id,
        )
        .await
        .unwrap();
    grants
        .put(
            workspace.id,
            shared_doc.id,
            &alice_principal,
            WorkspaceRole::Owner,
            true,
            alice.id,
        )
        .await
        .unwrap();
    // Restricting as Bob makes him the doc's only owner.
    docs.set_restricted(workspace.id, bob_only_doc.id, bob.id, true)
        .await
        .unwrap();
    grants
        .put(
            other_workspace.id,
            foreign_doc.id,
            &bob_principal,
            WorkspaceRole::Viewer,
            true,
            alice.id,
        )
        .await
        .unwrap();

    workspaces
        .remove_member(workspace.id, bob.id)
        .await
        .unwrap();

    let bob_grants_left: Vec<uuid::Uuid> =
        sqlx::query_scalar("SELECT doc_id FROM document_grants WHERE principal = $1")
            .bind(&bob_principal)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        bob_grants_left,
        vec![foreign_doc.id],
        "only Bob's grant in the other workspace survives"
    );
    let shared_doc_grants = grants.list(shared_doc.id).await.unwrap();
    assert_eq!(shared_doc_grants.len(), 1, "Alice's grant stays");
    assert_eq!(shared_doc_grants[0].principal, alice_principal);

    let mut invalidated_docs: Vec<uuid::Uuid> = sqlx::query_scalar(
        "SELECT doc_id FROM acl_invalidations
         WHERE workspace_id = $1 AND reason = 'member-remove' AND doc_id IS NOT NULL",
    )
    .bind(workspace.id)
    .fetch_all(&pool)
    .await
    .unwrap();
    invalidated_docs.sort();
    let mut expected_docs = vec![shared_doc.id, bob_only_doc.id];
    expected_docs.sort();
    assert_eq!(
        invalidated_docs, expected_docs,
        "one member-remove invalidation per affected doc"
    );
    assert_eq!(
        workspaces
            .get_member_role(workspace.id, bob.id)
            .await
            .unwrap(),
        None
    );
}
