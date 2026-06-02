//! Drives the same code path the CLI does via direct function calls.
//! Avoids spawning the binary in tests (would need stdin mocking + IPC).

use std::sync::Arc;

use knot_auth::Hasher;
use knot_storage::{PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore};

#[tokio::test(flavor = "multi_thread")]
async fn admin_create_seeds_first_user_and_workspace() {
    let pool = knot_test_support::fresh_db().await.pool;

    let users = Arc::new(PgUserStore::new(pool.clone()));
    let ws = Arc::new(PgWorkspaceStore::new(pool));
    let hasher = Hasher::fast_for_tests();

    let workspace = ws.create("default", "Workspace").await.unwrap();
    let hash = hasher.hash("hunter22").unwrap();
    let user = users
        .create_local("admin@example.com", "Admin", &hash)
        .await
        .unwrap();
    ws.add_member(workspace.id, user.id, WorkspaceRole::Owner)
        .await
        .unwrap();

    assert_eq!(users.count().await.unwrap(), 1);
    let role = ws.get_member_role(workspace.id, user.id).await.unwrap();
    assert_eq!(role, Some(WorkspaceRole::Owner));
}
