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
    let (s, doc, user) = setup().await;
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
    let (s, doc, user) = setup().await;
    assert_eq!(s.max_seq(doc).await.unwrap(), 0);
    let seqs = s
        .insert_batch(doc, &[(Some(user), vec![1u8]), (Some(user), vec![2u8])])
        .await
        .unwrap();
    assert_eq!(s.max_seq(doc).await.unwrap(), *seqs.last().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn delete_up_to_removes_inclusive() {
    let (s, doc, user) = setup().await;
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
    let n = s.delete_up_to(doc, seqs[1]).await.unwrap();
    assert_eq!(n, 2);
    let left = s.since(doc, 0).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].seq, seqs[2]);
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_batch_is_noop() {
    let (s, doc, _) = setup().await;
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
