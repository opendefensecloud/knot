use knot_storage::{
    DocStore, PgDocStore, PgUserStore, PgWorkspaceStore, UserStore, WorkspaceRole, WorkspaceStore,
    sort_key_between,
};
use uuid::Uuid;

async fn setup() -> (PgDocStore, Uuid, Uuid) {
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
    (PgDocStore::new(pool), ws.id, u.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_get_list_lifecycle() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "Hello", &sk, user).await.unwrap();
    assert_eq!(doc.title, "Hello");
    assert_eq!(doc.workspace_id, ws);
    let got = store.get(doc.id).await.unwrap().unwrap();
    assert_eq!(got.id, doc.id);
    let list = store.list_alive(ws).await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_updates_title_and_icon() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "Old", &sk, user).await.unwrap();
    let new = store
        .rename(ws, doc.id, user, "New", Some("📄"))
        .await
        .unwrap();
    assert_eq!(new.title, "New");
    assert_eq!(new.icon.as_deref(), Some("📄"));
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_hides_and_restore_brings_back() {
    let (store, ws, user) = setup().await;
    let sk = sort_key_between(None, None);
    let doc = store.create(ws, None, "X", &sk, user).await.unwrap();
    store.archive(ws, doc.id, user).await.unwrap();
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 0);
    store.restore(ws, doc.id, user).await.unwrap();
    assert_eq!(store.list_alive(ws).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn move_to_under_new_parent() {
    let (store, ws, user) = setup().await;
    let a = store.create(ws, None, "A", "m", user).await.unwrap();
    let b = store.create(ws, None, "B", "n", user).await.unwrap();
    let moved = store
        .move_to(ws, b.id, user, Some(a.id), "m")
        .await
        .unwrap();
    assert_eq!(moved.parent_id, Some(a.id));
    let kids = store.siblings(ws, Some(a.id)).await.unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].id, b.id);
}

#[tokio::test(flavor = "multi_thread")]
async fn rename_not_found() {
    let (store, ws, user) = setup().await;
    let err = store
        .rename(ws, Uuid::new_v4(), user, "X", None)
        .await
        .unwrap_err();
    assert!(matches!(err, knot_storage::DocStoreError::NotFound));
}

#[tokio::test(flavor = "multi_thread")]
async fn descendant_ids_returns_full_subtree() {
    let (store, ws, user) = setup().await;
    let a = store.create(ws, None, "A", "m", user).await.unwrap();
    let b = store.create(ws, Some(a.id), "B", "m", user).await.unwrap();
    let c = store.create(ws, Some(b.id), "C", "m", user).await.unwrap();
    let _d = store.create(ws, None, "D", "n", user).await.unwrap(); // unrelated root

    let descendants = store.descendant_ids(a.id).await.unwrap();
    assert_eq!(descendants.len(), 2);
    assert!(descendants.contains(&b.id));
    assert!(descendants.contains(&c.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn templates_flow_set_and_list() {
    let (store, ws, user) = setup().await;
    // Two regular docs + one template.
    let sk = sort_key_between(None, None);
    let a = store.create(ws, None, "A", &sk, user).await.unwrap();
    assert!(!a.is_template);
    let sk = sort_key_between(None, None);
    let b = store.create(ws, None, "B", &sk, user).await.unwrap();
    let sk = sort_key_between(None, None);
    let tpl = store
        .create(ws, None, "Meeting notes", &sk, user)
        .await
        .unwrap();
    // Flip the template flag.
    let after = store.set_template(ws, tpl.id, user, true).await.unwrap();
    assert!(after.is_template);
    // list_alive must exclude the template.
    let alive = store.list_alive(ws).await.unwrap();
    let ids: Vec<_> = alive.iter().map(|d| d.id).collect();
    assert!(ids.contains(&a.id));
    assert!(ids.contains(&b.id));
    assert!(!ids.contains(&tpl.id));
    // list_templates returns just the template.
    let templates = store.list_templates(ws).await.unwrap();
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].id, tpl.id);
    // Unmark restores it to the main tree.
    store.set_template(ws, tpl.id, user, false).await.unwrap();
    let alive2 = store.list_alive(ws).await.unwrap();
    assert!(alive2.iter().any(|d| d.id == tpl.id));
    let templates2 = store.list_templates(ws).await.unwrap();
    assert!(templates2.is_empty());
}
