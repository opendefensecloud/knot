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

/// The first half of the advisory lock key that moves into a parent take,
/// one per workspace (see `lock_move`): `"move"` in ASCII. The second half
/// is `hashtext(workspace_id::text)`.
const MOVE_LOCK: i32 = 0x6d6f_7665;

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
///   gives `Archived`; one that is not a page of `ws` gives `NotFound`.
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
/// moved row, both `FOR NO KEY UPDATE`. The caller takes it first in its
/// transaction.
///
/// - Under a parent, first the workspace's move lock, a transaction-scoped
///   advisory lock that only moves into a parent take, then the two rows in
///   id order, in one statement. Moves into a parent therefore run one at a
///   time per workspace, and the caller's cycle check sees every one that
///   committed before it. Row locks alone left a gap: two moves on disjoint
///   rows (A into B, and C into D, where C is B's parent and A is D's) took
///   no lock in common, both checks read the tree before either committed,
///   and together they closed the cycle A → B → C → D → A.
/// - At the top level, the group's advisory lock first, then the row. A
///   move to the top level can't close a cycle, so it skips the move lock.
///
/// The move lock adds no deadlock: it is the first lock of every
/// transaction that takes it, so a move waiting for it holds nothing that
/// another transaction can wait for. Its holder then locks as before: the
/// top-level lock before any row, rows under a parent in id order.
///
/// `NotFound` when the doc or the parent is not a page of `ws`, before the
/// caller's cycle check can say anything about a page elsewhere; `Archived`
/// when either is in the archive.
pub(crate) async fn lock_move(
    conn: &mut PgConnection,
    ws: Uuid,
    doc: Uuid,
    parent: Option<Uuid>,
) -> Result<(), DocStoreError> {
    let rows: Vec<(Uuid, bool)> = match parent {
        Some(parent) => {
            sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2::uuid::text))")
                .bind(MOVE_LOCK)
                .bind(ws)
                .execute(&mut *conn)
                .await?;
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
    let locked = |id: Uuid| rows.iter().any(|(row, _)| *row == id);
    if !locked(doc) || parent.is_some_and(|parent| !locked(parent)) {
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
    let read = sqlx::query_as(group_read(parent)).bind(ws).bind(moving);
    let read = match parent {
        Some(parent) => read.bind(parent),
        None => read,
    };
    let mut group: Vec<Member> = read.fetch_all(&mut *conn).await?;
    let slot = slot(&group, placement)?;
    if let Some(key) = key_at(&group, slot) {
        return Ok(key);
    }
    respread(conn, ws, parent, &mut group, moving).await?;
    // Unreachable in practice: neighbours from `spread` are valid, ordered
    // and short.
    key_at(&group, slot).ok_or(DocStoreError::Conflict)
}

/// The group read of `place`: every row of the group in workspace `$1` but
/// `$2`, the page being placed, in the sidebar's order. Under a parent, the
/// parent is `$3`.
///
/// One statement per kind of group, so that each finds its rows through the
/// index on `(workspace_id, parent_id, sort_key)`. `parent_id IS NOT
/// DISTINCT FROM $n` can't use an index for the parent, so one statement for
/// both read the whole workspace (or table), under the group's lock.
fn group_read(parent: Option<Uuid>) -> &'static str {
    match parent {
        None => {
            "SELECT id, sort_key, archived_at IS NULL AND NOT is_template FROM documents
              WHERE workspace_id = $1 AND parent_id IS NULL AND id IS DISTINCT FROM $2
              ORDER BY sort_key COLLATE \"C\", id"
        }
        Some(_) => {
            "SELECT id, sort_key, archived_at IS NULL AND NOT is_template FROM documents
              WHERE workspace_id = $1 AND parent_id = $3 AND id IS DISTINCT FROM $2
              ORDER BY sort_key COLLATE \"C\", id"
        }
    }
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

#[cfg(test)]
mod tests {
    use sqlx::{AssertSqlSafe, PgConnection, PgPool, Row};
    use uuid::Uuid;

    /// A workspace of 100 top-level pages with 40 subpages each, analysed,
    /// and one of the top-level pages.
    async fn seeded() -> (PgPool, Uuid, Uuid) {
        let pool = knot_test_support::fresh_db().await.pool;
        let ws: Uuid = sqlx::query_scalar(
            "INSERT INTO workspaces (slug, name) VALUES ('w', 'W') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let user: Uuid = sqlx::query_scalar(
            "INSERT INTO users (email, display_name) VALUES ('a@x.test', 'A') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO documents (workspace_id, title, sort_key, created_by)
             SELECT $1, 'Page', 'p' || to_char(i, 'FM000'), $2 FROM generate_series(1, 100) i",
        )
        .bind(ws)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO documents (workspace_id, parent_id, title, sort_key, created_by)
             SELECT $1, p.id, 'Subpage', 's' || to_char(i, 'FM00'), $2
               FROM documents p, generate_series(1, 40) i",
        )
        .bind(ws)
        .bind(user)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE documents")
            .execute(&pool)
            .await
            .unwrap();
        let parent = sqlx::query_scalar("SELECT id FROM documents WHERE parent_id IS NULL LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        (pool, ws, parent)
    }

    /// The plan, in text form, of `sql` prepared with three `uuid`
    /// parameters and run with `args`, under `plan_cache_mode` `mode`: a
    /// custom plan for these values, or the generic plan that a statement
    /// sqlx keeps prepared can switch to.
    async fn plan(conn: &mut PgConnection, sql: &str, args: &str, mode: &str) -> String {
        sqlx::raw_sql(AssertSqlSafe(format!(
            "SET plan_cache_mode = {mode}; PREPARE read(uuid, uuid, uuid) AS {sql}"
        )))
        .execute(&mut *conn)
        .await
        .unwrap();
        let rows = sqlx::raw_sql(AssertSqlSafe(format!("EXPLAIN EXECUTE read({args})")))
            .fetch_all(&mut *conn)
            .await
            .unwrap();
        sqlx::raw_sql("DEALLOCATE read")
            .execute(&mut *conn)
            .await
            .unwrap();
        rows.iter()
            .map(|row| row.get::<String, _>(0))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The group reads find their rows through the index on
    /// `(workspace_id, parent_id, sort_key)`, at the top level and under a
    /// parent, in custom and generic plans. `parent_id IS NOT DISTINCT FROM
    /// $n` can't use an index for the parent: with one workspace in the
    /// table, each read scanned all of it, `place` while it held the group's
    /// lock.
    #[tokio::test(flavor = "multi_thread")]
    async fn group_reads_find_their_rows_through_the_parent_index() {
        let (pool, ws, parent) = seeded().await;
        let mut conn = pool.acquire().await.unwrap();
        let at = |parent: Option<Uuid>| parent.map_or("NULL".to_owned(), |p| format!("'{p}'"));
        // `place` binds the workspace, the page being placed (none for a
        // create) and the parent; `siblings` the workspace and the parent.
        let reads = [None, Some(parent)].into_iter().flat_map(|group| {
            [
                (
                    format!("place, parent {group:?}"),
                    super::group_read(group).to_owned(),
                    format!("'{ws}', NULL, {}", at(group)),
                ),
                (
                    format!("siblings, parent {group:?}"),
                    crate::doc_store::siblings_sql(group),
                    format!("'{ws}', {}, NULL", at(group)),
                ),
            ]
        });
        let mut wrong = Vec::new();
        for (what, sql, args) in reads {
            for mode in ["force_custom_plan", "force_generic_plan"] {
                let plan = plan(&mut conn, &sql, &args, mode).await;
                let by_parent = plan
                    .lines()
                    .any(|line| line.contains("Index Cond") && line.contains("parent_id"));
                if plan.contains("Seq Scan") || !by_parent {
                    wrong.push(format!("{what}, {mode}:\n{plan}"));
                }
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n\n"));
    }
}
