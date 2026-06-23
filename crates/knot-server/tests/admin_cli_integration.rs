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

/// `admin create` (via the extracted `create_owner` core) must work as a
/// break-glass tool AFTER the system is already bootstrapped: it bootstraps the
/// workspace on the first call, then still creates additional local owners even
/// though users already exist — refusing only a duplicate email.
#[tokio::test(flavor = "multi_thread")]
async fn admin_create_owner_is_break_glass_after_bootstrap() {
    let pool = knot_test_support::fresh_db().await.pool;
    let users = PgUserStore::new(pool.clone());
    let ws = PgWorkspaceStore::new(pool.clone());
    let hasher = Hasher::fast_for_tests();

    // First call bootstraps the singleton workspace + first owner.
    knot_server::admin::create_owner(
        &users,
        &ws,
        &hasher,
        "first@example.com",
        "First",
        "hunter22",
        "default",
        "Workspace",
    )
    .await
    .expect("bootstrap owner");

    // Break-glass: a second local owner is allowed even though a user exists.
    let bg = knot_server::admin::create_owner(
        &users,
        &ws,
        &hasher,
        "break-glass@example.com",
        "BreakGlass",
        "hunter22",
        "default",
        "Workspace",
    )
    .await
    .expect("break-glass owner after bootstrap");

    assert_eq!(users.count().await.unwrap(), 2);
    let wsrow = ws.get_singleton().await.unwrap().expect("workspace exists");
    assert_eq!(
        ws.get_member_role(wsrow.id, bg.id).await.unwrap(),
        Some(WorkspaceRole::Owner),
        "break-glass user is an owner"
    );

    // A duplicate email is refused (would violate the unique constraint).
    let dup = knot_server::admin::create_owner(
        &users,
        &ws,
        &hasher,
        "first@example.com",
        "Dup",
        "hunter22",
        "default",
        "Workspace",
    )
    .await;
    assert!(dup.is_err(), "duplicate email must be rejected");
}
