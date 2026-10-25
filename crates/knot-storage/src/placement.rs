//! Where a page goes among its siblings, decided inside the transaction that
//! writes it.
//!
//! A *group* is the rows of one parent in a workspace (`parent_id IS NULL`:
//! the top level), archived pages and templates included: they keep their
//! keys, and `documents_sort_key_unique` counts them. `place` takes the
//! group's lock, reads the group in the sidebar's order (by code unit), and
//! returns a key for the requested slot. When no valid key of at most 64
//! characters fits there (a neighbour an old release wrote, or keys grown long),
//! it first respreads the whole group in one statement, then tries again.

use std::future::Future;

use sqlx::PgConnection;
use uuid::Uuid;

use crate::doc_store::{DocStoreError, ParentLock, lock_parent};
use crate::lexorank;

/// Where a page goes in its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Before every page in the group.
    First,
    /// After every page in the group.
    Last,
    /// Right after this page, a live, non-template member of the group.
    After(Uuid),
    /// Right before this page, a live, non-template member of the group.
    Before(Uuid),
}

/// The first half of the top-level group's advisory lock key: `"knot"` in
/// ASCII. The second half is `hashtext(workspace_id::text)`.
const TOP_LEVEL_LOCK: i32 = 0x6b6e_6f74;

/// The longest key placement writes. A longer one respreads the group.
const MAX_KEY_LEN: usize = 64;

/// Locks the group of `parent` in `ws` until the transaction ends, so that
/// placements in it run one after the other. Taking it again in the same
/// transaction is a no-op.
///
/// - At the top level: a transaction-scoped advisory lock per workspace.
/// - Under a parent: the parent's row, `FOR NO KEY UPDATE`. It conflicts
///   with archive's `FOR UPDATE` and with the stamping `UPDATE`, so a page
///   can't slip under a parent that is being archived. An archived parent
///   gives `Archived`; a missing one is left to the foreign key.
pub(crate) async fn lock_group(
    conn: &mut PgConnection,
    ws: Uuid,
    parent: Option<Uuid>,
) -> Result<(), DocStoreError> {
    let Some(parent) = parent else {
        sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::uuid::text))")
            .bind(TOP_LEVEL_LOCK)
            .bind(ws)
            .execute(&mut *conn)
            .await?;
        return Ok(());
    };
    lock_parent(conn, ws, parent, ParentLock::NoKeyUpdate).await
}

/// Locks what a move of `doc` to `parent` needs: the target group and the
/// moved row, both `FOR NO KEY UPDATE`.
///
/// - Under a parent, in one statement that takes the two rows in id order.
///   Two moves that swap nesting (D into P, P into D) lock the same rows in
///   the same order, so the second waits for the first and then its cycle
///   check sees the first's result.
/// - At the top level, the group's advisory lock first, then the row.
///
/// `NotFound` when the doc is missing; `Archived` when it or the parent is
/// in the archive.
pub(crate) async fn lock_move(
    conn: &mut PgConnection,
    ws: Uuid,
    doc: Uuid,
    parent: Option<Uuid>,
) -> Result<(), DocStoreError> {
    let rows: Vec<(Uuid, bool)> = match parent {
        Some(parent) => {
            sqlx::query_as(
                "SELECT id, archived_at IS NOT NULL FROM documents
                  WHERE workspace_id = $1 AND id = ANY($2)
                  ORDER BY id FOR NO KEY UPDATE",
            )
            .bind(ws)
            .bind(vec![parent, doc])
            .fetch_all(&mut *conn)
            .await?
        }
        None => {
            lock_group(conn, ws, None).await?;
            sqlx::query_as(
                "SELECT id, archived_at IS NOT NULL FROM documents
                  WHERE workspace_id = $1 AND id = $2 FOR NO KEY UPDATE",
            )
            .bind(ws)
            .bind(doc)
            .fetch_all(&mut *conn)
            .await?
        }
    };
    if !rows.iter().any(|(id, _)| *id == doc) {
        return Err(DocStoreError::NotFound);
    }
    if rows.iter().any(|(_, archived)| *archived) {
        return Err(DocStoreError::Archived);
    }
    Ok(())
}

/// A key for a page at `placement` in the group of `parent` in `ws`.
///
/// Takes the group's lock and reads the whole group except `moving`, the page
/// being placed when it already exists. The anchor of `After` and `Before`
/// must be a live, non-template member, else `BadAnchor`. The key goes
/// between the anchor and the row right next to it in the whole group: an
/// archived row there still bounds it, so the key can't collide with one.
///
/// When no valid key of at most 64 characters fits, the group is respread
/// (`lexorank::spread`) in one statement, and the key is computed again.
pub(crate) async fn place(
    conn: &mut PgConnection,
    ws: Uuid,
    parent: Option<Uuid>,
    placement: Placement,
    moving: Option<Uuid>,
) -> Result<String, DocStoreError> {
    lock_group(conn, ws, parent).await?;
    let mut group: Vec<Member> = sqlx::query_as(
        "SELECT id, sort_key, archived_at IS NULL AND NOT is_template FROM documents
          WHERE workspace_id = $1 AND parent_id IS NOT DISTINCT FROM $2
            AND id IS DISTINCT FROM $3
          ORDER BY sort_key COLLATE \"C\", id",
    )
    .bind(ws)
    .bind(parent)
    .bind(moving)
    .fetch_all(&mut *conn)
    .await?;
    let slot = slot(&group, placement)?;
    if let Some(key) = key_at(&group, slot) {
        return Ok(key);
    }
    respread(conn, ws, parent, &mut group, moving).await?;
    // Unreachable in practice: neighbours from `spread` are valid, ordered
    // and short.
    key_at(&group, slot).ok_or(DocStoreError::Conflict)
}

/// One row of a group: id, key, and whether it can be an anchor.
type Member = (Uuid, String, bool);

/// The index in `group` the new page goes before (`group.len()`: the end).
fn slot(group: &[Member], placement: Placement) -> Result<usize, DocStoreError> {
    let anchor = |id: Uuid| {
        group
            .iter()
            .position(|(member, _, anchorable)| *member == id && *anchorable)
            .ok_or(DocStoreError::BadAnchor)
    };
    Ok(match placement {
        Placement::First => 0,
        Placement::Last => group.len(),
        Placement::After(id) => anchor(id)? + 1,
        Placement::Before(id) => anchor(id)?,
    })
}

/// A valid key of at most `MAX_KEY_LEN` characters between the rows on
/// either side of `slot`, if there is one.
fn key_at(group: &[Member], slot: usize) -> Option<String> {
    let below = slot.checked_sub(1).map(|i| group[i].1.as_str());
    let above = group.get(slot).map(|(_, key, _)| key.as_str());
    lexorank::between(below, above)
        .ok()
        .filter(|key| key.len() <= MAX_KEY_LEN)
}

/// Rewrites the keys of `group`, in its order, to `spread(n)`, in a single
/// `UPDATE`. `documents_sort_key_unique` is checked at the end of the
/// statement, so keys may pass through each other on the way.
///
/// - The parent condition makes Postgres skip a row that moved to another
///   group while this statement waited for its lock.
/// - `moving`, if it is in this group, gets the provisional key `'~' || id`,
///   so its old key can't collide with a new one. The caller's own `UPDATE`
///   then gives it its real key. If it is in another group, the parent
///   condition skips it.
async fn respread(
    conn: &mut PgConnection,
    ws: Uuid,
    parent: Option<Uuid>,
    group: &mut [Member],
    moving: Option<Uuid>,
) -> Result<(), DocStoreError> {
    let keys = lexorank::spread(group.len());
    let mut ids: Vec<Uuid> = group.iter().map(|(id, _, _)| *id).collect();
    let mut new_keys = keys.clone();
    if let Some(moving) = moving {
        ids.push(moving);
        new_keys.push(format!("~{moving}"));
    }
    sqlx::query(
        "UPDATE documents d SET sort_key = v.k
           FROM unnest($3::uuid[], $4::text[]) AS v(id, k)
          WHERE d.id = v.id AND d.workspace_id = $1
            AND d.parent_id IS NOT DISTINCT FROM $2",
    )
    .bind(ws)
    .bind(parent)
    .bind(ids)
    .bind(new_keys)
    .execute(&mut *conn)
    .await
    .map_err(crate::doc_store::map_unique)?;
    for (member, key) in group.iter_mut().zip(keys) {
        member.1 = key;
    }
    Ok(())
}

/// Runs `attempt`, a whole transaction, once more when Postgres aborted it
/// to break a deadlock (SQLSTATE 40P01). For example, archive's stamping
/// `UPDATE` can lock rows of a group that a respread inside the archived
/// subtree is rewriting.
pub(crate) async fn retry_on_deadlock<T, F, Fut>(mut attempt: F) -> Result<T, DocStoreError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, DocStoreError>>,
{
    match attempt().await {
        Err(DocStoreError::Sqlx(sqlx::Error::Database(e)))
            if e.code().as_deref() == Some("40P01") =>
        {
            tracing::warn!(error = %e, "deadlock; running the transaction again");
            attempt().await
        }
        result => result,
    }
}
