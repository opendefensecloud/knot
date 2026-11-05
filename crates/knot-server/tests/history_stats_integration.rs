//! `knot-server history-stats`: log growth numbers over seeded rows.

use sqlx::PgPool;
use uuid::Uuid;

async fn seed_doc(pool: &PgPool, ws: Uuid, user: Uuid, sort_key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO documents (workspace_id, title, sort_key, created_by)
         VALUES ($1, 'D', $3, $2) RETURNING id",
    )
    .bind(ws)
    .bind(user)
    .bind(sort_key)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn seed_updates(pool: &PgPool, doc: Uuid, rows: i32, bytes: i32, days_ago: i32) {
    for _ in 0..rows {
        sqlx::query(
            "INSERT INTO doc_updates (doc_id, update_bytes, created_at)
             VALUES ($1, decode(repeat('ab', $2), 'hex'), date_trunc('day', now()) + interval '12 hours' - make_interval(days => $3))",
        )
        .bind(doc)
        .bind(bytes)
        .bind(days_ago)
        .execute(pool)
        .await
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn history_stats_reports_rows_bytes_and_daily_rates() {
    let db = knot_test_support::fresh_db().await;
    let pool = &db.pool;
    let ws: Uuid = sqlx::query_scalar(
        "INSERT INTO workspaces (slug, name) VALUES ('default', 'W') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let user: Uuid = sqlx::query_scalar(
        "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'Alice') RETURNING id",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    let a = seed_doc(pool, ws, user, "a").await;
    let b = seed_doc(pool, ws, user, "b").await;
    seed_updates(pool, a, 4, 10, 0).await;
    seed_updates(pool, a, 2, 10, 1).await;
    seed_updates(pool, b, 1, 40, 0).await;
    // Outside the 30 day window: counts in the totals, not in the daily rates.
    // So p50/p95/max stay 2.0/3.8/4: the in-window groups are still 4, 2 and 1.
    seed_updates(pool, b, 1, 20, 40).await;
    sqlx::query(
        "INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
         VALUES ($1, 1, decode(repeat('ab', 100), 'hex'), decode(repeat('cd', 7), 'hex'), now())",
    )
    .bind(a)
    .execute(pool)
    .await
    .unwrap();

    let s = knot_server::history_stats::collect(pool, 30).await.unwrap();
    // Rows: 4 + 2 + 1 + 1 old = 8. Bytes: 40 + 20 + 40 + 20 = 120.
    assert_eq!(s.update_rows, 8);
    assert_eq!(s.update_bytes_total, 120);
    assert!((s.update_bytes_avg - 120.0 / 8.0).abs() < 1e-9);
    assert_eq!(s.pages_with_updates, 2);
    assert_eq!(s.rows_per_page_day_max, 4);
    assert!((s.rows_per_page_day_p50 - 2.0).abs() < 1e-9); // groups: 4, 2, 1
    assert!((s.rows_per_page_day_p95 - 3.8).abs() < 1e-9); // percentile_cont, not _disc
    assert_eq!(s.baseline_rows, 1);
    assert_eq!(s.baseline_bytes_total, 107);
    assert!(s.doc_updates_relation_bytes > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_stats_on_empty_db_reports_zeros() {
    let db = knot_test_support::fresh_db().await;
    let s = knot_server::history_stats::collect(&db.pool, 30)
        .await
        .unwrap();
    assert_eq!(
        (
            s.update_rows,
            s.update_bytes_total,
            s.pages_with_updates,
            s.rows_per_page_day_max,
            s.baseline_rows,
            s.baseline_bytes_total
        ),
        (0, 0, 0, 0, 0, 0)
    );
    assert_eq!(
        (
            s.update_bytes_avg,
            s.rows_per_page_day_p50,
            s.rows_per_page_day_p95
        ),
        (0.0, 0.0, 0.0)
    );
}
