//! Storing a duplicate: a copy of a page, with its subpages, drawings and
//! files, written in one transaction.
//!
//! The caller prepares everything first (`DuplicatePlan`): the content of
//! every copy, and the objects of files kept outside the database, already
//! copied and recorded in `blob_copy_pending`. `apply_duplicate` then only
//! inserts rows. The copy goes right after the source, and the group's lock
//! is taken last, so the inserts don't hold up creates and moves next to the
//! source.

use std::collections::HashMap;
use std::time::Duration;

use sqlx::types::JsonValue;
use sqlx::{AssertSqlSafe, PgConnection, PgPool};
use uuid::Uuid;

use crate::blobs::{BlobMetadata, BlobStore, BlobStoreError};
use crate::doc_store::{COLS, DocRow, DocStoreError, Document, doc_from_row, map_unique};
use crate::placement::{self, Placement};
use crate::updates_store::INSERT_UPDATES_SQL;
use crate::{audit, invalidations, lexorank};

/// One page of the copy.
#[derive(Debug, Clone)]
pub struct PageCopy {
    pub new_id: Uuid,
    /// The top page: the source's parent. A subpage: the copy of its
    /// parent, earlier in the plan.
    pub parent: Option<Uuid>,
    pub title: String,
    pub icon: Option<String>,
    /// The copy is a restriction boundary, as its page is. The plan's actor
    /// gets an inheriting `owner` grant on it, so they can open what they
    /// copied.
    pub restricted: bool,
    /// The page's content as one Yjs update.
    pub state: Vec<u8>,
}

/// One drawing of the copy.
#[derive(Debug, Clone)]
pub struct BoardCopy {
    pub new_id: Uuid,
    /// The page of the copy that shows it.
    pub new_doc_id: Uuid,
    pub label: Option<String>,
    /// The board's content as one Yjs update.
    pub state: Vec<u8>,
    pub svg: Option<Vec<u8>>,
}

/// One file of the copy.
#[derive(Debug, Clone)]
pub struct BlobCopy {
    pub new_id: Uuid,
    /// The page of the copy that shows it.
    pub new_doc_id: Uuid,
    /// The file copied. The copy keeps its type, size, hash and name.
    pub source: BlobMetadata,
    /// The bytes are rows of `blob_bytes`: the transaction copies them.
    /// Otherwise the caller copied the object to `new_id` beforehand.
    pub bytes_in_db: bool,
}

/// Everything a duplicate writes.
#[derive(Debug, Clone)]
pub struct DuplicatePlan {
    pub ws: Uuid,
    /// Who duplicates: the creator and only contributor of every copy.
    pub actor: Uuid,
    pub source_id: Uuid,
    /// The copy of the source. It goes right after the source.
    pub top: PageCopy,
    /// The copies of its subpages, every parent before its children.
    pub pages: Vec<PageCopy>,
    pub boards: Vec<BoardCopy>,
    pub blobs: Vec<BlobCopy>,
}

/// What a committed duplicate created.
#[derive(Debug, Clone)]
pub struct DuplicateOutcome {
    pub top: Document,
    /// Every page of the copy, the top page first, then the plan's order,
    /// with the seq of the update that holds its content.
    pub pages: Vec<(Uuid, i64)>,
}

/// Stores `plan` in one transaction. `DuplicateConflict` when the source
/// changed since the plan was made: it, or its parent, was moved, archived or
/// purged, or the bytes of a file it copies from the database are gone.
/// Nothing is written then; the objects copied outside the database stay
/// recorded in `blob_copy_pending` for `purge_stale_copies`.
pub async fn apply_duplicate(
    pool: &PgPool,
    plan: &DuplicatePlan,
) -> Result<DuplicateOutcome, DocStoreError> {
    placement::retry_on_deadlock(|| apply_once(pool, plan)).await
}

/// One attempt of `apply_duplicate`.
async fn apply_once(
    pool: &PgPool,
    plan: &DuplicatePlan,
) -> Result<DuplicateOutcome, DocStoreError> {
    let mut tx = crate::begin(pool).await?;
    // Under a key no other page can hold, and no one else sees before the
    // commit: its real one comes last, under the group's lock.
    let provisional = format!("~{}", plan.top.new_id);
    insert_page(&mut tx, plan, &plan.top, &provisional)
        .await
        .map_err(|e| match e {
            // The source's parent was purged since the plan was made.
            sqlx::Error::Database(db) if db.constraint() == Some("documents_parent_id_fkey") => {
                DocStoreError::DuplicateConflict
            }
            e => DocStoreError::Sqlx(e),
        })?;
    for (page, key) in plan.pages.iter().zip(subpage_keys(&plan.pages)) {
        insert_page(&mut tx, plan, page, &key).await?;
    }
    // Inside a restricted copy the workspace role stops applying, so without
    // this grant the actor would lose what they copied. Other grants are not
    // copied.
    for page in std::iter::once(&plan.top).chain(&plan.pages) {
        if page.restricted {
            insert_owner_grant(&mut tx, plan.actor, page.new_id).await?;
        }
    }
    let mut pages = Vec::with_capacity(plan.pages.len() + 1);
    for page in std::iter::once(&plan.top).chain(&plan.pages) {
        let seq: i64 = sqlx::query_scalar(INSERT_UPDATES_SQL)
            .bind(page.new_id)
            .bind([Some(plan.actor)])
            .bind([page.state.as_slice()])
            .fetch_one(&mut *tx)
            .await?;
        pages.push((page.new_id, seq));
    }
    for board in &plan.boards {
        insert_board(&mut tx, plan.actor, board).await?;
    }
    for blob in &plan.blobs {
        insert_blob(&mut tx, plan, blob).await?;
    }
    let copied: Vec<Uuid> = plan.blobs.iter().map(|b| b.new_id).collect();
    sqlx::query("DELETE FROM blob_copy_pending WHERE blob_id = ANY($1)")
        .bind(&copied)
        .execute(&mut *tx)
        .await?;
    let data = JsonValue::from_iter([("source_id", JsonValue::from(plan.source_id.to_string()))]);
    audit::record_in_tx_with_data(
        &mut tx,
        plan.ws,
        Some(plan.actor),
        "doc.duplicate",
        "doc",
        plan.top.new_id,
        &data,
    )
    .await?;
    // One for the top page: the listener walks its descendants.
    invalidations::record_in_tx(&mut tx, plan.ws, plan.top.new_id, "create").await?;

    let sort_key = placement::place(
        &mut tx,
        plan.ws,
        plan.top.parent,
        Placement::After(plan.source_id),
        Some(plan.top.new_id),
    )
    .await
    .map_err(|e| match e {
        // The source is no longer a live page there, or its parent is gone
        // or archived.
        DocStoreError::BadAnchor | DocStoreError::Archived | DocStoreError::NotFound => {
            DocStoreError::DuplicateConflict
        }
        e => e,
    })?;
    let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
        "UPDATE documents SET sort_key = $2 WHERE id = $1 RETURNING {COLS}"
    )))
    .bind(plan.top.new_id)
    .bind(&sort_key)
    .fetch_one(&mut *tx)
    .await
    // A writer that skips the group's lock (an old release) took the key.
    .map_err(map_unique)?;
    tx.commit().await?;
    Ok(DuplicateOutcome {
        top: doc_from_row(row),
        pages,
    })
}

/// The keys of `pages`, in order: each parent's copies get `spread` keys in
/// the order the plan lists them.
fn subpage_keys(pages: &[PageCopy]) -> Vec<String> {
    let mut sizes: HashMap<Option<Uuid>, usize> = HashMap::new();
    for page in pages {
        *sizes.entry(page.parent).or_default() += 1;
    }
    let mut keys: HashMap<Option<Uuid>, std::vec::IntoIter<String>> = sizes
        .into_iter()
        .map(|(parent, n)| (parent, lexorank::spread(n).into_iter()))
        .collect();
    pages
        .iter()
        .map(|page| {
            keys.get_mut(&page.parent)
                .and_then(Iterator::next)
                .expect("one key per page of the group")
        })
        .collect()
}

async fn insert_page(
    conn: &mut PgConnection,
    plan: &DuplicatePlan,
    page: &PageCopy,
    sort_key: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO documents (id, workspace_id, parent_id, title, sort_key, icon, created_by,
                                restricted)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(page.new_id)
    .bind(plan.ws)
    .bind(page.parent)
    .bind(&page.title)
    .bind(sort_key)
    .bind(&page.icon)
    .bind(plan.actor)
    .bind(page.restricted)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// `actor`'s inheriting `owner` grant on the copy `doc_id`, as restricting a
/// page gives its restrictor.
async fn insert_owner_grant(
    conn: &mut PgConnection,
    actor: Uuid,
    doc_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO document_grants (doc_id, principal, role, inherit, granted_by)
         VALUES ($1, $2, 'owner', true, $3)",
    )
    .bind(doc_id)
    .bind(format!("user:{actor}"))
    .bind(actor)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// The board's row, with its SVG as if uploaded once, and its state as its
/// only update.
async fn insert_board(
    conn: &mut PgConnection,
    actor: Uuid,
    board: &BoardCopy,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO boards (id, doc_id, created_by, label, svg_cached, svg_seq)
         VALUES ($1, $2, $3, $4, $5, CASE WHEN $5::bytea IS NULL THEN 0 ELSE 1 END)",
    )
    .bind(board.new_id)
    .bind(board.new_doc_id)
    .bind(actor)
    .bind(&board.label)
    .bind(&board.svg)
    .execute(&mut *conn)
    .await?;
    sqlx::query("INSERT INTO board_updates (board_id, bytes) VALUES ($1, $2)")
        .bind(board.new_id)
        .bind(&board.state)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// The file's row, and its bytes when they are held in the database.
/// `DuplicateConflict` when those bytes are gone: the source was purged since
/// the plan was made.
async fn insert_blob(
    conn: &mut PgConnection,
    plan: &DuplicatePlan,
    blob: &BlobCopy,
) -> Result<(), DocStoreError> {
    let source = &blob.source;
    sqlx::query(
        "INSERT INTO blobs (id, workspace_id, doc_id, content_type, byte_size, sha256,
                            original_name, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(blob.new_id)
    .bind(plan.ws)
    .bind(blob.new_doc_id)
    .bind(&source.content_type)
    .bind(source.byte_size)
    .bind(&source.sha256)
    .bind(&source.original_name)
    .bind(plan.actor)
    .execute(&mut *conn)
    .await?;
    if blob.bytes_in_db {
        let copied = sqlx::query(
            "INSERT INTO blob_bytes (blob_id, bytes)
             SELECT $1, bytes FROM blob_bytes WHERE blob_id = $2",
        )
        .bind(blob.new_id)
        .bind(source.id)
        .execute(&mut *conn)
        .await?;
        if copied.rows_affected() != 1 {
            return Err(DocStoreError::DuplicateConflict);
        }
    }
    Ok(())
}

/// Records `ids`, the objects a duplicate is about to copy outside the
/// database, so that `purge_stale_copies` removes them if the duplicate never
/// commits. Committed before the first copy starts.
pub async fn record_pending_copies(pool: &PgPool, ids: &[Uuid]) -> Result<(), DocStoreError> {
    sqlx::query(
        "INSERT INTO blob_copy_pending (blob_id) SELECT unnest($1::uuid[])
         ON CONFLICT (blob_id) DO NOTHING",
    )
    .bind(ids)
    .execute(pool)
    .await?;
    Ok(())
}

/// Cleans up after duplicates that never committed: for every entry of
/// `blob_copy_pending` older than `older_than`, deletes the object when no
/// `blobs` row has its id (a missing object is fine), then the entry. An
/// entry whose row exists only goes. `older_than` must exceed the longest a
/// duplicate can take, or a running one loses its files.
///
/// A delete the store refuses is logged, and its entry stays for the next
/// run. Returns how many unreferenced copies are gone.
pub async fn purge_stale_copies(
    pool: &PgPool,
    blobs: &dyn BlobStore,
    older_than: Duration,
) -> Result<usize, DocStoreError> {
    let stale: Vec<(Uuid, bool)> = sqlx::query_as(
        "SELECT p.blob_id, EXISTS (SELECT 1 FROM blobs b WHERE b.id = p.blob_id)
           FROM blob_copy_pending p
          WHERE p.created_at <= now() - make_interval(secs => $1)
          ORDER BY p.created_at, p.blob_id",
    )
    .bind(older_than.as_secs_f64())
    .fetch_all(pool)
    .await?;
    let mut removed = 0;
    for (id, referenced) in stale {
        if !referenced {
            match blobs.delete(id).await {
                Ok(()) | Err(BlobStoreError::NotFound) => removed += 1,
                Err(e) => {
                    tracing::warn!(error = %e, blob_id = %id, "stale copy not deleted; retrying next run");
                    continue;
                }
            }
        }
        sqlx::query("DELETE FROM blob_copy_pending WHERE blob_id = $1")
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(removed)
}
