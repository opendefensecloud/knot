//! doc_updates persistence: append-only log of Y.Doc binary updates.
//!
//! Per spec §5.4, `seq` is a GLOBAL bigserial. From page history M1.2 on,
//! every row also carries `rev`, a per-page gapless number in commit order
//! handed out by the page's `doc_history_heads` row, what made it (`source`,
//! `meta`) and the content epoch its writer saw. Rows written before the
//! upgrade (legacy rows) have no rev until `ensure_legacy_numbered` numbers
//! them `1..legacy_rows` in seq order.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use thiserror::Error;
use uuid::Uuid;

/// What made a row (`doc_updates.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSource {
    /// Live typing over the WebSocket.
    Edit,
    /// A task checkbox toggled from /tasks.
    Task,
    /// An accepted suggestion.
    Suggestion,
    /// A restore to an earlier version.
    Restore,
    /// A restore that fell back to writing the whole page.
    RestoreFallback,
    /// A Markdown import.
    Import,
    /// A page created from a template.
    Template,
    /// The first row of a duplicated page.
    Copy,
}

impl UpdateSource {
    /// The SQL spelling (`doc_updates_source_check`).
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateSource::Edit => "edit",
            UpdateSource::Task => "task",
            UpdateSource::Suggestion => "suggestion",
            UpdateSource::Restore => "restore",
            UpdateSource::RestoreFallback => "restore_fallback",
            UpdateSource::Import => "import",
            UpdateSource::Template => "template",
            UpdateSource::Copy => "copy",
        }
    }
}

/// A `source` that is none of `UpdateSource`'s spellings.
#[derive(Debug, Error)]
#[error("unknown update source {0:?}")]
pub struct UnknownUpdateSource(pub String);

impl FromStr for UpdateSource {
    type Err = UnknownUpdateSource;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "edit" => UpdateSource::Edit,
            "task" => UpdateSource::Task,
            "suggestion" => UpdateSource::Suggestion,
            "restore" => UpdateSource::Restore,
            "restore_fallback" => UpdateSource::RestoreFallback,
            "import" => UpdateSource::Import,
            "template" => UpdateSource::Template,
            "copy" => UpdateSource::Copy,
            other => return Err(UnknownUpdateSource(other.to_string())),
        })
    }
}

/// One row to append.
#[derive(Debug, Clone, PartialEq)]
pub struct NewUpdate {
    pub by_user_id: Option<Uuid>,
    pub bytes: Vec<u8>,
    pub source: UpdateSource,
    /// Ids, timestamps and page titles only; never page text or names.
    pub meta: Option<serde_json::Value>,
}

/// Where an appended row landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Persisted {
    pub seq: i64,
    pub rev: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DocUpdate {
    pub seq: i64,
    pub doc_id: Uuid,
    pub update_bytes: Vec<u8>,
    pub by_user_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    /// `None` for a legacy row not numbered yet.
    pub rev: Option<i64>,
    /// `None` for a legacy row.
    pub source: Option<UpdateSource>,
    pub meta: Option<serde_json::Value>,
    /// `None` for a legacy row.
    pub epoch: Option<i32>,
}

/// A page's `doc_history_heads` row. All zero for a page that has none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryHead {
    /// The highest rev handed out.
    pub head_rev: i64,
    /// Rows written before the upgrade, numbered `1..=legacy_rows`.
    pub legacy_rows: i64,
    pub content_epoch: i32,
}

/// Which rows `UpdatesStore::rows_after` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowsAfter {
    /// Rows with `rev > r`, in rev order.
    Rev(i64),
    /// Every row of the page in commit order: legacy rows not numbered yet
    /// (by seq), then the rest by rev.
    All,
}

#[derive(Debug, Error)]
pub enum UpdatesStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    /// The page's content epoch is no longer the writer's: an erase or a
    /// restore rewrote its history. Nothing was written.
    #[error("page history epoch changed (writer {room}, stored {stored}); reload the page")]
    EpochChanged { room: i32, stored: i32 },
    /// The page does not exist (purged). Nothing was written.
    #[error("page does not exist")]
    DocGone,
    /// The page's unnumbered rows are not the `legacy_rows` counted at the
    /// upgrade: someone edited the tables by hand.
    #[error("{unnumbered} legacy rows, but {legacy_rows} were counted at upgrade")]
    LegacyCountMismatch { legacy_rows: i64, unnumbered: i64 },
}

impl UpdatesStoreError {
    /// The SQLSTATE of a database error.
    fn sqlstate(&self) -> Option<String> {
        match self {
            UpdatesStoreError::Sqlx(e) => e
                .as_database_error()
                .and_then(|d| d.code())
                .map(|c| c.into_owned()),
            _ => None,
        }
    }

    /// Whether retrying the same write can never succeed: the store refuses
    /// this page's writes (history rules, epoch, page gone), the data or
    /// statement is bad, or the error repeats deterministically. Everything
    /// else (connections, timeouts, lock waits, deadlocks) is transient.
    pub fn is_permanent(&self) -> bool {
        match self {
            UpdatesStoreError::EpochChanged { .. }
            | UpdatesStoreError::DocGone
            | UpdatesStoreError::LegacyCountMismatch { .. } => true,
            UpdatesStoreError::Sqlx(e) => match e {
                sqlx::Error::PoolClosed
                | sqlx::Error::Decode(_)
                | sqlx::Error::ColumnDecode { .. }
                | sqlx::Error::RowNotFound => true,
                sqlx::Error::Database(_) => match self.sqlstate() {
                    Some(code) => {
                        matches!(code.as_str(), "KH001" | "KH002" | "23503" | "23505")
                            || code.starts_with("22")
                            || code.starts_with("42")
                            || code.starts_with("XX")
                    }
                    None => false,
                },
                _ => false,
            },
        }
    }

    /// Whether the failure may depend on the batch's size, so the writer
    /// should retry with a smaller one: a statement timeout (57014) or a
    /// program limit (class 54). A lock wait (55P03) does not.
    pub fn shrinks_batch(&self) -> bool {
        self.sqlstate()
            .is_some_and(|code| code == "57014" || code.starts_with("54"))
    }
}

/// How long one statement of `PgUpdatesStore::append`, or one lock wait,
/// may take before Postgres ends it (55P03 or 57014, both transient). Below
/// the writer's `attempt_timeout`, so an attempt normally ends in Postgres,
/// on a connection that rolls back and goes back to the pool cleanly.
pub const APPEND_STATEMENT_TIMEOUT: Duration = Duration::from_secs(8);

#[async_trait]
pub trait UpdatesStore: Send + Sync + 'static {
    /// Appends `rows` to the page's log in one transaction, in input order,
    /// which is rev order. `epoch` is the content epoch the writer saw; when
    /// the page's differs, nothing is written (`EpochChanged`). Returns where
    /// each row landed, in input order.
    async fn append(
        &self,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError>;

    /// The page's rows after `from` (see `RowsAfter`).
    async fn rows_after(
        &self,
        doc_id: Uuid,
        from: RowsAfter,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError>;

    /// The page's heads row; all zero when it has none.
    async fn head(&self, doc_id: Uuid) -> Result<HistoryHead, UpdatesStoreError>;

    /// Numbers the page's legacy rows `1..=legacy_rows` in seq order, once.
    /// Returns how many rows this call numbered (0 when there was nothing to
    /// do, or another replica did it).
    async fn ensure_legacy_numbered(&self, doc_id: Uuid) -> Result<u64, UpdatesStoreError>;

    /// Inserts a batch of `(by_user_id, update_bytes)` rows atomically, each
    /// row stored with its OWN author, as edits at epoch 0. Returns the
    /// assigned seqs in input order. Kept for tests; production code calls
    /// `append`.
    async fn insert_batch(
        &self,
        doc_id: Uuid,
        updates: &[(Option<Uuid>, Vec<u8>)],
    ) -> Result<Vec<i64>, UpdatesStoreError> {
        let rows: Vec<NewUpdate> = updates
            .iter()
            .map(|(by_user_id, bytes)| NewUpdate {
                by_user_id: *by_user_id,
                bytes: bytes.clone(),
                source: UpdateSource::Edit,
                meta: None,
            })
            .collect();
        let persisted = self.append(doc_id, 0, &rows).await?;
        Ok(persisted.into_iter().map(|p| p.seq).collect())
    }

    /// Fetch updates with `seq > after_seq` for a doc, in seq order. Reads
    /// only the columns that predate page history (`rev`, `source`, `meta`
    /// and `epoch` come back `None`), so it also works on a database that was
    /// never migrated past 0.9.9 (`knot-server export-check` on a staging
    /// copy). `rows_after` reads them.
    async fn since(
        &self,
        doc_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError>;

    /// Highest seq for a doc, or 0 if none.
    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError>;
}

/// The first statement of every flush: the page row's KEY SHARE. No row: the
/// page is gone (`DocGone`).
pub(crate) const DOC_KEY_SHARE_SQL: &str = "SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE";

/// The second statement of every flush: hands out `$2` revs from the page's
/// heads row and returns the rev before them and the content epoch. Binds:
/// `$1` the page, `$2` the row count. A page's first flush creates the row
/// with `head_rev = $2`, so base = 0 and its first row gets rev 1. Takes FOR
/// NO KEY UPDATE on the heads row until the transaction ends.
pub(crate) const HEADS_UPSERT_SQL: &str =
    "INSERT INTO doc_history_heads AS h (doc_id, head_rev, dirty, last_write_at)
     VALUES ($1, $2, true, clock_timestamp())
     ON CONFLICT (doc_id) DO UPDATE
     SET head_rev = h.head_rev + $2, dirty = true, last_write_at = clock_timestamp()
     RETURNING h.head_rev - $2 AS base, h.content_epoch";

/// Appends one page's rows, each with its author, rev, source, meta and
/// epoch, and credits the authors in `doc_contributors` in the same
/// statement. Binds: `$1` the page, `$2` the authors (`uuid[]`), `$3` the
/// updates (`bytea[]`), `$4` the rev before the first row, `$5` the sources
/// (`text[]`), `$6` the metas (`text[]` of JSON, cast to jsonb), `$7` the
/// epoch. Returns `(seq, rev)` by rev, which is input order.
///
/// Runs third in `append_in_tx`, after the page row's KEY SHARE and the
/// heads row's lock, both held until the transaction ends. So:
///   - rev order is commit order: a later flush gets its base only after
///     this one commits or rolls back, and a rollback hands the revs back;
///   - a purge (FOR UPDATE on the page row, then the cascade) makes the
///     flush wait, or waits for it, instead of deadlocking;
///   - the lock order is always documents → heads → `doc_contributors`;
///   - `created_at = clock_timestamp()` is taken after the lock, so it is
///     non-decreasing in rev order.
///
/// One statement whatever the batch size: the rows travel as parallel
/// arrays, and UNNEST … ORDER BY ord feeds them in input order, so ascending
/// seq is rev order too.
///
/// The `contrib` CTE credits every distinct author in the SAME statement, so
/// a content change and its byline entry commit or fail together. A
/// data-modifying CTE always runs to completion even though the outer
/// SELECT never reads it. Details:
///   - DISTINCT: ON CONFLICT DO UPDATE may not touch a row twice in one
///     statement, and a batch often holds many rows per author.
///   - ORDER BY by_user_id: replicas flush the same doc concurrently; taking
///     the row locks in one global order rules out deadlocks.
///   - the refresh is throttled to once a minute — continuous typing flushes
///     every 250 ms, and a minute-old "last edited" is as good as a fresh one
///     for a byline. The NOT EXISTS skips authors credited within the minute
///     before the upsert, because ON CONFLICT … DO UPDATE locks the existing
///     row even when its WHERE then skips the update; without it every flush
///     would queue on the rows of concurrent flushes (other replicas) of the
///     same doc while holding the heads row. The WHERE stays for the race
///     where a row turns fresh after the NOT EXISTS ran.
pub(crate) const INSERT_UPDATES_SQL: &str = "WITH ins AS (
             INSERT INTO doc_updates
                 (doc_id, by_user_id, update_bytes, rev, source, meta, epoch, created_at)
             SELECT $1, u.by_user_id, u.update_bytes, $4 + u.ord, u.source, u.meta::jsonb,
                    $7, clock_timestamp()
             FROM UNNEST($2::uuid[], $3::bytea[], $5::text[], $6::text[])
                  WITH ORDINALITY AS u(by_user_id, update_bytes, source, meta, ord)
             ORDER BY u.ord
             RETURNING seq, rev, by_user_id
         ),
         contrib AS (
             INSERT INTO doc_contributors (doc_id, user_id)
             SELECT DISTINCT $1::uuid, ins.by_user_id
             FROM ins
             WHERE ins.by_user_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM doc_contributors c
                   WHERE c.doc_id = $1
                     AND c.user_id = ins.by_user_id
                     AND c.last_edited_at >= now() - interval '1 minute'
               )
             ORDER BY ins.by_user_id
             ON CONFLICT (doc_id, user_id) DO UPDATE
                 SET last_edited_at = now()
                 WHERE doc_contributors.last_edited_at < now() - interval '1 minute'
         )
         SELECT seq, rev FROM ins ORDER BY rev";

/// Cancels the statement of an abandoned append, if its transaction is still
/// the one running on that backend: `$1` the backend pid, `$2` the
/// transaction's start (`now()` inside it, which is `xact_start` exactly). A
/// connection that went back to the pool and runs someone else's
/// transaction has another `xact_start`, so it is never cancelled.
///
/// `pub` only so `knot_storage::testing` can re-export it: tests check the
/// exact text the drop guard runs.
#[doc(hidden)]
pub const CANCEL_ABANDONED_SQL: &str =
    "SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid = $1 AND xact_start = $2";

/// Appends `rows` to `doc_id`'s log inside the caller's transaction (see
/// `INSERT_UPDATES_SQL` for the lock order this gives). Takes the page row's
/// KEY SHARE (no row: `DocGone`), then the heads row, then checks `epoch`
/// against the page's (`EpochChanged`; the caller rolls back), then inserts.
/// Returns `(seq, rev)` per row, in input order. Sets no timeouts: Duplicate
/// runs it inside its own, longer transaction.
pub async fn append_in_tx(
    conn: &mut PgConnection,
    doc_id: Uuid,
    epoch: i32,
    rows: &[NewUpdate],
) -> Result<Vec<Persisted>, UpdatesStoreError> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let page: Option<i32> = sqlx::query_scalar(DOC_KEY_SHARE_SQL)
        .bind(doc_id)
        .fetch_optional(&mut *conn)
        .await?;
    if page.is_none() {
        return Err(UpdatesStoreError::DocGone);
    }
    let (base, stored): (i64, i32) = sqlx::query_as(HEADS_UPSERT_SQL)
        .bind(doc_id)
        .bind(rows.len() as i64)
        .fetch_one(&mut *conn)
        .await?;
    if stored != epoch {
        return Err(UpdatesStoreError::EpochChanged {
            room: epoch,
            stored,
        });
    }
    let mut authors = Vec::with_capacity(rows.len());
    let mut bytes = Vec::with_capacity(rows.len());
    let mut sources = Vec::with_capacity(rows.len());
    let mut metas = Vec::with_capacity(rows.len());
    for row in rows {
        authors.push(row.by_user_id);
        bytes.push(row.bytes.as_slice());
        sources.push(row.source.as_str());
        metas.push(row.meta.as_ref().map(|m| m.to_string()));
    }
    let persisted: Vec<(i64, i64)> = sqlx::query_as(INSERT_UPDATES_SQL)
        .bind(doc_id)
        .bind(&authors)
        .bind(&bytes)
        .bind(base)
        .bind(&sources)
        .bind(&metas)
        .bind(epoch)
        .fetch_all(&mut *conn)
        .await?;
    Ok(persisted
        .into_iter()
        .map(|(seq, rev)| Persisted { seq, rev })
        .collect())
}

/// The page's numbered rows with `after_rev < rev <= through_rev` (no upper
/// bound when `None`), in rev order: at most `max_rows` of them, and only as
/// many as keep the summed `octet_length(update_bytes)` at or below
/// `max_bytes`, stopping before the row that would cross it. The first row
/// comes back even when it alone is over `max_bytes`, so a caller that
/// admitted it (the history builder's size check) always makes progress.
/// Legacy rows not numbered yet (rev NULL) are never returned.
///
/// The limits are applied in SQL to the rows' sizes before any bytes are
/// read: the first `max_rows` revs come in order from the `(doc_id, rev)`
/// index, so rows past them are never scanned, and only the rows kept are
/// fetched, one index probe each. Runs on the caller's connection, so it
/// reads inside the caller's (repeatable-read) transaction.
pub async fn rows_in_range(
    conn: &mut PgConnection,
    doc_id: Uuid,
    after_rev: i64,
    through_rev: Option<i64>,
    max_rows: i64,
    max_bytes: i64,
) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
    // `sized` names its columns apart from doc_updates', so UPDATE_COLS
    // resolve to the joined row.
    let rows: Vec<UpdateRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {UPDATE_COLS}
           FROM (SELECT page.rev AS kept_rev,
                        row_number() OVER w AS kept_n,
                        sum(page.len) OVER w AS kept_bytes
                   FROM (SELECT rev, octet_length(update_bytes) AS len
                           FROM doc_updates
                          WHERE doc_id = $1 AND rev > $2 AND rev <= $3
                          ORDER BY rev
                          LIMIT $4) page
                 WINDOW w AS (ORDER BY page.rev ROWS UNBOUNDED PRECEDING)) sized
           JOIN doc_updates ON doc_id = $1 AND rev = sized.kept_rev
          WHERE sized.kept_n = 1 OR sized.kept_bytes <= $5
          ORDER BY rev"
    )))
    .bind(doc_id)
    .bind(after_rev)
    .bind(through_rev.unwrap_or(i64::MAX))
    .bind(max_rows.max(0))
    .bind(max_bytes)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter().map(doc_update).collect()
}

/// While armed, cancels the append's statement in Postgres when the append
/// future is dropped (the writer's `attempt_timeout` abandoned it).
/// Dropping the future sends no CancelRequest: the statement keeps running,
/// holding its connection, the page row's KEY SHARE and the heads row, and
/// every retry would queue behind it. Disarmed when the append returns.
struct CancelOnDrop {
    pool: PgPool,
    /// The backend pid and the transaction's start, once known.
    backend: Option<(i32, DateTime<Utc>)>,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some((pid, xact_start)) = self.backend else {
            return;
        };
        // No runtime (shutdown): skip; `statement_timeout` still bounds the
        // attempt. Never panic in `Drop`.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let pool = self.pool.clone();
        runtime.spawn(async move {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                sqlx::query(CANCEL_ABANDONED_SQL)
                    .bind(pid)
                    .bind(xact_start)
                    .execute(&pool),
            )
            .await;
        });
    }
}

type UpdateRow = (
    i64,
    Uuid,
    Vec<u8>,
    Option<Uuid>,
    DateTime<Utc>,
    Option<i64>,
    Option<String>,
    Option<serde_json::Value>,
    Option<i32>,
);

const UPDATE_COLS: &str =
    "seq, doc_id, update_bytes, by_user_id, created_at, rev, source, meta, epoch";

fn doc_update(r: UpdateRow) -> Result<DocUpdate, UpdatesStoreError> {
    let source =
        r.6.map(|s| s.parse::<UpdateSource>())
            .transpose()
            .map_err(|e| sqlx::Error::Decode(Box::new(e)))?;
    Ok(DocUpdate {
        seq: r.0,
        doc_id: r.1,
        update_bytes: r.2,
        by_user_id: r.3,
        created_at: r.4,
        rev: r.5,
        source,
        meta: r.7,
        epoch: r.8,
    })
}

#[derive(Clone)]
pub struct PgUpdatesStore {
    pool: PgPool,
    statement_timeout: Duration,
}

impl PgUpdatesStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            statement_timeout: APPEND_STATEMENT_TIMEOUT,
        }
    }

    /// Overrides `APPEND_STATEMENT_TIMEOUT` (tests).
    pub fn with_statement_timeout(self, timeout: Duration) -> Self {
        Self {
            statement_timeout: timeout,
            ..self
        }
    }

    /// One append: BEGIN, the timeouts, `append_in_tx`, COMMIT. Tells
    /// `guard` the backend once it is known.
    async fn append_once(
        &self,
        guard: &mut CancelOnDrop,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        // Cancellation-safe (`tests/cancel_safety.rs`).
        let mut tx = crate::begin(&self.pool).await?;
        let timeout = format!("{}ms", self.statement_timeout.as_millis());
        let (pid, xact_start, _, _): (i32, DateTime<Utc>, String, String) = sqlx::query_as(
            "SELECT pg_backend_pid(), now(),
                    set_config('lock_timeout', $1, true),
                    set_config('statement_timeout', $1, true)",
        )
        .bind(&timeout)
        .fetch_one(&mut *tx)
        .await?;
        guard.backend = Some((pid, xact_start));
        let persisted = append_in_tx(&mut tx, doc_id, epoch, rows).await?;
        tx.commit().await?;
        Ok(persisted)
    }
}

#[async_trait]
impl UpdatesStore for PgUpdatesStore {
    async fn append(
        &self,
        doc_id: Uuid,
        epoch: i32,
        rows: &[NewUpdate],
    ) -> Result<Vec<Persisted>, UpdatesStoreError> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let mut guard = CancelOnDrop {
            pool: self.pool.clone(),
            backend: None,
            armed: true,
        };
        let result = self.append_once(&mut guard, doc_id, epoch, rows).await;
        // Returned, with or without an error: no statement of ours is left
        // running. Only a dropped future leaves the guard armed.
        guard.armed = false;
        result
    }

    async fn rows_after(
        &self,
        doc_id: Uuid,
        from: RowsAfter,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        let rows: Vec<UpdateRow> = match from {
            RowsAfter::Rev(after) => {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {UPDATE_COLS} FROM doc_updates
                      WHERE doc_id = $1 AND rev > $2 ORDER BY rev"
                )))
                .bind(doc_id)
                .bind(after)
                .fetch_all(&self.pool)
                .await?
            }
            // Legacy rows predate every numbered one, so this is commit
            // order even before they are numbered.
            RowsAfter::All => {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {UPDATE_COLS} FROM doc_updates
                      WHERE doc_id = $1 ORDER BY rev NULLS FIRST, seq"
                )))
                .bind(doc_id)
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(doc_update).collect()
    }

    async fn head(&self, doc_id: Uuid) -> Result<HistoryHead, UpdatesStoreError> {
        let row: Option<(i64, i64, i32)> = sqlx::query_as(
            "SELECT head_rev, legacy_rows, content_epoch FROM doc_history_heads
              WHERE doc_id = $1",
        )
        .bind(doc_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .map(|(head_rev, legacy_rows, content_epoch)| HistoryHead {
                head_rev,
                legacy_rows,
                content_epoch,
            })
            .unwrap_or_default())
    }

    async fn ensure_legacy_numbered(&self, doc_id: Uuid) -> Result<u64, UpdatesStoreError> {
        // Room boot runs inside `acquire`, which an HTTP handler can drop; a
        // cancelled `pool.begin()` could strand a connection holding row
        // locks on the page's whole legacy log.
        let mut tx = crate::begin(&self.pool).await?;
        let numbered = number_legacy_rows(&mut tx, doc_id).await?;
        tx.commit().await?;
        Ok(numbered)
    }

    async fn since(
        &self,
        doc_id: Uuid,
        after_seq: i64,
    ) -> Result<Vec<DocUpdate>, UpdatesStoreError> {
        let rows = sqlx::query_as::<_, (i64, Uuid, Vec<u8>, Option<Uuid>, DateTime<Utc>)>(
            "SELECT seq, doc_id, update_bytes, by_user_id, created_at
             FROM doc_updates
             WHERE doc_id = $1 AND seq > $2
             ORDER BY seq",
        )
        .bind(doc_id)
        .bind(after_seq)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| DocUpdate {
                seq: r.0,
                doc_id: r.1,
                update_bytes: r.2,
                by_user_id: r.3,
                created_at: r.4,
                rev: None,
                source: None,
                meta: None,
                epoch: None,
            })
            .collect())
    }

    async fn max_seq(&self, doc_id: Uuid) -> Result<i64, UpdatesStoreError> {
        let v: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM doc_updates WHERE doc_id = $1")
                .bind(doc_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(v.unwrap_or(0))
    }
}

/// Whether the page's legacy rows are numbered: numbering gives them
/// `1..=legacy_rows` in one statement and every later row a rev above, so
/// rev `legacy_rows` exists exactly when it is done. One index probe.
async fn legacy_numbering_done(
    conn: &mut PgConnection,
    doc_id: Uuid,
    legacy_rows: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM doc_updates WHERE doc_id = $1 AND rev = $2)")
        .bind(doc_id)
        .bind(legacy_rows)
        .fetch_one(&mut *conn)
        .await
}

/// `ensure_legacy_numbered`'s transaction. Returns at once, without a lock,
/// when the page has no legacy rows or they are numbered, so only the first
/// boot of an unnumbered page scans its rows. Otherwise takes the locks in
/// the global order (page row, then heads row) and checks again under them:
/// the heads row's lock serializes two replicas numbering the same page,
/// and appends and trigger-path inserts wait for the one UPDATE.
async fn number_legacy_rows(
    conn: &mut PgConnection,
    doc_id: Uuid,
) -> Result<u64, UpdatesStoreError> {
    let legacy_rows: Option<i64> =
        sqlx::query_scalar("SELECT legacy_rows FROM doc_history_heads WHERE doc_id = $1")
            .bind(doc_id)
            .fetch_optional(&mut *conn)
            .await?;
    let legacy_rows = legacy_rows.unwrap_or(0);
    if legacy_rows == 0 || legacy_numbering_done(conn, doc_id, legacy_rows).await? {
        return Ok(0);
    }

    // A purge waits on the page row instead of meeting the numbering on the
    // page's `doc_updates` rows.
    let page: Option<i32> = sqlx::query_scalar(DOC_KEY_SHARE_SQL)
        .bind(doc_id)
        .fetch_optional(&mut *conn)
        .await?;
    if page.is_none() {
        return Err(UpdatesStoreError::DocGone);
    }
    let legacy_rows: Option<i64> = sqlx::query_scalar(
        "SELECT legacy_rows FROM doc_history_heads WHERE doc_id = $1 FOR NO KEY UPDATE",
    )
    .bind(doc_id)
    .fetch_optional(&mut *conn)
    .await?;
    let legacy_rows = legacy_rows.unwrap_or(0);
    if legacy_rows == 0 || legacy_numbering_done(conn, doc_id, legacy_rows).await? {
        // Another replica numbered the page first.
        return Ok(0);
    }
    let unnumbered: i64 = sqlx::query_scalar(
        "SELECT count(*) FILTER (WHERE rev IS NULL) FROM doc_updates WHERE doc_id = $1",
    )
    .bind(doc_id)
    .fetch_one(&mut *conn)
    .await?;
    // No row at `rev = legacy_rows` was found above, so `unnumbered == 0`
    // (rows numbered by hand to other revs) is a mismatch too, not "done".
    if unnumbered != legacy_rows {
        tracing::error!(
            %doc_id,
            legacy_rows,
            unnumbered,
            "legacy rows do not match the count taken at upgrade"
        );
        metrics::counter!("knot_history_legacy_mismatch_total").increment(1);
        return Err(UpdatesStoreError::LegacyCountMismatch {
            legacy_rows,
            unnumbered,
        });
    }
    // Under the heads row's lock no other numbering touches these rows, so
    // this UPDATE's row-lock order cannot deadlock with another numbering.
    let numbered = sqlx::query(
        "UPDATE doc_updates d SET rev = n.rn
           FROM (SELECT seq, row_number() OVER (ORDER BY seq) AS rn
                   FROM doc_updates WHERE doc_id = $1 AND rev IS NULL) n
          WHERE d.seq = n.seq AND d.rev IS NULL",
    )
    .bind(doc_id)
    .execute(&mut *conn)
    .await?;
    Ok(numbered.rows_affected())
}
