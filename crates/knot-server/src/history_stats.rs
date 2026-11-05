//! `knot-server history-stats`: measure how fast the page-history log grows.
//! Read-only; never migrates.

use anyhow::Context as _;
use sqlx::PgPool;

#[derive(Debug)]
pub struct HistoryStats {
    pub update_rows: i64,
    pub update_bytes_total: i64,
    pub update_bytes_avg: f64,
    pub rows_per_page_day_p50: f64,
    pub rows_per_page_day_p95: f64,
    pub rows_per_page_day_max: i64,
    pub pages_with_updates: i64,
    pub baseline_rows: i64,
    pub baseline_bytes_total: i64,
    pub doc_updates_relation_bytes: i64,
}

/// Collects the growth numbers. The per-page-per-day rates cover the last
/// `days` days; everything else covers all rows.
pub async fn collect(pool: &PgPool, days: i32) -> Result<HistoryStats, sqlx::Error> {
    let (update_rows, update_bytes_total, update_bytes_avg, pages_with_updates): (
        i64,
        i64,
        f64,
        i64,
    ) = sqlx::query_as(
        "SELECT count(*),
                COALESCE(sum(octet_length(update_bytes)), 0)::int8,
                COALESCE(avg(octet_length(update_bytes)), 0)::float8,
                count(DISTINCT doc_id)
         FROM doc_updates",
    )
    .fetch_one(pool)
    .await?;

    let (p50, p95, max): (f64, f64, i64) = sqlx::query_as(
        "SELECT COALESCE(percentile_cont(0.5) WITHIN GROUP (ORDER BY n), 0)::float8,
                COALESCE(percentile_cont(0.95) WITHIN GROUP (ORDER BY n), 0)::float8,
                COALESCE(max(n), 0)::int8
         FROM (SELECT count(*) AS n
               FROM doc_updates
               WHERE created_at >= now() - make_interval(days => $1)
               GROUP BY doc_id, date_trunc('day', created_at)) per_day",
    )
    .bind(days)
    .fetch_one(pool)
    .await?;

    let (baseline_rows, baseline_bytes_total): (i64, i64) = sqlx::query_as(
        "SELECT count(*),
                COALESCE(sum(octet_length(state_bytes) + octet_length(state_vector)), 0)::int8
         FROM doc_history_baselines",
    )
    .fetch_one(pool)
    .await?;

    let doc_updates_relation_bytes: i64 =
        sqlx::query_scalar("SELECT COALESCE(pg_total_relation_size('doc_updates'), 0)::int8")
            .fetch_one(pool)
            .await?;

    Ok(HistoryStats {
        update_rows,
        update_bytes_total,
        update_bytes_avg,
        rows_per_page_day_p50: p50,
        rows_per_page_day_p95: p95,
        rows_per_page_day_max: max,
        pages_with_updates,
        baseline_rows,
        baseline_bytes_total,
        doc_updates_relation_bytes,
    })
}

/// 1536 -> "1.5 KiB".
fn human(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub async fn run(cfg: knot_config::Config, days: i32) -> anyhow::Result<()> {
    if cfg.database_url.is_empty() {
        anyhow::bail!("KNOT_DATABASE_URL must be set");
    }
    // Not knot_storage::connect: that runs migrations.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect(&cfg.database_url)
        .await
        .context("database connect")?;
    let s = collect(&pool, days).await.context("collect stats")?;
    println!("update_rows: {}", s.update_rows);
    println!(
        "update_bytes_total: {} ({})",
        s.update_bytes_total,
        human(s.update_bytes_total as f64)
    );
    println!(
        "update_bytes_avg: {:.1} ({})",
        s.update_bytes_avg,
        human(s.update_bytes_avg)
    );
    println!(
        "rows_per_page_day_p50 (last {days}d): {:.1}",
        s.rows_per_page_day_p50
    );
    println!(
        "rows_per_page_day_p95 (last {days}d): {:.1}",
        s.rows_per_page_day_p95
    );
    println!(
        "rows_per_page_day_max (last {days}d): {}",
        s.rows_per_page_day_max
    );
    println!("pages_with_updates: {}", s.pages_with_updates);
    println!("baseline_rows: {}", s.baseline_rows);
    println!(
        "baseline_bytes_total: {} ({})",
        s.baseline_bytes_total,
        human(s.baseline_bytes_total as f64)
    );
    println!(
        "doc_updates_relation_bytes: {} ({})",
        s.doc_updates_relation_bytes,
        human(s.doc_updates_relation_bytes as f64)
    );
    Ok(())
}
