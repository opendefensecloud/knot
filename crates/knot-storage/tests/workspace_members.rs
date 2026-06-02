use knot_storage::{PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore};

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
