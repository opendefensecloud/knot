use knot_storage::{
    DocStore, PgDocStore, PgSnapshotStore, PgUserStore, PgWorkspaceStore, SnapshotStamp,
    SnapshotStore, SnapshotStoreError, UserStore, WorkspaceRole, WorkspaceStore,
};
async fn setup() -> (PgSnapshotStore, uuid::Uuid) {
    let (s, _pool, doc) = setup_with_pool().await;
    (s, doc)
}

async fn setup_with_pool() -> (PgSnapshotStore, sqlx::PgPool, uuid::Uuid) {
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
    (PgSnapshotStore::new(pool.clone()), pool, d.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_and_load_latest_round_trip() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"state-bytes", b"sv-bytes")
        .await
        .unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(got.snapshot_seq, 100);
    assert_eq!(got.state_bytes, b"state-bytes");
    assert_eq!(got.state_vector, b"sv-bytes");
}

#[tokio::test(flavor = "multi_thread")]
async fn latest_returns_highest_snapshot_seq() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"a", b"a").await.unwrap();
    s.insert(doc, 200, b"b", b"b").await.unwrap();
    s.insert(doc, 150, b"c", b"c").await.unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(got.snapshot_seq, 200);
    assert_eq!(got.state_bytes, b"b");
}

#[tokio::test(flavor = "multi_thread")]
async fn upsert_overwrites_same_seq() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"v1", b"sv1").await.unwrap();
    s.insert(doc, 100, b"v2", b"sv2").await.unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(got.state_bytes, b"v2");
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_keeps_recent_and_per_day() {
    let (s, doc) = setup().await;
    for i in 1..=7i64 {
        s.insert(doc, i * 100, &format!("v{i}").into_bytes(), b"sv")
            .await
            .unwrap();
    }
    let n = s.gc(doc, 5, 30).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(s.latest(doc).await.unwrap().unwrap().snapshot_seq, 700);
}

// ---------------------------------------------------------------------------
// Page history M1.2: stamped snapshots
// ---------------------------------------------------------------------------

fn stamp(seq: i64, rev: i64, epoch: i32) -> SnapshotStamp {
    SnapshotStamp {
        snapshot_seq: seq,
        snapshot_rev: Some(rev),
        epoch: Some(epoch),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_stamped_round_trips_rev_and_epoch() {
    let (s, doc) = setup().await;
    s.insert_stamped(doc, stamp(42, 7, 0), b"state", b"sv")
        .await
        .unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(
        (got.snapshot_seq, got.snapshot_rev, got.epoch),
        (42, Some(7), Some(0))
    );
    assert_eq!(got.state_bytes, b"state");
    assert_eq!(got.state_vector, b"sv");
    let by_seq = s.by_seq(doc, 42).await.unwrap().unwrap();
    assert_eq!(by_seq, got);

    // A legacy write (the provided `insert`) carries neither.
    s.insert(doc, 10, b"old", b"sv").await.unwrap();
    let legacy = s.by_seq(doc, 10).await.unwrap().unwrap();
    assert_eq!((legacy.snapshot_rev, legacy.epoch), (None, None));
}

#[tokio::test(flavor = "multi_thread")]
async fn latest_prefers_the_highest_snapshot_rev() {
    let (s, doc) = setup().await;
    s.insert(doc, 900, b"legacy", b"sv").await.unwrap();
    s.insert_stamped(doc, stamp(500, 7, 0), b"stamped 7", b"sv")
        .await
        .unwrap();
    s.insert_stamped(doc, stamp(400, 6, 0), b"stamped 6", b"sv")
        .await
        .unwrap();
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(
        (got.snapshot_seq, got.snapshot_rev),
        (500, Some(7)),
        "a stamped snapshot beats a legacy one with a higher seq"
    );
    assert_eq!(got.state_bytes, b"stamped 7");
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_stamped_promotes_a_legacy_row_but_never_rewrites_a_stamped_one() {
    let (s, doc) = setup().await;
    s.insert(doc, 100, b"legacy", b"sv0").await.unwrap();
    s.insert_stamped(doc, stamp(100, 3, 0), b"first", b"sv1")
        .await
        .unwrap();
    let got = s.by_seq(doc, 100).await.unwrap().unwrap();
    assert_eq!(
        (got.state_bytes.as_slice(), got.snapshot_rev, got.epoch),
        (b"first".as_slice(), Some(3), Some(0)),
        "the legacy row is promoted"
    );

    s.insert_stamped(doc, stamp(100, 4, 0), b"second", b"sv2")
        .await
        .unwrap();
    s.insert(doc, 100, b"legacy again", b"sv3").await.unwrap();
    let got = s.by_seq(doc, 100).await.unwrap().unwrap();
    assert_eq!(
        (
            got.state_bytes.as_slice(),
            got.state_vector.as_slice(),
            got.snapshot_rev
        ),
        (b"first".as_slice(), b"sv1".as_slice(), Some(3)),
        "a stamped row is immutable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_epoch_snapshot_is_epoch_changed() {
    let (s, pool, doc) = setup_with_pool().await;
    sqlx::query("INSERT INTO doc_history_heads (doc_id, content_epoch) VALUES ($1, 1)")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    let e = s
        .insert_stamped(doc, stamp(10, 1, 0), b"x", b"sv")
        .await
        .unwrap_err();
    assert!(matches!(e, SnapshotStoreError::EpochChanged), "{e:?}");
    // A legacy write (no epoch) is refused once the epoch moved, too.
    let e = s.insert(doc, 11, b"x", b"sv").await.unwrap_err();
    assert!(matches!(e, SnapshotStoreError::EpochChanged), "{e:?}");
    s.insert_stamped(doc, stamp(12, 1, 1), b"x", b"sv")
        .await
        .unwrap();
    assert_eq!(s.latest(doc).await.unwrap().unwrap().snapshot_seq, 12);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_of_a_purged_page_is_doc_gone() {
    let (s, pool, doc) = setup_with_pool().await;
    sqlx::query("DELETE FROM documents WHERE id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    let e = s
        .insert_stamped(doc, stamp(10, 1, 0), b"x", b"sv")
        .await
        .unwrap_err();
    assert!(matches!(e, SnapshotStoreError::DocGone), "{e:?}");
}
