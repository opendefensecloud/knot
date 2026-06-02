use knot_storage::{
    DocStore, PgDocStore, PgUpdatesStore, PgUserStore, PgWorkspaceStore, UpdatesStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};
async fn setup() -> (PgUpdatesStore, uuid::Uuid, uuid::Uuid) {
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
    (PgUpdatesStore::new(pool), d.id, u.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_batch_returns_monotone_seqs_in_input_order() {
    let (s, doc, user) = setup().await;
    let batch = vec![vec![1u8, 2, 3], vec![4u8, 5], vec![6u8]];
    let seqs = s.insert_batch(doc, Some(user), &batch).await.unwrap();
    assert_eq!(seqs.len(), 3);
    assert!(seqs[0] < seqs[1] && seqs[1] < seqs[2], "got {seqs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn since_returns_after_watermark_in_order() {
    let (s, doc, user) = setup().await;
    let seqs = s
        .insert_batch(doc, Some(user), &[vec![1u8], vec![2u8], vec![3u8]])
        .await
        .unwrap();
    let after = seqs[0];
    let got = s.since(doc, after).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].seq, seqs[1]);
    assert_eq!(got[0].update_bytes, vec![2u8]);
    assert_eq!(got[1].seq, seqs[2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn max_seq_zero_when_empty_then_grows() {
    let (s, doc, user) = setup().await;
    assert_eq!(s.max_seq(doc).await.unwrap(), 0);
    let seqs = s
        .insert_batch(doc, Some(user), &[vec![1u8], vec![2u8]])
        .await
        .unwrap();
    assert_eq!(s.max_seq(doc).await.unwrap(), *seqs.last().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_up_to_removes_inclusive() {
    let (s, doc, user) = setup().await;
    let seqs = s
        .insert_batch(doc, Some(user), &[vec![1u8], vec![2u8], vec![3u8]])
        .await
        .unwrap();
    let n = s.delete_up_to(doc, seqs[1]).await.unwrap();
    assert_eq!(n, 2);
    let left = s.since(doc, 0).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].seq, seqs[2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_batch_is_noop() {
    let (s, doc, _) = setup().await;
    let seqs = s.insert_batch(doc, None, &[]).await.unwrap();
    assert!(seqs.is_empty());
    assert_eq!(s.max_seq(doc).await.unwrap(), 0);
}
