use knot_storage::{
    DocStore, GrantStore, PgDocStore, PgGrantStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};
use uuid::Uuid;

async fn setup() -> (PgDocStore, PgGrantStore, Uuid, Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let u = users.create_local("a@x.test", "A", "$h$").await.unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    (
        PgDocStore::new(pool.clone()),
        PgGrantStore::new(pool),
        ws.id,
        u.id,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn put_list_delete() {
    let (docs, grants, ws, user) = setup().await;
    let d = docs.create(ws, None, "X", "m", user).await.unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, d.id, &principal, WorkspaceRole::Editor, true, user)
        .await
        .unwrap();
    let l = grants.list(d.id).await.unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].role, WorkspaceRole::Editor);
    grants.delete(ws, d.id, &principal, user).await.unwrap();
    assert!(grants.list(d.id).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_updates_existing() {
    let (docs, grants, ws, user) = setup().await;
    let d = docs.create(ws, None, "X", "m", user).await.unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, d.id, &principal, WorkspaceRole::Viewer, true, user)
        .await
        .unwrap();
    grants
        .put(ws, d.id, &principal, WorkspaceRole::Owner, false, user)
        .await
        .unwrap();
    let l = grants.list(d.id).await.unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].role, WorkspaceRole::Owner);
    assert!(!l[0].inherit);
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_includes_ancestor_inherit_true() {
    let (docs, grants, ws, user) = setup().await;
    let root = docs.create(ws, None, "Root", "m", user).await.unwrap();
    let child = docs
        .create(ws, Some(root.id), "Child", "m", user)
        .await
        .unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, root.id, &principal, WorkspaceRole::Editor, true, user)
        .await
        .unwrap();
    let inh = grants.list_inherited(ws, child.id).await.unwrap();
    assert_eq!(inh.len(), 1);
    assert_eq!(inh[0].role, WorkspaceRole::Editor);
}

#[tokio::test(flavor = "multi_thread")]
async fn inherited_skips_ancestor_inherit_false() {
    let (docs, grants, ws, user) = setup().await;
    let root = docs.create(ws, None, "Root", "m", user).await.unwrap();
    let child = docs
        .create(ws, Some(root.id), "Child", "m", user)
        .await
        .unwrap();
    let principal = format!("user:{}", user);
    grants
        .put(ws, root.id, &principal, WorkspaceRole::Editor, false, user)
        .await
        .unwrap();
    let inh = grants.list_inherited(ws, child.id).await.unwrap();
    assert!(
        inh.is_empty(),
        "ancestor inherit=false should not propagate"
    );
    // Sanity: own grants always returned regardless of inherit flag.
    grants
        .put(ws, child.id, &principal, WorkspaceRole::Viewer, false, user)
        .await
        .unwrap();
    let inh2 = grants.list_inherited(ws, child.id).await.unwrap();
    assert_eq!(inh2.len(), 1);
    assert_eq!(inh2[0].doc_id, child.id);
    let _ = ws;
    let _ = Uuid::nil(); // silence unused
}
