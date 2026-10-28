//! Document archive (#37): helpers shared by the archive routes and the
//! retention job. See
//! docs/superpowers/specs/2026-10-13-document-archive-retention-design.md.

use axum::{http::StatusCode, response::Response};
use knot_storage::{BlobStoreError, DocStoreError};
use uuid::Uuid;

use crate::AppState;
use crate::http_error::{doc_archived, json_err};

/// How long a doc stays restorable, from `KNOT_ARCHIVE_RETENTION_DAYS`.
pub fn retention(state: &AppState) -> chrono::Duration {
    chrono::Duration::days(i64::from(state.config.archive_retention_days))
}

/// What started a purge; the `trigger` label on `knot_docs_purged_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeTrigger {
    /// The hourly job: the retention period ran out.
    Retention,
    /// An owner chose "Delete permanently".
    Manual,
}

impl PurgeTrigger {
    fn label(self) -> &'static str {
        match self {
            PurgeTrigger::Retention => "retention",
            PurgeTrigger::Manual => "manual",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PurgeError {
    #[error("not found")]
    NotFound,
    #[error("not archived")]
    NotArchived,
    #[error("not an archive root")]
    NotArchiveRoot,
    /// Deleting attachment bytes failed; nothing was deleted from the
    /// database, so the next run retries the whole purge.
    #[error("blob delete: {0}")]
    Blob(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<DocStoreError> for PurgeError {
    fn from(e: DocStoreError) -> Self {
        match e {
            DocStoreError::NotFound => PurgeError::NotFound,
            DocStoreError::NotArchived => PurgeError::NotArchived,
            DocStoreError::NotArchiveRoot => PurgeError::NotArchiveRoot,
            other => PurgeError::Internal(other.to_string()),
        }
    }
}

/// Permanently delete an archive root. With a blob backend outside the
/// database (S3), the attachment bytes of the whole subtree go first (they
/// would otherwise outlive their rows); `begin_purge` fences that window, so
/// from then on the doc can't be restored with half its attachments gone,
/// and if a delete fails the retention job retries the purge on its next
/// run. Then the root row goes, and its foreign-key cascades take every
/// descendant and everything hanging off them (Postgres-held bytes too).
pub async fn purge_doc(
    state: &AppState,
    workspace_id: Uuid,
    doc_id: Uuid,
    actor: Option<Uuid>,
    trigger: PurgeTrigger,
) -> Result<(), PurgeError> {
    let missing = |what: &str| PurgeError::Internal(format!("{what} not configured"));
    let docs = state.docs.clone().ok_or_else(|| missing("docs"))?;
    let blob_meta = state
        .blob_meta
        .clone()
        .ok_or_else(|| missing("blob_meta"))?;
    let blob_store = state
        .blob_store
        .clone()
        .ok_or_else(|| missing("blob_store"))?;

    // Same checks as `DocStore::purge`, made before any bytes are deleted
    // so a live doc can never lose its attachments.
    match docs.get(doc_id).await? {
        Some(doc) if doc.workspace_id == workspace_id => match doc.archive_root_id {
            None => return Err(PurgeError::NotArchived),
            Some(root) if root != doc_id => return Err(PurgeError::NotArchiveRoot),
            Some(_) => {}
        },
        _ => return Err(PurgeError::NotFound),
    }
    if !blob_store.bytes_deleted_with_metadata() {
        docs.begin_purge(workspace_id, doc_id).await?;
        let subtree = docs.subtree_ids(doc_id).await?;
        let blob_ids = blob_meta
            .ids_for_docs(&subtree)
            .await
            .map_err(|e| PurgeError::Internal(e.to_string()))?;
        for blob_id in blob_ids {
            match blob_store.delete(blob_id).await {
                Ok(()) | Err(BlobStoreError::NotFound) => {}
                Err(e) => return Err(PurgeError::Blob(e.to_string())),
            }
        }
    }
    let deleted = docs.purge(workspace_id, doc_id, actor).await?;
    evict_local(state, &deleted).await;
    if let Some(rooms) = state.rooms_v2.as_ref() {
        for id in &deleted {
            // Disconnect read-only viewers (4403: the page is gone) and drop
            // the in-memory copy.
            rooms.revoke_and_evict(*id).await;
        }
    }
    metrics::counter!("knot_docs_purged_total", "trigger" => trigger.label()).increment(1);
    tracing::info!(%doc_id, docs = deleted.len(), trigger = trigger.label(), "archive purge");
    Ok(())
}

/// For write routes outside `require_doc_role_mw` (shares, task checkboxes,
/// attachments, boards): `Some(409 doc.archived)` when the doc is in the
/// archive. Role checks stay with the caller; this only adds the archive
/// rule, so call it after them.
pub async fn refuse_if_archived(
    state: &AppState,
    workspace_id: Uuid,
    doc_id: Uuid,
    user_id: Uuid,
) -> Option<Response> {
    let Some(acl) = state.acl.as_ref() else {
        return Some(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""));
    };
    match acl.access(workspace_id, doc_id, user_id).await {
        Ok(Some(access)) if access.archived => Some(doc_archived()),
        Ok(_) => None,
        Err(e) => {
            tracing::error!(error=?e, %doc_id, "acl resolve");
            Some(json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal", ""))
        }
    }
}

/// Drop the local ACL cache entries for `doc_ids` right away. Archive,
/// restore and purge also write ACL invalidations that evict them on every
/// replica, but that NOTIFY round-trip is asynchronous: without this, a
/// rename sent right after an archive could still see the doc as live here.
pub async fn evict_local(state: &AppState, doc_ids: &[Uuid]) {
    let Some(acl) = state.acl.as_ref() else {
        return;
    };
    for doc_id in doc_ids {
        acl.evict_doc(*doc_id);
    }
    acl.run_pending_tasks().await;
}

/// Advisory-lock key that keeps the purge job to one replica at a time
/// ("knotpurg" in ASCII).
pub const PURGE_LOCK_KEY: i64 = 0x6b6e_6f74_7075_7267;
/// Archive roots fetched per query.
const PURGE_BATCH: i64 = 100;
/// The job's period. The first run is one period after start-up, so a
/// deploy with a wrong retention setting can be rolled back first.
pub const PURGE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// How old the object of a duplicate that never committed must be before
/// the purge job deletes it (P16). It must exceed the longest a duplicate
/// runs, which its caps of 200 pages and 200 MiB of files keep far below an
/// hour.
pub const STALE_COPY_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Outcome of one [`run_purge_once`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PurgeRun {
    /// Another replica holds the purge lock.
    Skipped,
    /// Roots purged, and roots whose purge failed (retried next run).
    Done { purged: usize, failed: usize },
}

/// Purge every archive root archived longer than `retention` ago, and
/// delete the objects of duplicates that never committed, unless another
/// replica is already doing so.
pub async fn run_purge_once(
    state: &AppState,
    retention: chrono::Duration,
) -> Result<PurgeRun, PurgeError> {
    let internal = |e: sqlx::Error| PurgeError::Internal(e.to_string());
    let pool = state
        .pool
        .clone()
        .ok_or_else(|| PurgeError::Internal("pool not configured".into()))?;
    // A session-level lock on a connection taken out of the pool: dropping
    // it closes the session, which releases the lock even if this run
    // fails part-way. A pooled connection would go back to the pool still
    // holding the lock and stop every later run, on every replica.
    let mut lock_conn = pool.acquire().await.map_err(internal)?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(PURGE_LOCK_KEY)
        .fetch_one(&mut lock_conn)
        .await
        .map_err(internal)?;
    if !locked {
        return Ok(PurgeRun::Skipped);
    }
    let run = purge_due(state, chrono::Utc::now() - retention).await;
    remove_stale_copies(state, &pool).await;
    drop(lock_conn);
    run
}

/// Deletes the objects that duplicates copied outside the database and
/// never committed, once [`STALE_COPY_AGE`] old (P16). A failure is logged,
/// and what it left is tried again on the next run.
async fn remove_stale_copies(state: &AppState, pool: &knot_storage::Pool) {
    let Some(blobs) = state.blob_store.as_deref() else {
        return;
    };
    match knot_storage::duplicate::purge_stale_copies(pool, blobs, STALE_COPY_AGE).await {
        Ok(0) => {}
        Ok(removed) => tracing::info!(removed, "duplicate copies never committed deleted"),
        Err(e) => tracing::warn!(error = %e, "duplicate copies cleanup failed; retrying next run"),
    }
}

async fn purge_due(
    state: &AppState,
    cutoff: chrono::DateTime<chrono::Utc>,
) -> Result<PurgeRun, PurgeError> {
    let docs = state
        .docs
        .clone()
        .ok_or_else(|| PurgeError::Internal("docs not configured".into()))?;
    let mut purged = 0;
    let mut failed = 0;
    // Roots this run won't look at again: failed ones stay due and would
    // otherwise come back in every batch.
    let mut skip: std::collections::HashSet<Uuid> = std::collections::HashSet::new();
    loop {
        let limit = PURGE_BATCH + i64::try_from(skip.len()).unwrap_or(i64::MAX - PURGE_BATCH);
        let due: Vec<(Uuid, Uuid)> = docs
            .due_for_purge(cutoff, limit)
            .await?
            .into_iter()
            .filter(|(_, doc_id)| !skip.contains(doc_id))
            .collect();
        if due.is_empty() {
            return Ok(PurgeRun::Done { purged, failed });
        }
        for (workspace_id, doc_id) in due {
            match purge_doc(state, workspace_id, doc_id, None, PurgeTrigger::Retention).await {
                Ok(()) => purged += 1,
                // Restored or purged by someone else since the scan.
                Err(
                    PurgeError::NotFound | PurgeError::NotArchived | PurgeError::NotArchiveRoot,
                ) => {
                    skip.insert(doc_id);
                }
                Err(e) => {
                    failed += 1;
                    skip.insert(doc_id);
                    metrics::counter!("knot_docs_purge_failures_total").increment(1);
                    tracing::warn!(error = %e, %doc_id, "archive purge failed; retrying next run");
                }
            }
        }
    }
}

/// Run [`run_purge_once`] every [`PURGE_INTERVAL`] for the life of the
/// process.
pub fn spawn_purge_job(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PURGE_INTERVAL).await;
            match run_purge_once(&state, retention(&state)).await {
                Ok(PurgeRun::Skipped) => {
                    tracing::debug!("archive purge: another replica holds the lock");
                }
                Ok(PurgeRun::Done { purged, failed }) => {
                    if purged > 0 || failed > 0 {
                        tracing::info!(purged, failed, "archive purge run");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "archive purge run failed"),
            }
        }
    });
}
