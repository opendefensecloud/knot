use knot_storage::{
    DocStore, PgDocStore, PgUpdatesStore, PgUserStore, PgWorkspaceStore, UpdatesStore, UserStore,
    WorkspaceRole, WorkspaceStore,
};
async fn setup() -> (PgUpdatesStore, sqlx::PgPool, uuid::Uuid, uuid::Uuid) {
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
    (PgUpdatesStore::new(pool.clone()), pool, d.id, u.id)
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_batch_returns_monotone_seqs_in_input_order() {
    let (s, _pool, doc, user) = setup().await;
    let batch = vec![
        (Some(user), vec![1u8, 2, 3]),
        (Some(user), vec![4u8, 5]),
        (Some(user), vec![6u8]),
    ];
    let seqs = s.insert_batch(doc, &batch).await.unwrap();
    assert_eq!(seqs.len(), 3);
    assert!(seqs[0] < seqs[1] && seqs[1] < seqs[2], "got {seqs:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn since_returns_after_watermark_in_order() {
    let (s, _pool, doc, user) = setup().await;
    let seqs = s
        .insert_batch(
            doc,
            &[
                (Some(user), vec![1u8]),
                (Some(user), vec![2u8]),
                (Some(user), vec![3u8]),
            ],
        )
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
    let (s, _pool, doc, user) = setup().await;
    assert_eq!(s.max_seq(doc).await.unwrap(), 0);
    let seqs = s
        .insert_batch(doc, &[(Some(user), vec![1u8]), (Some(user), vec![2u8])])
        .await
        .unwrap();
    assert_eq!(s.max_seq(doc).await.unwrap(), *seqs.last().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_survive_and_direct_delete_is_refused() {
    let (s, pool, doc, user) = setup().await;
    let seqs = s
        .insert_batch(
            doc,
            &[
                (Some(user), vec![1u8]),
                (Some(user), vec![2u8]),
                (Some(user), vec![3u8]),
            ],
        )
        .await
        .unwrap();
    let e = sqlx::query("DELETE FROM doc_updates WHERE doc_id = $1 AND seq <= $2")
        .bind(doc)
        .bind(seqs[1])
        .execute(&pool)
        .await
        .unwrap_err();
    assert_eq!(
        e.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("KH001")
    );
    assert_eq!(s.since(doc, 0).await.unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_batch_is_noop() {
    let (s, _pool, doc, _) = setup().await;
    let seqs = s.insert_batch(doc, &[]).await.unwrap();
    assert!(seqs.is_empty());
    assert_eq!(s.max_seq(doc).await.unwrap(), 0);
}

// ---------------------------------------------------------------------------
// Per-row authors + doc_contributors
// ---------------------------------------------------------------------------

struct Authors {
    s: PgUpdatesStore,
    pool: sqlx::PgPool,
    doc: uuid::Uuid,
    alice: uuid::Uuid,
    bob: uuid::Uuid,
}

async fn two_authors() -> Authors {
    let pool = knot_test_support::fresh_db().await.pool;
    let ws = PgWorkspaceStore::new(pool.clone())
        .create("default", "W")
        .await
        .unwrap();
    let users = PgUserStore::new(pool.clone());
    let alice = users
        .create_local("a@x.test", "Alice", "$h$")
        .await
        .unwrap();
    let bob = users.create_local("b@x.test", "Bob", "$h$").await.unwrap();
    for u in [&alice, &bob] {
        PgWorkspaceStore::new(pool.clone())
            .add_member(ws.id, u.id, WorkspaceRole::Editor)
            .await
            .unwrap();
    }
    let d = PgDocStore::new(pool.clone())
        .create(ws.id, None, "D", "m", alice.id)
        .await
        .unwrap();
    Authors {
        s: PgUpdatesStore::new(pool.clone()),
        pool,
        doc: d.id,
        alice: alice.id,
        bob: bob.id,
    }
}

type ContributorRow = (
    uuid::Uuid,
    chrono::DateTime<chrono::Utc>,
    chrono::DateTime<chrono::Utc>,
);

async fn contributors(pool: &sqlx::PgPool, doc: uuid::Uuid) -> Vec<ContributorRow> {
    sqlx::query_as(
        "SELECT user_id, first_edited_at, last_edited_at
         FROM doc_contributors WHERE doc_id = $1 ORDER BY user_id",
    )
    .bind(doc)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The writer batches whatever arrived in a 250 ms window. With live edits
/// attributed, two people typing at once land in one batch — each row must
/// keep its own author, and seqs must still line up with the input order.
#[tokio::test(flavor = "multi_thread")]
async fn insert_batch_keeps_each_rows_own_author() {
    let a = two_authors().await;
    let batch = vec![
        (Some(a.alice), vec![1u8]),
        (Some(a.bob), vec![2u8]),
        (None, vec![3u8]),
        (Some(a.alice), vec![4u8]),
    ];
    let seqs = a.s.insert_batch(a.doc, &batch).await.unwrap();
    let rows = a.s.since(a.doc, 0).await.unwrap();
    let got: Vec<_> = rows
        .iter()
        .map(|r| (r.seq, r.by_user_id, r.update_bytes.clone()))
        .collect();
    let want: Vec<_> = seqs
        .iter()
        .zip(&batch)
        .map(|(seq, (by, bytes))| (*seq, *by, bytes.clone()))
        .collect();
    assert_eq!(got, want, "rows must keep their own author, in input order");
}

#[tokio::test(flavor = "multi_thread")]
async fn insert_batch_records_one_contributor_per_distinct_author() {
    let a = two_authors().await;
    a.s.insert_batch(
        a.doc,
        &[
            (Some(a.alice), vec![1u8]),
            (Some(a.bob), vec![2u8]),
            (None, vec![3u8]),
            (Some(a.alice), vec![4u8]),
        ],
    )
    .await
    .unwrap();
    let rows = contributors(&a.pool, a.doc).await;
    let mut who: Vec<_> = rows.iter().map(|r| r.0).collect();
    let mut want = vec![a.alice, a.bob];
    who.sort();
    want.sort();
    assert_eq!(who, want, "one doc_contributors row per distinct author");
    for (user, first, last) in &rows {
        assert_eq!(
            first, last,
            "a new contributor starts with first = last ({user})"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn anonymous_batch_records_no_contributor() {
    let a = two_authors().await;
    a.s.insert_batch(a.doc, &[(None, vec![1u8]), (None, vec![2u8])])
        .await
        .unwrap();
    assert!(
        contributors(&a.pool, a.doc).await.is_empty(),
        "unattributed updates must not create contributors"
    );
}

/// Continuous typing flushes every 250 ms; refreshing last_edited_at on each
/// flush would rewrite the row four times a second for no visible gain. It
/// is refreshed only once the stored value is more than a minute old.
#[tokio::test(flavor = "multi_thread")]
async fn contributor_last_edited_at_refresh_is_throttled_to_once_a_minute() {
    let a = two_authors().await;
    let one_edit = [(Some(a.alice), vec![1u8])];
    let edit = || a.s.insert_batch(a.doc, &one_edit);
    edit().await.unwrap();

    // Backdate to just inside the window: the next edit must leave it alone.
    let (recent, first): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "UPDATE doc_contributors
             SET last_edited_at = now() - interval '30 seconds',
                 first_edited_at = now() - interval '1 day'
             WHERE doc_id = $1 AND user_id = $2
             RETURNING last_edited_at, first_edited_at",
        )
        .bind(a.doc)
        .bind(a.alice)
        .fetch_one(&a.pool)
        .await
        .unwrap();
    edit().await.unwrap();
    let rows = contributors(&a.pool, a.doc).await;
    assert_eq!(
        rows,
        vec![(a.alice, first, recent)],
        "an edit within a minute of last_edited_at must not rewrite the row"
    );

    // Backdate past the window: the next edit refreshes last_edited_at only.
    let stale: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "UPDATE doc_contributors SET last_edited_at = now() - interval '61 seconds'
         WHERE doc_id = $1 AND user_id = $2
         RETURNING last_edited_at",
    )
    .bind(a.doc)
    .bind(a.alice)
    .fetch_one(&a.pool)
    .await
    .unwrap();
    edit().await.unwrap();
    let rows = contributors(&a.pool, a.doc).await;
    assert_eq!(rows.len(), 1);
    let (_, got_first, got_last) = rows[0];
    assert_eq!(got_first, first, "first_edited_at must never move");
    assert!(
        got_last > stale + chrono::Duration::seconds(60),
        "a stale last_edited_at must be refreshed to now(): {stale} -> {got_last}"
    );
}

/// Continuous typing flushes every 250 ms. A flush for an author credited
/// within the last minute must not even lock their contributor row: `ON
/// CONFLICT … DO UPDATE … WHERE` locks the row although the WHERE skips the
/// update, so concurrent flushes of the same doc (other replicas) would queue
/// on it — and a waiting insert holds an allocated seq while later seqs
/// commit, widening the window in which replay can see seqs out of order.
#[tokio::test(flavor = "multi_thread")]
async fn flush_for_a_freshly_credited_author_does_not_lock_their_row() {
    let a = two_authors().await;
    let one_edit = [(Some(a.alice), vec![1u8])];
    a.s.insert_batch(a.doc, &one_edit).await.unwrap();

    // Another session holds Alice's contributor row.
    let mut holder = a.pool.begin().await.unwrap();
    sqlx::query("SELECT 1 FROM doc_contributors WHERE doc_id = $1 AND user_id = $2 FOR UPDATE")
        .bind(a.doc)
        .bind(a.alice)
        .execute(&mut *holder)
        .await
        .unwrap();

    let flushed = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        a.s.insert_batch(a.doc, &one_edit),
    )
    .await;
    holder.rollback().await.unwrap();
    assert!(
        flushed.is_ok(),
        "the flush waited on the contributor row's lock"
    );
    flushed.unwrap().unwrap();
}

// ---------------------------------------------------------------------------
// Page history M1.2: revs, labels and epochs
// ---------------------------------------------------------------------------

use knot_storage::testing::db_error;
use knot_storage::{
    HistoryHead, NewUpdate, RowsAfter, UpdateSource, UpdatesStoreError, rows_in_range,
};
use knot_test_support::migrate::migrator_up_to;

fn new_row(
    by: Option<uuid::Uuid>,
    bytes: &[u8],
    source: UpdateSource,
    meta: Option<serde_json::Value>,
) -> NewUpdate {
    NewUpdate {
        by_user_id: by,
        bytes: bytes.to_vec(),
        source,
        meta,
    }
}

fn edit(by: Option<uuid::Uuid>, bytes: &[u8]) -> NewUpdate {
    new_row(by, bytes, UpdateSource::Edit, None)
}

/// (dirty, last_write_at) of the page's heads row.
async fn heads_flags(
    pool: &sqlx::PgPool,
    doc: uuid::Uuid,
) -> (bool, Option<chrono::DateTime<chrono::Utc>>) {
    sqlx::query_as("SELECT dirty, last_write_at FROM doc_history_heads WHERE doc_id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn row_count(pool: &sqlx::PgPool, doc: uuid::Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM doc_updates WHERE doc_id = $1")
        .bind(doc)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn sqlstate(e: &UpdatesStoreError) -> Option<String> {
    match e {
        UpdatesStoreError::Sqlx(e) => e
            .as_database_error()
            .and_then(|d| d.code())
            .map(|c| c.into_owned()),
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn append_assigns_consecutive_revs_and_stores_labels() {
    let a = two_authors().await;
    let suggestion = uuid::Uuid::new_v4();
    let meta = serde_json::json!({"suggestion_id": suggestion, "accepted_by": a.alice});
    let rows = vec![
        new_row(Some(a.alice), b"e", UpdateSource::Edit, None),
        new_row(Some(a.alice), b"t", UpdateSource::Task, None),
        new_row(
            Some(a.bob),
            b"s",
            UpdateSource::Suggestion,
            Some(meta.clone()),
        ),
    ];
    let persisted = a.s.append(a.doc, 0, &rows).await.unwrap();
    assert_eq!(
        persisted.iter().map(|p| p.rev).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        persisted.windows(2).all(|w| w[0].seq < w[1].seq),
        "seqs ascend with revs: {persisted:?}"
    );

    let got = a.s.rows_after(a.doc, RowsAfter::Rev(0)).await.unwrap();
    let got: Vec<_> = got
        .into_iter()
        .map(|r| {
            (
                r.seq,
                r.rev,
                r.by_user_id,
                r.update_bytes,
                r.source,
                r.meta,
                r.epoch,
            )
        })
        .collect();
    let want: Vec<_> = persisted
        .iter()
        .zip(&rows)
        .map(|(p, r)| {
            (
                p.seq,
                Some(p.rev),
                r.by_user_id,
                r.bytes.clone(),
                Some(r.source),
                r.meta.clone(),
                Some(0),
            )
        })
        .collect();
    assert_eq!(got, want);
    assert_eq!(
        a.s.head(a.doc).await.unwrap(),
        HistoryHead {
            head_rev: 3,
            legacy_rows: 0,
            content_epoch: 0
        }
    );
    let (dirty, last_write_at) = heads_flags(&a.pool, a.doc).await;
    assert!(dirty, "the first flush creates the heads row dirty");
    assert!(last_write_at.is_some(), "and stamps last_write_at");

    // As M1.3's builder will: the next flush goes through the ON CONFLICT arm.
    sqlx::query(
        "UPDATE doc_history_heads SET dirty = false, last_write_at = '2000-01-01'
          WHERE doc_id = $1",
    )
    .bind(a.doc)
    .execute(&a.pool)
    .await
    .unwrap();
    let persisted =
        a.s.append(a.doc, 0, &[edit(Some(a.bob), b"4"), edit(None, b"5")])
            .await
            .unwrap();
    assert_eq!(
        persisted.iter().map(|p| p.rev).collect::<Vec<_>>(),
        vec![4, 5]
    );
    let (dirty, last_write_at) = heads_flags(&a.pool, a.doc).await;
    assert!(dirty, "a flush marks the page dirty again");
    let old: chrono::DateTime<chrono::Utc> = "2000-01-01T00:00:00Z".parse().unwrap();
    assert!(last_write_at.unwrap() > old, "{last_write_at:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn append_with_a_stale_epoch_writes_nothing() {
    let (s, pool, doc, user) = setup().await;
    // First a row, so the heads row exists: on a fresh page the UPDATE below
    // would match nothing and the append would succeed for the wrong reason.
    s.append(doc, 0, &[edit(Some(user), b"1")]).await.unwrap();
    let bumped = sqlx::query("UPDATE doc_history_heads SET content_epoch = 1 WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(bumped.rows_affected(), 1);

    let e = s
        .append(doc, 0, &[edit(Some(user), b"2")])
        .await
        .unwrap_err();
    assert!(
        matches!(e, UpdatesStoreError::EpochChanged { room: 0, stored: 1 }),
        "{e:?}"
    );
    assert!(e.is_permanent());
    assert_eq!(row_count(&pool, doc).await, 1, "nothing more was written");
    assert_eq!(s.head(doc).await.unwrap().head_rev, 1);
}

const HISTORY_LOG_COLUMNS: i64 = 20261107120000;

/// A page whose `n` rows predate the heads migration (rev NULL), on a
/// database then migrated all the way. Legacy rows hold their ordinal as
/// bytes (`int4send`).
async fn legacy_page(n: i32) -> (PgUpdatesStore, sqlx::PgPool, uuid::Uuid) {
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap();
    migrator_up_to(HISTORY_LOG_COLUMNS)
        .run(&pool)
        .await
        .unwrap();
    let ws: uuid::Uuid =
        sqlx::query_scalar("INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id")
            .fetch_one(&pool)
            .await
            .unwrap();
    let user: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'A') RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    let doc: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', 'm', $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO doc_updates (doc_id, update_bytes)
         SELECT $1, int4send(g) FROM generate_series(1, $2) g",
    )
    .bind(doc)
    .bind(n)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::migrate!("../../migrations").run(&pool).await.unwrap();
    (PgUpdatesStore::new(pool.clone()), pool, doc)
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_after_all_returns_legacy_rows_by_seq_then_revs() {
    let (s, pool, doc) = legacy_page(4).await;
    assert_eq!(
        s.head(doc).await.unwrap(),
        HistoryHead {
            head_rev: 4,
            legacy_rows: 4,
            content_epoch: 0
        }
    );
    let appended = s
        .append(doc, 0, &[edit(None, b"new 5"), edit(None, b"new 6")])
        .await
        .unwrap();
    assert_eq!(
        appended.iter().map(|p| p.rev).collect::<Vec<_>>(),
        vec![5, 6]
    );

    let legacy_bytes: Vec<Vec<u8>> = (1..=4i32).map(|i| i.to_be_bytes().to_vec()).collect();
    let all = s.rows_after(doc, RowsAfter::All).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.rev).collect::<Vec<_>>(),
        vec![None, None, None, None, Some(5), Some(6)]
    );
    assert!(
        all[..4].windows(2).all(|w| w[0].seq < w[1].seq),
        "legacy rows by seq"
    );
    assert_eq!(
        all[..4]
            .iter()
            .map(|r| r.update_bytes.clone())
            .collect::<Vec<_>>(),
        legacy_bytes
    );
    assert!(
        all[..4]
            .iter()
            .all(|r| r.source.is_none() && r.epoch.is_none())
    );

    assert_eq!(s.ensure_legacy_numbered(doc).await.unwrap(), 4);
    let all = s.rows_after(doc, RowsAfter::All).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.rev).collect::<Vec<_>>(),
        (1..=6).map(Some).collect::<Vec<_>>()
    );
    assert!(all.windows(2).all(|w| w[0].seq < w[1].seq), "seq order");
    assert_eq!(
        all[..4]
            .iter()
            .map(|r| r.update_bytes.clone())
            .collect::<Vec<_>>(),
        legacy_bytes,
        "legacy rows numbered in seq order"
    );
    assert_eq!(s.ensure_legacy_numbered(doc).await.unwrap(), 0);
    assert_eq!(
        s.rows_after(doc, RowsAfter::Rev(4))
            .await
            .unwrap()
            .iter()
            .map(|r| r.rev)
            .collect::<Vec<_>>(),
        vec![Some(5), Some(6)]
    );

    // A page created after the upgrade has nothing to number.
    let fresh: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         SELECT workspace_id, 'N', 'n', created_by FROM documents WHERE id = $1
         RETURNING id",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        s.ensure_legacy_numbered(fresh).await.unwrap(),
        0,
        "no heads row"
    );
    s.append(fresh, 0, &[edit(None, b"x")]).await.unwrap();
    assert_eq!(s.ensure_legacy_numbered(fresh).await.unwrap(), 0);
    assert_eq!(
        s.head(uuid::Uuid::new_v4()).await.unwrap(),
        HistoryHead::default()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_numbering_count_mismatch_is_reported() {
    let (s, pool, doc) = legacy_page(4).await;
    sqlx::query("UPDATE doc_history_heads SET legacy_rows = 3 WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    let e = s.ensure_legacy_numbered(doc).await.unwrap_err();
    assert!(
        matches!(
            e,
            UpdatesStoreError::LegacyCountMismatch {
                legacy_rows: 3,
                unnumbered: 4
            }
        ),
        "{e:?}"
    );
    assert!(e.is_permanent());
    let numbered: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM doc_updates WHERE doc_id = $1 AND rev IS NOT NULL",
    )
    .bind(doc)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(numbered, 0, "no row numbered");
}

/// Legacy rows someone numbered by hand, not as `1..=legacy_rows`, leave no
/// row unnumbered and no row at `rev = legacy_rows`. That is a mismatch too:
/// booting would meet a rev gap at rev 1 and retry catch-up forever.
#[tokio::test(flavor = "multi_thread")]
async fn hand_numbered_legacy_rows_are_reported_as_a_mismatch() {
    let (s, pool, doc) = legacy_page(4).await;
    sqlx::query("UPDATE doc_updates SET rev = seq + 100 WHERE doc_id = $1")
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap();
    let e = s.ensure_legacy_numbered(doc).await.unwrap_err();
    assert!(
        matches!(
            e,
            UpdatesStoreError::LegacyCountMismatch {
                legacy_rows: 4,
                unnumbered: 0
            }
        ),
        "{e:?}"
    );
    assert!(e.is_permanent());
}

#[tokio::test(flavor = "multi_thread")]
async fn error_classification() {
    let (s, pool, doc, user) = setup().await;
    s.append(doc, 0, &[edit(Some(user), b"1")]).await.unwrap();
    let store_err = |e: sqlx::Error| UpdatesStoreError::Sqlx(e);

    // KH002: an explicit rev at the wrong epoch.
    let e = store_err(
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, rev, epoch) VALUES ($1, '\\x01', 2, 5)",
        )
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap_err(),
    );
    assert_eq!(sqlstate(&e).as_deref(), Some("KH002"), "{e}");
    assert!(e.is_permanent());

    // KH001: rewriting a row.
    let e = store_err(
        sqlx::query("UPDATE doc_updates SET by_user_id = NULL WHERE doc_id = $1")
            .bind(doc)
            .execute(&pool)
            .await
            .unwrap_err(),
    );
    assert_eq!(sqlstate(&e).as_deref(), Some("KH001"), "{e}");
    assert!(e.is_permanent());

    // 23505: a rev the page already has, at the right epoch.
    let e = store_err(
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, rev, epoch) VALUES ($1, '\\x01', 1, 0)",
        )
        .bind(doc)
        .execute(&pool)
        .await
        .unwrap_err(),
    );
    assert_eq!(sqlstate(&e).as_deref(), Some("23505"), "{e}");
    assert!(e.is_permanent());

    // A page that does not exist.
    let e = s
        .append(uuid::Uuid::new_v4(), 0, &[edit(None, b"x")])
        .await
        .unwrap_err();
    assert!(matches!(e, UpdatesStoreError::DocGone), "{e:?}");
    assert!(e.is_permanent());

    let permanent = |e: sqlx::Error| store_err(e).is_permanent();
    assert!(!permanent(sqlx::Error::PoolTimedOut));
    assert!(permanent(sqlx::Error::PoolClosed));
    assert!(permanent(sqlx::Error::RowNotFound));
    assert!(permanent(sqlx::Error::Decode("bad".into())));
    assert!(permanent(sqlx::Error::ColumnDecode {
        index: "0".into(),
        source: "bad".into()
    }));
    for code in [
        "KH001", "KH002", "23503", "23505", "22P02", "42703", "XX000",
    ] {
        assert!(permanent(db_error(code)), "{code} is permanent");
    }
    for code in ["40P01", "55P03", "57014", "53300", "08006"] {
        assert!(!permanent(db_error(code)), "{code} is transient");
    }
    assert!(UpdatesStoreError::DocGone.is_permanent());
    assert!(
        UpdatesStoreError::LegacyCountMismatch {
            legacy_rows: 1,
            unnumbered: 2
        }
        .is_permanent()
    );

    let shrinks = |e: sqlx::Error| store_err(e).shrinks_batch();
    assert!(shrinks(db_error("57014")));
    assert!(shrinks(db_error("54000")));
    assert!(shrinks(db_error("54001")));
    assert!(!shrinks(db_error("55P03")));
    assert!(!shrinks(db_error("40P01")));
    assert!(!shrinks(db_error("22P02")));
    assert!(!shrinks(sqlx::Error::PoolTimedOut));
    assert!(!UpdatesStoreError::DocGone.shrinks_batch());
}

#[test]
fn update_sources_round_trip_their_sql_spellings() {
    use std::str::FromStr;
    let all = [
        (UpdateSource::Edit, "edit"),
        (UpdateSource::Task, "task"),
        (UpdateSource::Suggestion, "suggestion"),
        (UpdateSource::Restore, "restore"),
        (UpdateSource::RestoreFallback, "restore_fallback"),
        (UpdateSource::Import, "import"),
        (UpdateSource::Template, "template"),
        (UpdateSource::Copy, "copy"),
    ];
    for (source, sql) in all {
        assert_eq!(source.as_str(), sql);
        assert_eq!(UpdateSource::from_str(sql).unwrap(), source);
    }
    assert!(UpdateSource::from_str("Edit").is_err());
    assert!(UpdateSource::from_str("").is_err());
}

/// `knot-server export-check` reads a staging copy that may never have been
/// migrated past 0.9.9: `since` and `latest` still work there.
#[tokio::test(flavor = "multi_thread")]
async fn since_and_latest_read_a_database_from_before_the_upgrade() {
    use knot_storage::{PgSnapshotStore, SnapshotStore};
    const V0_9_9_SCHEMA: i64 = 20261104120001;
    let url = knot_test_support::fresh_db_url().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .unwrap();
    migrator_up_to(V0_9_9_SCHEMA).run(&pool).await.unwrap();
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
    for (seq, bytes) in [(10i64, b"old".as_slice()), (20, b"new")] {
        sqlx::query(
            "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector)
             VALUES ($1, $2, $3, 'sv')",
        )
        .bind(doc)
        .bind(seq)
        .bind(bytes)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO doc_updates (doc_id, update_bytes) VALUES ($1, '\\x01'), ($1, '\\x02')",
    )
    .bind(doc)
    .execute(&pool)
    .await
    .unwrap();

    let latest = PgSnapshotStore::new(pool.clone())
        .latest(doc)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            latest.snapshot_seq,
            latest.state_bytes.as_slice(),
            latest.snapshot_rev
        ),
        (20, b"new".as_slice(), None)
    );
    let rows = PgUpdatesStore::new(pool).since(doc, 0).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.update_bytes.clone())
            .collect::<Vec<_>>(),
        vec![vec![1u8], vec![2u8]]
    );
}

/// The revs `rows_in_range` returns for these bounds.
async fn revs_in_range(
    conn: &mut sqlx::PgConnection,
    doc: uuid::Uuid,
    after_rev: i64,
    through_rev: Option<i64>,
    max_rows: i64,
    max_bytes: i64,
) -> Vec<i64> {
    rows_in_range(conn, doc, after_rev, through_rev, max_rows, max_bytes)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.rev.expect("a numbered row"))
        .collect()
}

/// `rows_in_range` returns the numbered rows `after_rev < rev <= through_rev`
/// in rev order, at most `max_rows` of them, and stops before the row whose
/// bytes would take the total over `max_bytes`; the first row comes back
/// even when it alone is over the limit, so every caller gets a row.
#[tokio::test(flavor = "multi_thread")]
async fn rows_in_range_caps_rows_and_bytes() {
    const ANY: i64 = i64::MAX;
    let (s, pool, doc, user) = setup().await;
    let rows: Vec<NewUpdate> = (1..=5u8).map(|i| edit(Some(user), &[i; 10])).collect();
    s.append(doc, 0, &rows).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let c = &mut *conn;

    assert_eq!(
        revs_in_range(c, doc, 0, None, ANY, ANY).await,
        [1, 2, 3, 4, 5]
    );
    assert_eq!(revs_in_range(c, doc, 0, None, 3, ANY).await, [1, 2, 3]);
    assert_eq!(
        revs_in_range(c, doc, 0, None, ANY, 25).await,
        [1, 2],
        "the third would cross 25"
    );
    assert_eq!(
        revs_in_range(c, doc, 0, None, ANY, 30).await,
        [1, 2, 3],
        "exactly 30"
    );
    assert_eq!(
        revs_in_range(c, doc, 0, None, ANY, 1).await,
        [1],
        "a first row over the limit"
    );
    assert_eq!(revs_in_range(c, doc, 0, Some(2), ANY, ANY).await, [1, 2]);
    assert_eq!(revs_in_range(c, doc, 2, None, ANY, ANY).await, [3, 4, 5]);
    assert_eq!(revs_in_range(c, doc, 2, Some(4), ANY, 15).await, [3]);
    assert_eq!(revs_in_range(c, doc, 2, Some(4), 1, ANY).await, [3]);
    assert!(revs_in_range(c, doc, 5, None, ANY, ANY).await.is_empty());
    assert!(revs_in_range(c, doc, 2, Some(2), ANY, ANY).await.is_empty());

    // Each row comes back whole, as `rows_after` maps it.
    let got = rows_in_range(c, doc, 2, Some(3), ANY, ANY).await.unwrap();
    let want = s.rows_after(doc, RowsAfter::Rev(2)).await.unwrap();
    assert_eq!(got, want[..1]);
    assert_eq!(got[0].update_bytes, vec![3u8; 10]);
}

/// Legacy rows are not returned until `ensure_legacy_numbered` gives them
/// their revs.
#[tokio::test(flavor = "multi_thread")]
async fn rows_in_range_leaves_out_unnumbered_legacy_rows() {
    let (s, pool, doc) = legacy_page(3).await;
    s.append(doc, 0, &[edit(None, b"4"), edit(None, b"5")])
        .await
        .unwrap();
    let mut conn = pool.acquire().await.unwrap();
    assert_eq!(
        revs_in_range(&mut conn, doc, 0, None, i64::MAX, i64::MAX).await,
        [4, 5]
    );
    s.ensure_legacy_numbered(doc).await.unwrap();
    assert_eq!(
        revs_in_range(&mut conn, doc, 0, None, i64::MAX, i64::MAX).await,
        [1, 2, 3, 4, 5]
    );
}
