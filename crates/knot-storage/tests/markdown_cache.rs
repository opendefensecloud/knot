use knot_storage::{
    DocStore, MarkdownCacheStore, PgDocStore, PgMarkdownCache, PgUserStore, PgWorkspaceStore,
    UserStore, WorkspaceRole, WorkspaceStore,
};
async fn setup() -> (PgMarkdownCache, uuid::Uuid) {
    let pool = knot_test_support::fresh_db().await.pool;

    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let u = PgUserStore::new(pool.clone())
        .create_local("a@x.test", "A", "$h$")
        .await
        .unwrap();
    PgWorkspaceStore::new(pool.clone())
        .add_member(ws.id, u.id, WorkspaceRole::Owner)
        .await
        .unwrap();
    let d = PgDocStore::new(pool.clone())
        .create(ws.id, None, "D", "m", u.id)
        .await
        .unwrap();
    (PgMarkdownCache::new(pool), d.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn put_then_get_if_fresh() {
    let (s, doc) = setup().await;
    s.put(doc, 42, "# hi\n").await.unwrap();
    let got = s.get_if_fresh(doc, 42).await.unwrap().unwrap();
    assert_eq!(got.markdown_text, "# hi\n");
    assert_eq!(got.rendered_at_seq, 42);
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_seq_returns_none() {
    let (s, doc) = setup().await;
    s.put(doc, 42, "# hi\n").await.unwrap();
    assert!(s.get_if_fresh(doc, 43).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_upserts_on_repeat() {
    let (s, doc) = setup().await;
    s.put(doc, 1, "v1").await.unwrap();
    s.put(doc, 2, "v2").await.unwrap();
    let got = s.get_if_fresh(doc, 2).await.unwrap().unwrap();
    assert_eq!(got.markdown_text, "v2");
    assert!(s.get_if_fresh(doc, 1).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn invalidate_removes_row() {
    let (s, doc) = setup().await;
    s.put(doc, 1, "v").await.unwrap();
    s.invalidate(doc).await.unwrap();
    assert!(s.get_if_fresh(doc, 1).await.unwrap().is_none());
}
