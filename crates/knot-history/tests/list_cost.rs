//! The list's baseline section on a page with thousands of sessions (page
//! history M1.4, Task 14 Step 0; the M1.3 ledger deferred this measurement).
//!
//! The baseline section (`list.rs`, `kind = 'baseline' … ORDER BY ended_at
//! DESC, id DESC`) has no fitting index: it reads every entry of the page
//! through `doc_history_entries_log_idx` and sorts the baselines. This
//! measures what that costs with 5,000 sessions and 30 baselines on the page
//! and 50,000 entries on ten other pages. Under 20 ms: no index (the plan's
//! Ruling on the baseline-section index); above: the partial index
//! `(doc_id, ended_at DESC, id DESC) WHERE kind = 'baseline'` is a follow-up
//! for the next milestone that ships a migration.
//!
//! Run: `cargo nextest run -p knot-history --test list_cost --run-ignored only --no-capture`

mod common;

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// `list.rs`'s baseline-section query, as its `format!` sends it (with its
/// `COLUMNS` spelled out).
const BASELINE_SECTION: &str =
    "SELECT id, kind, author_id, started_at, ended_at, end_rev, sealed, \
                       words_added, words_removed, meta FROM doc_history_entries
              WHERE doc_id = $1 AND kind = 'baseline'
                AND ($2::timestamptz IS NULL OR (ended_at, id) < ($2, $3))
              ORDER BY ended_at DESC, id DESC
              LIMIT $4";

const RUNS: usize = 5;

/// Adds `n` sealed sessions by one author, `end_rev` 1..=n, to `doc`.
async fn sessions(db: &common::db::Db, doc: Uuid, n: i64) {
    sqlx::query(
        "INSERT INTO doc_history_entries
           (doc_id, kind, author_id, started_at, ended_at, start_rev, end_rev, sealed,
            words_added, words_removed)
         SELECT $1, 'session', $2,
                now() - make_interval(mins => ($3 - g)::int + 1),
                now() - make_interval(mins => ($3 - g)::int),
                g, g, true, 1, 0
           FROM generate_series(1, $3) g",
    )
    .bind(doc)
    .bind(common::author(0))
    .bind(n)
    .execute(&db.pool)
    .await
    .expect("sessions");
}

/// One plan: its node types, top down, and its `Execution Time` in ms.
async fn explain(
    db: &common::db::Db,
    doc: Uuid,
    cursor: Option<(DateTime<Utc>, i64)>,
) -> (Vec<String>, f64) {
    let (c_at, c_id) = match cursor {
        Some((at, id)) => (Some(at), id),
        None => (None, i64::MAX),
    };
    let plan: Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (ANALYZE, FORMAT JSON) {BASELINE_SECTION}"
    )))
    .bind(doc)
    .bind(c_at)
    .bind(c_id)
    .bind(knot_history::list::DEFAULT_LIMIT + 1)
    .fetch_one(&db.pool)
    .await
    .expect("explain");
    let top = &plan[0];
    let mut nodes = Vec::new();
    let mut stack = vec![&top["Plan"]];
    while let Some(node) = stack.pop() {
        let mut name = node["Node Type"].as_str().unwrap_or("?").to_string();
        if let Some(index) = node["Index Name"].as_str() {
            name = format!("{name} ({index})");
        }
        nodes.push(name);
        if let Some(children) = node["Plans"].as_array() {
            stack.extend(children.iter().rev());
        }
    }
    let ms = top["Execution Time"].as_f64().expect("Execution Time");
    (nodes, ms)
}

/// The median of `RUNS` executions, and the plan of the last one.
async fn median(
    db: &common::db::Db,
    doc: Uuid,
    cursor: Option<(DateTime<Utc>, i64)>,
) -> (Vec<String>, f64) {
    let mut times = Vec::with_capacity(RUNS);
    let mut nodes = Vec::new();
    for _ in 0..RUNS {
        let (n, ms) = explain(db, doc, cursor).await;
        nodes = n;
        times.push(ms);
    }
    times.sort_by(f64::total_cmp);
    (nodes, times[RUNS / 2])
}

#[tokio::test]
#[ignore = "measurement"]
async fn baseline_section_cost() {
    let db = common::db::db().await;
    let doc = db.page().await;

    sessions(&db, doc, 5_000).await;
    db.exec(
        "INSERT INTO doc_history_entries
           (doc_id, kind, author_id, started_at, ended_at, sealed)
         SELECT $1, 'baseline', NULL,
                now() - make_interval(days => 365 - (g - 1) * 12),
                now() - make_interval(days => 365 - (g - 1) * 12),
                true
           FROM generate_series(1, 30) g",
        doc,
    )
    .await;
    for _ in 0..10 {
        let other = db.page().await;
        sessions(&db, other, 5_000).await;
    }
    sqlx::query("ANALYZE doc_history_entries")
        .execute(&db.pool)
        .await
        .expect("analyze");

    // The cursor of a second baseline page: the 10th newest baseline.
    let cursor: (DateTime<Utc>, i64) = sqlx::query_as(
        "SELECT ended_at, id FROM doc_history_entries
          WHERE doc_id = $1 AND kind = 'baseline'
          ORDER BY ended_at DESC, id DESC
          OFFSET 9 LIMIT 1",
    )
    .bind(doc)
    .fetch_one(&db.pool)
    .await
    .expect("cursor");

    let (first_nodes, first_ms) = median(&db, doc, None).await;
    let (cursor_nodes, cursor_ms) = median(&db, doc, Some(cursor)).await;
    println!(
        "baseline section, first page ($2 NULL): {first_ms:.3} ms (median of {RUNS}); plan: {}",
        first_nodes.join(" > ")
    );
    println!(
        "baseline section, with a cursor: {cursor_ms:.3} ms (median of {RUNS}); plan: {}",
        cursor_nodes.join(" > ")
    );
}
