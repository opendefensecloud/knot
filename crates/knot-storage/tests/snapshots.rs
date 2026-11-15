use knot_storage::{
    DocStore, PgDocStore, PgSnapshotStore, PgUserStore, PgWorkspaceStore, SnapshotStamp,
    SnapshotStore, SnapshotStoreError, ThinPolicy, UserStore, WorkspaceRole, WorkspaceStore,
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

// ---------------------------------------------------------------------------
// Thinning (page history M1.4): the 3 newest per page, plus the trusted one
// and those the history builder has still to probe
// ---------------------------------------------------------------------------

/// The current `knot_history::BUILDER_VERSION` (knot-storage cannot name
/// it; `main.rs` passes the real one).
const BV: i32 = 2;

fn policy() -> ThinPolicy {
    ThinPolicy {
        keep_newest: 3,
        keep_unprobed: 32,
        builder_version: BV,
        legacy_keep_recent: 5,
        legacy_retain_days: 30,
    }
}

/// Another page in the workspace of `doc`, by the same author.
async fn another_page(pool: &sqlx::PgPool, doc: uuid::Uuid) -> uuid::Uuid {
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         SELECT workspace_id, 'Other', $2, created_by FROM documents WHERE id = $1
         RETURNING id",
    )
    .bind(doc)
    .bind(format!("n{}", uuid::Uuid::new_v4().simple()))
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Makes `doc` a page the current builder has built: a state row with
/// `builder_version` BV, `genesis_rev` 0 and `snapshot_checked_rev`
/// `checked`, and a `genesis` checkpoint created `genesis_age` ago.
async fn current(pool: &sqlx::PgPool, doc: uuid::Uuid, checked: i64, genesis_age: &str) {
    state_row(pool, doc, BV, Some(0), checked).await;
    sqlx::query(
        "INSERT INTO doc_history_checkpoints (doc_id, rev, kind, state_bytes, state_vector, created_at)
         VALUES ($1, 0, 'genesis', '\\x00', '\\x00', now() - $2::interval)",
    )
    .bind(doc)
    .bind(genesis_age)
    .execute(pool)
    .await
    .unwrap();
}

async fn state_row(
    pool: &sqlx::PgPool,
    doc: uuid::Uuid,
    builder_version: i32,
    genesis_rev: Option<i64>,
    checked: i64,
) {
    sqlx::query(
        "INSERT INTO doc_history_state (doc_id, builder_version, genesis_rev, snapshot_checked_rev)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(doc)
    .bind(builder_version)
    .bind(genesis_rev)
    .bind(checked)
    .execute(pool)
    .await
    .unwrap();
}

async fn set_state(pool: &sqlx::PgPool, doc: uuid::Uuid, set: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE doc_history_state SET {set} WHERE doc_id = $1"
    )))
    .bind(doc)
    .execute(pool)
    .await
    .unwrap();
}

/// A fixed snapshot (it names its pending rows) at `seq`, holding the log
/// up to `rev`, created now.
async fn fixed(s: &PgSnapshotStore, doc: uuid::Uuid, seq: i64, rev: i64) {
    assert!(
        s.insert_stamped(doc, stamp(seq, rev, 0), b"fixed", b"sv", Some(&[]))
            .await
            .unwrap()
    );
}

/// A fixed snapshot written through raw SQL `age` ago (an UPDATE of a
/// stamped row is skipped by the `doc_snapshots_epoch` trigger).
async fn fixed_aged(pool: &sqlx::PgPool, doc: uuid::Uuid, seq: i64, rev: i64, age: &str) {
    sqlx::query(
        "INSERT INTO doc_snapshots
             (doc_id, snapshot_seq, state_bytes, state_vector, snapshot_rev, epoch,
              pending_revs, created_at)
         VALUES ($1, $2, 'fixed', 'sv', $3, 0, '{}', now() - $4::interval)",
    )
    .bind(doc)
    .bind(seq)
    .bind(rev)
    .bind(age)
    .execute(pool)
    .await
    .unwrap();
}

/// The page's snapshots as `(snapshot_rev, snapshot_seq)`, in that order.
async fn kept(pool: &sqlx::PgPool, doc: uuid::Uuid) -> Vec<(Option<i64>, i64)> {
    sqlx::query_as(
        "SELECT snapshot_rev, snapshot_seq FROM doc_snapshots WHERE doc_id = $1
         ORDER BY snapshot_rev NULLS FIRST, snapshot_seq",
    )
    .bind(doc)
    .fetch_all(pool)
    .await
    .unwrap()
}

fn revs(kept: &[(Option<i64>, i64)]) -> Vec<i64> {
    kept.iter().filter_map(|(rev, _)| *rev).collect()
}

/// Every snapshot of `doc` was created before its genesis checkpoint.
async fn all_before_the_genesis(pool: &sqlx::PgPool, doc: uuid::Uuid) -> bool {
    sqlx::query_scalar(
        "SELECT bool_and(s.created_at < c.created_at)
           FROM doc_snapshots s
           JOIN doc_history_checkpoints c ON c.doc_id = s.doc_id AND c.kind = 'genesis'
          WHERE s.doc_id = $1",
    )
    .bind(doc)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn thinning_keeps_the_three_newest_in_boot_order() {
    let (s, pool, doc) = setup_with_pool().await;
    current(&pool, doc, 7, "2 days").await;
    for rev in 1..=7 {
        fixed(&s, doc, rev, rev).await;
    }
    // Legacy rows at higher seqs: still before every stamped row in boot
    // order.
    s.insert(doc, 100, b"legacy", b"sv").await.unwrap();
    s.insert(doc, 200, b"legacy", b"sv").await.unwrap();
    let latest = s.latest(doc).await.unwrap();

    assert_eq!(s.thin(doc, policy()).await.unwrap(), 6);
    assert_eq!(
        kept(&pool, doc).await,
        vec![(Some(5), 5), (Some(6), 6), (Some(7), 7)]
    );
    assert_eq!(s.latest(doc).await.unwrap(), latest);
}

#[tokio::test(flavor = "multi_thread")]
async fn thinning_keeps_snapshots_the_builder_has_not_probed() {
    let (s, pool, doc) = setup_with_pool().await;
    current(&pool, doc, 2, "2 days").await;
    for rev in 1..=7 {
        fixed(&s, doc, rev, rev).await;
    }
    assert_eq!(s.thin(doc, policy()).await.unwrap(), 2);
    assert_eq!(revs(&kept(&pool, doc).await), vec![3, 4, 5, 6, 7]);

    // A rev the builder skipped is not waiting to be probed.
    set_state(&pool, doc, "skip_snapshots = '{3}'").await;
    assert_eq!(s.thin(doc, policy()).await.unwrap(), 1);
    assert_eq!(revs(&kept(&pool, doc).await), vec![4, 5, 6, 7]);
}

#[tokio::test(flavor = "multi_thread")]
async fn thinning_keeps_at_most_32_unprobed() {
    let (s, pool, doc) = setup_with_pool().await;
    current(&pool, doc, 0, "2 days").await;
    for rev in 1..=40 {
        fixed(&s, doc, rev, rev).await;
    }
    // A second snapshot of rev 1, at a higher seq: the one the builder
    // would probe.
    fixed(&s, doc, 1000, 1).await;
    assert_eq!(kept(&pool, doc).await.len(), 41);

    assert_eq!(s.thin(doc, policy()).await.unwrap(), 6);
    let left = kept(&pool, doc).await;
    let want: Vec<i64> = (1..=32).chain(38..=40).collect();
    assert_eq!(revs(&left), want);
    assert_eq!(left[0], (Some(1), 1000), "rev 1 keeps its higher seq");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_given_up_page_keeps_what_its_rebuild_has_yet_to_probe() {
    let (s, pool, doc) = setup_with_pool().await;
    current(&pool, doc, 5, "2 days").await;
    set_state(&pool, doc, "retry_after = 'infinity'").await;
    for rev in 1..=10 {
        fixed(&s, doc, rev, rev).await;
    }
    assert_eq!(s.thin(doc, policy()).await.unwrap(), 5);
    assert_eq!(revs(&kept(&pool, doc).await), (6..=10).collect::<Vec<_>>());

    // Rows written before its genesis, none probed: all kept, as on a page
    // that is not given up.
    let other = another_page(&pool, doc).await;
    for rev in 1..=40 {
        fixed_aged(&pool, other, rev, rev, "1 hour").await;
    }
    current(&pool, other, 0, "0 seconds").await;
    set_state(&pool, other, "retry_after = 'infinity'").await;
    assert!(all_before_the_genesis(&pool, other).await);
    assert_eq!(s.thin(other, policy()).await.unwrap(), 0);
    assert_eq!(kept(&pool, other).await.len(), 40);
}

#[tokio::test(flavor = "multi_thread")]
async fn thinning_keeps_the_trusted_snapshot() {
    let (s, pool, doc) = setup_with_pool().await;
    current(&pool, doc, 4, "2 days").await;
    fixed(&s, doc, 1, 1).await;
    // Written after the migration without `pending_revs`: an older pod's.
    for rev in 2..=4 {
        s.insert_stamped(doc, stamp(rev, rev, 0), b"older pod", b"sv", None)
            .await
            .unwrap();
    }
    assert_eq!(s.thin(doc, policy()).await.unwrap(), 0);
    assert_eq!(revs(&kept(&pool, doc).await), vec![1, 2, 3, 4]);
    let trusted = s.latest_trusted(doc).await.unwrap().unwrap();
    assert_eq!(trusted.snapshot_rev, Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn thinning_a_page_not_built_by_this_version_deletes_what_gc_deletes() {
    let (s, pool, doc) = setup_with_pool().await;
    let older = doc;
    state_row(&pool, older, BV - 1, Some(0), 12).await;
    let no_genesis = another_page(&pool, doc).await;
    state_row(&pool, no_genesis, BV, None, 0).await;
    let no_state = another_page(&pool, doc).await;
    let by_gc = another_page(&pool, doc).await;
    for page in [older, no_genesis, no_state, by_gc] {
        fixed(&s, page, 1, 1).await;
        for rev in 2..=12 {
            s.insert_stamped(page, stamp(rev, rev, 0), b"older pod", b"sv", None)
                .await
                .unwrap();
        }
    }

    assert_eq!(s.gc(by_gc, 5, 30).await.unwrap(), 6);
    let want = kept(&pool, by_gc).await;
    assert_eq!(revs(&want), vec![1, 8, 9, 10, 11, 12]);
    for page in [older, no_genesis, no_state] {
        assert_eq!(s.thin(page, policy()).await.unwrap(), 6);
        assert_eq!(kept(&pool, page).await, want);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn thinning_keeps_what_the_rebuild_has_yet_to_probe() {
    let (s, pool, doc) = setup_with_pool().await;
    for rev in 1..=40 {
        fixed_aged(&pool, doc, rev, rev, "1 hour").await;
    }
    current(&pool, doc, 0, "0 seconds").await;
    assert!(all_before_the_genesis(&pool, doc).await);
    assert_eq!(s.thin(doc, policy()).await.unwrap(), 0);
    assert_eq!(kept(&pool, doc).await.len(), 40);

    set_state(&pool, doc, "snapshot_checked_rev = 40").await;
    assert_eq!(s.thin(doc, policy()).await.unwrap(), 37);
    assert_eq!(revs(&kept(&pool, doc).await), vec![38, 39, 40]);
    let trusted = s.latest_trusted(doc).await.unwrap().unwrap();
    assert_eq!(trusted.snapshot_rev, Some(40));
}
