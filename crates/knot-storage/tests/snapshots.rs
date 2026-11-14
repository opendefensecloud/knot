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
    s.insert_stamped(doc, stamp(42, 7, 0), b"state", b"sv", None)
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
    s.insert_stamped(doc, stamp(500, 7, 0), b"stamped 7", b"sv", None)
        .await
        .unwrap();
    s.insert_stamped(doc, stamp(400, 6, 0), b"stamped 6", b"sv", None)
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
    s.insert_stamped(doc, stamp(100, 3, 0), b"first", b"sv1", None)
        .await
        .unwrap();
    let got = s.by_seq(doc, 100).await.unwrap().unwrap();
    assert_eq!(
        (got.state_bytes.as_slice(), got.snapshot_rev, got.epoch),
        (b"first".as_slice(), Some(3), Some(0)),
        "the legacy row is promoted"
    );

    s.insert_stamped(doc, stamp(100, 4, 0), b"second", b"sv2", None)
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
        .insert_stamped(doc, stamp(10, 1, 0), b"x", b"sv", None)
        .await
        .unwrap_err();
    assert!(matches!(e, SnapshotStoreError::EpochChanged), "{e:?}");
    // A legacy write (no epoch) is refused once the epoch moved, too.
    let e = s.insert(doc, 11, b"x", b"sv").await.unwrap_err();
    assert!(matches!(e, SnapshotStoreError::EpochChanged), "{e:?}");
    s.insert_stamped(doc, stamp(12, 1, 1), b"x", b"sv", None)
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
        .insert_stamped(doc, stamp(10, 1, 0), b"x", b"sv", None)
        .await
        .unwrap_err();
    assert!(matches!(e, SnapshotStoreError::DocGone), "{e:?}");
}

// ---------------------------------------------------------------------------
// Snapshots that name the rows holding their pending content
// (`doc_snapshots.pending_revs`, `older_writer`, `latest_trusted`)
// ---------------------------------------------------------------------------

/// 0.9.14's snapshot GC statement (`PgSnapshotStore::gc` up to 0.9.14),
/// verbatim: it knows nothing of trusted snapshots.
const V0_9_14_GC_SQL: &str = "WITH recent AS (
                 SELECT snapshot_seq FROM doc_snapshots
                 WHERE doc_id = $1 ORDER BY snapshot_seq DESC LIMIT $2
             ),
             per_day AS (
                 SELECT DISTINCT ON (date_trunc('day', created_at)) snapshot_seq
                 FROM doc_snapshots
                 WHERE doc_id = $1 AND created_at >= now() - ($3 || ' days')::interval
                 ORDER BY date_trunc('day', created_at), created_at DESC
             )
             DELETE FROM doc_snapshots
             WHERE doc_id = $1
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM recent)
               AND snapshot_seq NOT IN (SELECT snapshot_seq FROM per_day)";

/// A 0.9.10 to 0.9.14 snapshot write (`migrations_apply.rs`), which names
/// no `pending_revs`.
const STAMPED_SNAPSHOT_INSERT_SQL: &str = "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch)
             VALUES ($1, $2, $3, $4, $5, $6)";

/// The snapshot schema of 0.9.14, the release before `pending_revs`.
const V0_9_14_SCHEMA: i64 = 20261114120000;

/// A stamped row written through raw SQL a day before the migration:
/// `(doc, seq, bytes, rev)`, epoch 0, `pending_revs` NULL.
async fn backdated_snapshot(pool: &sqlx::PgPool, doc: uuid::Uuid, seq: i64, rev: i64) {
    sqlx::query(
        "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch, created_at)
         VALUES ($1, $2, 'old', 'sv', $3, 0, now() - interval '1 day')",
    )
    .bind(doc)
    .bind(seq)
    .bind(rev)
    .execute(pool)
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_revs_round_trip() {
    let (s, doc) = setup().await;
    assert!(
        s.insert_stamped(doc, stamp(10, 7, 0), b"state", b"sv", Some(&[3, 7]))
            .await
            .unwrap()
    );
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(
        (got.snapshot_rev, got.pending_revs, got.older_writer),
        (Some(7), Some(vec![3, 7]), false)
    );

    s.insert_stamped(doc, stamp(20, 8, 0), b"state", b"sv", Some(&[]))
        .await
        .unwrap();
    let got = s.by_seq(doc, 20).await.unwrap().unwrap();
    assert_eq!(
        (got.pending_revs.clone(), got.older_writer),
        (Some(vec![]), false),
        "an empty list is not NULL"
    );
    assert_eq!(s.latest(doc).await.unwrap().unwrap(), got);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_null_stamped_snapshot_written_after_the_migration_is_an_older_writers() {
    let (s, pool, doc) = setup_with_pool().await;
    s.insert_stamped(doc, stamp(30, 3, 0), b"older pod", b"sv", None)
        .await
        .unwrap();
    let got = s.by_seq(doc, 30).await.unwrap().unwrap();
    assert_eq!((got.pending_revs, got.older_writer), (None, true));
    assert!(s.latest(doc).await.unwrap().unwrap().older_writer);

    backdated_snapshot(&pool, doc, 20, 2).await;
    let got = s.by_seq(doc, 20).await.unwrap().unwrap();
    assert_eq!(
        (got.pending_revs, got.older_writer),
        (None, false),
        "a stamped NULL row from before the migration is a legacy one"
    );

    s.insert(doc, 10, b"0.9.9", b"sv").await.unwrap();
    let got = s.by_seq(doc, 10).await.unwrap().unwrap();
    assert_eq!(
        (got.snapshot_rev, got.pending_revs, got.older_writer),
        (None, None, false),
        "an unstamped row is never flagged"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn latest_trusted_skips_an_older_writers_rows() {
    let (s, pool, doc) = setup_with_pool().await;
    s.insert_stamped(doc, stamp(10, 1, 0), b"fixed", b"sv", Some(&[1]))
        .await
        .unwrap();
    s.insert_stamped(doc, stamp(20, 2, 0), b"older pod", b"sv", None)
        .await
        .unwrap();
    let latest = s.latest(doc).await.unwrap().unwrap();
    assert_eq!((latest.snapshot_rev, latest.older_writer), (Some(2), true));
    let trusted = s.latest_trusted(doc).await.unwrap().unwrap();
    assert_eq!(
        (
            trusted.snapshot_rev,
            trusted.state_bytes.as_slice(),
            trusted.pending_revs,
            trusted.older_writer
        ),
        (Some(1), b"fixed".as_slice(), Some(vec![1]), false)
    );

    // Only the flagged row: nothing is trusted.
    let (s2, other) = setup().await;
    s2.insert_stamped(other, stamp(20, 2, 0), b"older pod", b"sv", None)
        .await
        .unwrap();
    assert_eq!(s2.latest_trusted(other).await.unwrap(), None);

    // A NULL row from before the migration is not flagged: both return it.
    backdated_snapshot(&pool, doc, 30, 3).await;
    let latest = s.latest(doc).await.unwrap().unwrap();
    let trusted = s.latest_trusted(doc).await.unwrap().unwrap();
    assert_eq!((latest.snapshot_rev, latest.older_writer), (Some(3), false));
    assert_eq!(trusted, latest);
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_at_returns_the_named_revs_in_order() {
    use knot_storage::{PgUpdatesStore, UpdatesStore};
    let (_s, pool, doc) = setup_with_pool().await;
    let updates = PgUpdatesStore::new(pool);
    let rows: Vec<(Option<uuid::Uuid>, Vec<u8>)> = (1..=6u8).map(|i| (None, vec![i])).collect();
    updates.insert_batch(doc, &rows).await.unwrap();

    let got = updates.rows_at(doc, &[5, 2]).await.unwrap();
    assert_eq!(
        got.iter()
            .map(|r| (r.rev, r.update_bytes.clone()))
            .collect::<Vec<_>>(),
        vec![(Some(2), vec![2]), (Some(5), vec![5])]
    );
    let got = updates.rows_at(doc, &[4, 99]).await.unwrap();
    assert_eq!(
        got.iter().map(|r| r.rev).collect::<Vec<_>>(),
        vec![Some(4)],
        "a missing rev is simply absent"
    );
    assert!(updates.rows_at(doc, &[]).await.unwrap().is_empty());
}

/// Ruling 6's seq collision, store side: the trigger skips the update of a
/// stamped row, and `insert_stamped` says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_stamped_write_at_a_taken_seq_reports_it_was_skipped() {
    let (s, doc) = setup().await;
    // A 0.9.14 pod's write.
    assert!(
        s.insert_stamped(doc, stamp(4, 2, 0), b"first", b"sv1", None)
            .await
            .unwrap()
    );
    assert!(
        !s.insert_stamped(doc, stamp(4, 2, 0), b"second", b"sv2", Some(&[]))
            .await
            .unwrap(),
        "a write over a stamped row was reported as written"
    );
    let got = s.by_seq(doc, 4).await.unwrap().unwrap();
    assert_eq!(
        (
            got.state_bytes.as_slice(),
            got.pending_revs,
            got.older_writer
        ),
        (b"first".as_slice(), None, true)
    );

    assert!(
        s.insert_stamped(doc, stamp(5, 3, 0), b"next", b"sv", Some(&[]))
            .await
            .unwrap()
    );

    s.insert(doc, 9, b"legacy", b"sv").await.unwrap();
    assert!(
        s.insert_stamped(doc, stamp(9, 4, 0), b"promoted", b"sv", Some(&[]))
            .await
            .unwrap(),
        "promoting a legacy row is a write"
    );
    let got = s.by_seq(doc, 9).await.unwrap().unwrap();
    assert_eq!(
        (
            got.state_bytes.as_slice(),
            got.snapshot_rev,
            got.pending_revs
        ),
        (b"promoted".as_slice(), Some(4), Some(vec![]))
    );
}

/// Ruling 6: GC keeps the page's newest trusted snapshot beside its newest
/// five and one per day; 0.9.14's statement does not.
#[tokio::test(flavor = "multi_thread")]
async fn gc_keeps_the_trusted_snapshot() {
    let (s, pool, doc) = setup_with_pool().await;
    s.insert_stamped(doc, stamp(1, 1, 0), b"fixed", b"sv", Some(&[]))
        .await
        .unwrap();
    for i in 2..=7 {
        s.insert_stamped(doc, stamp(i, i, 0), b"older pod", b"sv", None)
            .await
            .unwrap();
    }
    assert_eq!(s.gc(doc, 5, 30).await.unwrap(), 1);
    let kept: Vec<i64> = sqlx::query_scalar(
        "SELECT snapshot_seq FROM doc_snapshots WHERE doc_id = $1 ORDER BY snapshot_seq",
    )
    .bind(doc)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(kept, vec![1, 3, 4, 5, 6, 7], "only rev 2 goes");
    let trusted = s.latest_trusted(doc).await.unwrap().unwrap();
    assert_eq!(trusted.snapshot_rev, Some(1));

    let deleted = sqlx::query(V0_9_14_GC_SQL)
        .bind(doc)
        .bind(5i64)
        .bind(30i32)
        .execute(&pool)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(deleted, 1, "0.9.14's GC deletes the trusted row");
    assert_eq!(s.latest_trusted(doc).await.unwrap(), None);
}

/// A database that never ran the `pending_revs` migration (`knot-server
/// export-check` on a staging copy of 0.9.14): `latest` still reads it, and
/// nothing on it is trusted over the latest snapshot.
#[tokio::test(flavor = "multi_thread")]
async fn latest_reads_a_database_without_pending_revs() {
    use knot_test_support::migrate::{bare_db, migrate_up_to};
    let (_url, pool) = bare_db(2).await;
    migrate_up_to(&pool, V0_9_14_SCHEMA).await;
    let doc: uuid::Uuid = sqlx::query_scalar(
        "WITH w AS (INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id),
              u AS (INSERT INTO users (email, display_name) VALUES ('a@x.test', 'A')
                    RETURNING id)
         INSERT INTO documents (workspace_id, title, sort_key, created_by)
         SELECT w.id, 'D', 'm', u.id FROM w, u RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(STAMPED_SNAPSHOT_INSERT_SQL)
        .bind(doc)
        .bind(40i64)
        .bind(b"stamped".as_slice())
        .bind(b"sv".as_slice())
        .bind(4i64)
        .bind(0i32)
        .execute(&pool)
        .await
        .unwrap();

    let s = PgSnapshotStore::new(pool);
    let got = s.latest(doc).await.unwrap().unwrap();
    assert_eq!(
        (
            got.snapshot_seq,
            got.state_bytes.as_slice(),
            got.snapshot_rev,
            got.epoch,
            got.pending_revs,
            got.older_writer
        ),
        (40, b"stamped".as_slice(), Some(4), Some(0), None, false)
    );
    assert_eq!(s.latest_trusted(doc).await.unwrap(), None);
}
