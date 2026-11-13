//! audit_events writers (best effort on the pool, fail closed for exports,
//! transactional with `_in_tx`) and the per-page reads.
//!
//! [`record`] and [`record_with_data`] never fail their caller: a failed
//! insert is logged and counted in `knot_audit_write_failures_total{action}`,
//! and the request goes on. [`record_for_targets`], the export writer, logs
//! and counts the same way but hands the error back: an export that can't
//! be recorded, or whose insert takes longer than
//! [`EXPORT_AUDIT_WRITE_TIMEOUT`], is refused (`503 audit.unavailable`),
//! never released unaudited. The `_in_tx` writers return the error, so the mutation they
//! audit rolls back with them.
//!
//! The reads serve the page audit log (`GET /api/docs/{id}/audit`): one
//! page's events newest first ([`list_doc_events`]) and its counts
//! ([`doc_stats`]), all on `audit_events_target_idx (target_id, id)`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::types::JsonValue;
use sqlx::{PgConnection, PgPool};
use thiserror::Error;
use uuid::Uuid;

/// How long [`record_for_targets`] may take, connection included, before it
/// gives up and the export is refused. Well inside the 30 s request
/// timeout, so an insert that hangs (a lock on `audit_events`, a lost
/// synchronous standby) ends in a counted `503 audit.unavailable` rather
/// than an uncounted 408. The insert also runs under a `statement_timeout`
/// of the same length, so Postgres cancels it rather than letting it land
/// later for a file that was never sent.
pub const EXPORT_AUDIT_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why [`record_for_targets`] didn't record an export.
#[derive(Debug, Error)]
pub enum AuditWriteError {
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("the audit write took longer than {0:?}")]
    TimedOut(Duration),
}

/// Counts one audit insert that failed, best effort or not. `action` is a
/// code constant, never a user value, so the label stays bounded.
fn count_write_failure(action: &str) {
    metrics::counter!("knot_audit_write_failures_total", "action" => action.to_owned())
        .increment(1);
}

pub async fn record(
    pool: &PgPool,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
) {
    let no_data = JsonValue::Object(Default::default());
    record_with_data(
        pool,
        workspace_id,
        actor,
        action,
        target_kind,
        target_id,
        &no_data,
    )
    .await
}

/// Same as `record`, with details about the event in `data` (a JSON
/// object). Best-effort: failures are logged and swallowed.
pub async fn record_with_data(
    pool: &PgPool,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
    data: &JsonValue,
) {
    let written = match pool.acquire().await {
        Ok(mut conn) => {
            record_in_tx_with_data(
                &mut conn,
                workspace_id,
                actor,
                action,
                target_kind,
                target_id,
                data,
            )
            .await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = written {
        tracing::warn!(error=?e, action, "audit write failed (best-effort)");
        count_write_failure(action);
    }
}

/// One `action` row per id in `target_ids`, all with the same `data`, in one
/// INSERT, so either every page of an export is recorded or none is; an
/// empty slice is a no-op. The export routes await it before releasing the
/// file and refuse the export on an error. It gives up after
/// [`EXPORT_AUDIT_WRITE_TIMEOUT`]. Every failure, a database error or the
/// timeout, is logged and counted once (whatever the page count) in
/// `knot_audit_write_failures_total{action}` for operators.
pub async fn record_for_targets(
    pool: &PgPool,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_ids: &[Uuid],
    data: &JsonValue,
) -> Result<(), AuditWriteError> {
    if target_ids.is_empty() {
        return Ok(());
    }
    let bounded_write = tokio::time::timeout(
        EXPORT_AUDIT_WRITE_TIMEOUT,
        insert_for_targets(
            pool,
            workspace_id,
            actor,
            action,
            target_kind,
            target_ids,
            data,
        ),
    )
    .await;
    let result = match bounded_write {
        Ok(written) => written.map_err(AuditWriteError::from),
        Err(_elapsed) => Err(AuditWriteError::TimedOut(EXPORT_AUDIT_WRITE_TIMEOUT)),
    };
    if let Err(error) = &result {
        tracing::warn!(
            error=?error,
            action,
            targets = target_ids.len(),
            "audit write failed; the export it records is refused"
        );
        count_write_failure(action);
    }
    result
}

/// The INSERT behind [`record_for_targets`], in its own transaction so its
/// `statement_timeout` applies to it alone. Abandoned by the caller's
/// timeout before the COMMIT, it never commits.
async fn insert_for_targets(
    pool: &PgPool,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_ids: &[Uuid],
    data: &JsonValue,
) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('statement_timeout', $1, true)")
        .bind(format!("{}ms", EXPORT_AUDIT_WRITE_TIMEOUT.as_millis()))
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id, data)
         SELECT $1, $2, $3, $4, t.target_id, $6 FROM UNNEST($5::uuid[]) AS t(target_id)",
    )
    .bind(workspace_id)
    .bind(actor)
    .bind(action)
    .bind(target_kind)
    .bind(target_ids)
    .bind(data)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await
}

/// Same as `record` but accepts an in-flight transaction so the audit row
/// is committed alongside its mutation.
pub async fn record_in_tx(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
) -> Result<(), sqlx::Error> {
    let no_data = JsonValue::Object(Default::default());
    record_in_tx_with_data(
        tx,
        workspace_id,
        actor,
        action,
        target_kind,
        target_id,
        &no_data,
    )
    .await
}

/// Same as `record_in_tx`, with details about the event in `data` (a JSON
/// object).
pub async fn record_in_tx_with_data(
    tx: &mut PgConnection,
    workspace_id: Uuid,
    actor: Option<Uuid>,
    action: &str,
    target_kind: &str,
    target_id: Uuid,
    data: &JsonValue,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id, data)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(workspace_id)
    .bind(actor)
    .bind(action)
    .bind(target_kind)
    .bind(target_id)
    .bind(data)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// One audit event on a page, as the page audit log reads it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DocAuditRow {
    pub id: i64,
    pub actor_id: Option<Uuid>,
    pub action: String,
    pub data: JsonValue,
    pub created_at: DateTime<Utc>,
}

/// How often one action was recorded on a page, and when last.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ActionCount {
    pub action: String,
    pub count: i64,
    pub last_at: DateTime<Utc>,
}

/// A page's `doc.export` rows by `data.format` and `data.scope` (either
/// `None` on a row without it).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportGroup {
    pub format: Option<String>,
    pub scope: Option<String>,
    pub count: i64,
    pub last_30_days: i64,
}

/// How often one person exported a page, and when last.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExporterCount {
    pub actor_id: Uuid,
    pub count: i64,
    pub last_at: DateTime<Utc>,
}

/// The counts the page audit log shows above its events.
#[derive(Debug, Clone, Default)]
pub struct DocAuditStats {
    /// Every action on the page, most frequent first (ties by action).
    pub by_action: Vec<ActionCount>,
    pub export_groups: Vec<ExportGroup>,
    /// Every person who exported the page (rows without an actor are not
    /// here), most recent first. Unbounded: the caller caps it.
    pub exporters: Vec<ExporterCount>,
    pub last_export: Option<DocAuditRow>,
}

/// The rows of one page: the same filter in every read, so each one is a
/// range of `audit_events_target_idx`.
const DOC_ROWS: &str = "target_id = $2 AND workspace_id = $1 AND target_kind = 'doc'";

/// The statement [`list_doc_events`] runs: `$1` workspace, `$2` page, `$3`
/// actions or NULL, `$4` the cursor id or NULL, `$5` the limit. The cursor
/// bound is `id < COALESCE($4, i64::MAX)`, not `$4 IS NULL OR id < $4`: the
/// generic plan Postgres switches to after five runs keeps it as an index
/// condition, so a deep page doesn't re-read every newer row. Public so a
/// test can check that plan.
#[doc(hidden)]
pub fn list_doc_events_sql() -> String {
    format!(
        "SELECT id, actor_id, action, data, created_at FROM audit_events
          WHERE {DOC_ROWS}
            AND ($3::text[] IS NULL OR action = ANY($3))
            AND id < COALESCE($4::bigint, 9223372036854775807)
          ORDER BY id DESC LIMIT $5"
    )
}

/// One page of `doc_id`'s events, newest first. `before`: only ids below it.
/// `actions`: only these actions (`None` = all). Callers pass `limit + 1`.
pub async fn list_doc_events(
    pool: &PgPool,
    workspace_id: Uuid,
    doc_id: Uuid,
    actions: Option<&[&str]>,
    before: Option<i64>,
    limit: i64,
) -> Result<Vec<DocAuditRow>, sqlx::Error> {
    let action_filter: Option<Vec<String>> =
        actions.map(|names| names.iter().map(|name| (*name).to_owned()).collect());
    sqlx::query_as::<_, DocAuditRow>(sqlx::AssertSqlSafe(list_doc_events_sql()))
        .bind(workspace_id)
        .bind(doc_id)
        .bind(action_filter)
        .bind(before)
        .bind(limit)
        .fetch_all(pool)
        .await
}

/// The counts of `doc_id`'s events over all of them: per action, exports
/// per format and scope, per exporter, and the latest export.
pub async fn doc_stats(
    pool: &PgPool,
    workspace_id: Uuid,
    doc_id: Uuid,
) -> Result<DocAuditStats, sqlx::Error> {
    let by_action_sql = format!(
        "SELECT action, count(*) AS count, max(created_at) AS last_at FROM audit_events
          WHERE {DOC_ROWS}
          GROUP BY action
          ORDER BY count DESC, action"
    );
    let export_groups_sql = format!(
        "SELECT data->>'format' AS format, data->>'scope' AS scope, count(*) AS count,
                count(*) FILTER (WHERE created_at > now() - interval '30 days') AS last_30_days
           FROM audit_events
          WHERE {DOC_ROWS} AND action = 'doc.export'
          GROUP BY 1, 2"
    );
    let exporters_sql = format!(
        "SELECT actor_id, count(*) AS count, max(created_at) AS last_at FROM audit_events
          WHERE {DOC_ROWS} AND action = 'doc.export' AND actor_id IS NOT NULL
          GROUP BY actor_id
          ORDER BY max(created_at) DESC, actor_id"
    );
    let last_export_sql = format!(
        "SELECT id, actor_id, action, data, created_at FROM audit_events
          WHERE {DOC_ROWS} AND action = 'doc.export'
          ORDER BY id DESC LIMIT 1"
    );
    let (by_action, export_groups, exporters, last_export) = tokio::try_join!(
        sqlx::query_as::<_, ActionCount>(sqlx::AssertSqlSafe(by_action_sql))
            .bind(workspace_id)
            .bind(doc_id)
            .fetch_all(pool),
        sqlx::query_as::<_, ExportGroup>(sqlx::AssertSqlSafe(export_groups_sql))
            .bind(workspace_id)
            .bind(doc_id)
            .fetch_all(pool),
        sqlx::query_as::<_, ExporterCount>(sqlx::AssertSqlSafe(exporters_sql))
            .bind(workspace_id)
            .bind(doc_id)
            .fetch_all(pool),
        sqlx::query_as::<_, DocAuditRow>(sqlx::AssertSqlSafe(last_export_sql))
            .bind(workspace_id)
            .bind(doc_id)
            .fetch_optional(pool),
    )?;
    Ok(DocAuditStats {
        by_action,
        export_groups,
        exporters,
        last_export,
    })
}
