//! `knot_history_table_bytes{table}`: the on-disk size of each table page
//! history stores, sampled by one replica at a time.
//!
//! Every replica runs the sampler on its own 10-minute interval and samples
//! under `SIZE_LOCK_KEY` on a detached connection (the GC's pattern,
//! `knot_crdt::gc`). The lock is held only for the one size query, so it
//! keeps two samples from running at once, not replicas from sampling: a
//! replica skips only when another is sampling at that moment, and usually
//! every replica samples and exports the gauge. Dashboards read it with
//! `max`.

use std::time::Duration;

use sqlx::PgPool;

/// How often the sizes are sampled.
pub const SIZE_SAMPLE_EVERY: Duration = Duration::from_secs(10 * 60);

/// The session-level advisory lock a replica holds while it samples ("knoths" + 1).
pub const SIZE_LOCK_KEY: i64 = 0x6b6e_6f74_6873_0001;

/// The tables page history writes (M1.2's log and M1.3's builder output),
/// each reported as a `table` label.
pub const HISTORY_TABLES: [&str; 7] = [
    "doc_updates",
    "doc_snapshots",
    "doc_history_baselines",
    "doc_history_heads",
    "doc_history_state",
    "doc_history_checkpoints",
    "doc_history_entries",
];

/// The delay before the first sample after the process starts.
const FIRST_SAMPLE_AFTER: Duration = Duration::from_secs(60);

/// Sets `knot_history_table_bytes{table}` to `pg_total_relation_size` (heap,
/// indexes and TOAST) of each of [`HISTORY_TABLES`]. Returns `false`, setting
/// nothing, when another replica holds [`SIZE_LOCK_KEY`].
pub async fn sample_table_sizes(pool: &PgPool) -> Result<bool, sqlx::Error> {
    // A session-level lock on a connection taken out of the pool: dropping
    // it closes the session, which releases the lock even if this sample
    // fails part-way. A pooled connection would go back to the pool still
    // holding the lock and stop every later sample, on every replica.
    let mut conn = pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(SIZE_LOCK_KEY)
        .fetch_one(&mut conn)
        .await?;
    if !locked {
        return Ok(false);
    }
    // `to_regclass` so a missing table is skipped instead of failing the
    // whole sample.
    let sizes: Vec<(String, Option<i64>)> = sqlx::query_as(
        "SELECT t, pg_total_relation_size(to_regclass(t))::int8
           FROM unnest($1::text[]) AS t",
    )
    .bind(&HISTORY_TABLES[..])
    .fetch_all(&mut conn)
    .await?;
    for (table, bytes) in sizes {
        if let Some(bytes) = bytes {
            metrics::gauge!("knot_history_table_bytes", "table" => table).set(bytes as f64);
        }
    }
    // The drop only closes the socket, so the server would release the lock
    // asynchronously; unlock first so the next sample finds it free.
    if let Err(e) = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(SIZE_LOCK_KEY)
        .execute(&mut conn)
        .await
    {
        tracing::warn!(error = %e, "history sizes: releasing the lock failed");
    }
    Ok(true)
}

/// Samples the table sizes every [`SIZE_SAMPLE_EVERY`], the first time one
/// minute after it is spawned.
pub fn spawn_size_sampler(pool: PgPool) {
    tokio::spawn(async move {
        let start = tokio::time::Instant::now() + FIRST_SAMPLE_AFTER;
        let mut every = tokio::time::interval_at(start, SIZE_SAMPLE_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            if let Err(e) = sample_table_sizes(&pool).await {
                tracing::warn!(error = %e, "history sizes: sample failed");
            }
        }
    });
}
