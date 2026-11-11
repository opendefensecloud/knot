//! `knot_history_table_bytes{table}` (page history M1.3, Task 10).
//! Current-thread tests: the local recorder is thread-local.

mod common;

use common::metrics::CountingRecorder;
use knot_history::sizes::{HISTORY_TABLES, SIZE_LOCK_KEY, sample_table_sizes};

fn table_bytes(rec: &CountingRecorder, table: &str) -> Option<f64> {
    rec.labeled_gauge("knot_history_table_bytes", &[("table", table)])
}

#[tokio::test]
async fn table_sizes_are_sampled_once_per_lock_holder() {
    let rec = CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&rec);
    let db = knot_test_support::fresh_db().await;
    let pool = &db.pool;

    // Another replica holds the lock: this one samples nothing.
    let mut other = pool.acquire().await.unwrap().detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SIZE_LOCK_KEY)
        .fetch_one(&mut other)
        .await
        .unwrap();
    assert!(locked);
    assert!(!sample_table_sizes(pool).await.unwrap());
    for t in HISTORY_TABLES {
        assert_eq!(table_bytes(&rec, t), None, "{t} sampled without the lock");
    }

    // Released: this replica samples every table.
    let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(SIZE_LOCK_KEY)
        .fetch_one(&mut other)
        .await
        .unwrap();
    assert!(unlocked);
    assert!(sample_table_sizes(pool).await.unwrap());
    for t in HISTORY_TABLES {
        let bytes = table_bytes(&rec, t).unwrap_or_else(|| panic!("{t} not sampled"));
        // Even an empty table has its primary key's metapage.
        assert!(bytes > 0.0, "{t}: {bytes}");
    }

    // The sample let go of the lock: the next one (or another replica) gets it.
    assert!(sample_table_sizes(pool).await.unwrap());
    let relocked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SIZE_LOCK_KEY)
        .fetch_one(&mut other)
        .await
        .unwrap();
    assert!(relocked, "the sampler kept the lock");
}
