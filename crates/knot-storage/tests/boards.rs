use knot_storage::{
    BoardStore, DocStore, PgBoardStore, PgDocStore, PgUserStore, PgWorkspaceStore, UserStore,
    WorkspaceRole, WorkspaceStore, sort_key_between,
};
use uuid::Uuid;

async fn setup() -> (PgBoardStore, Uuid, Uuid) {
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
    let docs = PgDocStore::new(pool.clone());
    let sk = sort_key_between(None, None);
    let doc = docs.create(ws.id, None, "Doc", &sk, u.id).await.unwrap();
    (PgBoardStore::new(pool), doc.id, u.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn create_get_list_lifecycle() {
    let (store, doc_id, user) = setup().await;
    let b = store
        .create(doc_id, user, Some("Diagram".into()))
        .await
        .unwrap();
    assert_eq!(b.doc_id, doc_id);
    assert_eq!(b.label.as_deref(), Some("Diagram"));
    let got = store.get(b.id).await.unwrap();
    assert_eq!(got.id, b.id);
    let list = store.list_for_doc(doc_id).await.unwrap();
    assert_eq!(list.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_hides_from_list() {
    let (store, doc_id, user) = setup().await;
    let b = store.create(doc_id, user, None).await.unwrap();
    store.delete(b.id).await.unwrap();
    let list = store.list_for_doc(doc_id).await.unwrap();
    assert!(list.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn append_updates_round_trip() {
    let (store, doc_id, user) = setup().await;
    let b = store.create(doc_id, user, None).await.unwrap();
    let s1 = store.append_update(b.id, &[1, 2, 3]).await.unwrap();
    let s2 = store.append_update(b.id, &[4, 5, 6]).await.unwrap();
    assert!(s2 > s1);
    assert_eq!(store.max_update_seq(b.id).await.unwrap(), s2);
    let all = store.load_updates(b.id).await.unwrap();
    assert_eq!(all, vec![vec![1, 2, 3], vec![4, 5, 6]]);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_latest() {
    let (store, doc_id, user) = setup().await;
    let b = store.create(doc_id, user, None).await.unwrap();
    store.put_snapshot(b.id, 1, &[9, 9, 9]).await.unwrap();
    store.put_snapshot(b.id, 5, &[8, 8, 8]).await.unwrap();
    let (seq, bytes) = store.latest_snapshot(b.id).await.unwrap().unwrap();
    assert_eq!(seq, 5);
    assert_eq!(bytes, vec![8, 8, 8]);
}

#[tokio::test(flavor = "multi_thread")]
async fn svg_set_and_get() {
    let (store, doc_id, user) = setup().await;
    let b = store.create(doc_id, user, None).await.unwrap();
    assert!(store.get_svg(b.id).await.unwrap().is_none());
    store.set_svg(b.id, b"<svg/>").await.unwrap();
    let got = store.get_svg(b.id).await.unwrap().unwrap();
    assert_eq!(&got, b"<svg/>");
}

#[tokio::test(flavor = "multi_thread")]
async fn since_returns_updates_after_seq_in_order() {
    let (store, doc_id, user) = setup().await;
    let board_id = store.create(doc_id, user, None).await.unwrap().id;

    let s1 = store.append_update(board_id, b"u1").await.unwrap();
    let s2 = store.append_update(board_id, b"u2").await.unwrap();
    let s3 = store.append_update(board_id, b"u3").await.unwrap();

    let all = store.since(board_id, 0).await.unwrap();
    assert_eq!(
        all.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(),
        vec![b"u1".to_vec(), b"u2".to_vec(), b"u3".to_vec()]
    );
    assert_eq!(
        all.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        vec![s1, s2, s3]
    );

    let rest = store.since(board_id, s1).await.unwrap();
    assert_eq!(
        rest.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>(),
        vec![b"u2".to_vec(), b"u3".to_vec()]
    );

    assert!(store.since(board_id, s3).await.unwrap().is_empty());
}
