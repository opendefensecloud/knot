//! Task index for Plan 31's workspace todo view. Tasks are derived from the
//! markdown-cache contents of each doc — every time the cache refreshes, the
//! indexer re-extracts task items and calls `upsert_for_doc`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DocTask {
    pub id: String,
    pub workspace_id: Uuid,
    pub doc_id: Uuid,
    pub item_index: i32,
    pub text: String,
    pub assignee_user_id: Option<Uuid>,
    pub checked: bool,
    pub completed_at: Option<DateTime<Utc>>,
    /// Optional "due by" timestamp extracted from a `knot://time/<iso>`
    /// link inside the task content with an explicit "by"/"due" cue.
    /// `None` when no such cue is present.
    pub due_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A task as "My tasks" lists it: the row and the title of its doc.
#[derive(Debug, Clone)]
pub struct AssignedTask {
    pub task: DocTask,
    pub doc_title: String,
}

/// Input shape from the markdown extractor — id is derived from doc_id +
/// item_index by the store.
#[derive(Debug, Clone, Default)]
pub struct DocTaskInput {
    pub item_index: i32,
    pub text: String,
    pub assignee_user_id: Option<Uuid>,
    pub checked: bool,
    pub due_at: Option<DateTime<Utc>>,
}

/// What a write of a doc's tasks is, for its `task_assigned` notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskWrite {
    /// An edit of the page, by this user when known: a task it newly
    /// assigns notifies the assignee, unless they assigned it themselves.
    Edit(Option<Uuid>),
    /// The first write of a copied page (duplicate, template, workspace
    /// import): its tasks arrive assigned, and nobody is notified. Later
    /// edits compare against what the copy recorded.
    Copy,
}

#[derive(Debug, Error)]
pub enum TaskStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, TaskStoreError>;

#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Replace the task set for `doc_id` with `items`. Rows that fell out
    /// of the new set are deleted. `completed_at` is preserved across
    /// re-indexing when the checked status doesn't change.
    ///
    /// On a [`TaskWrite::Edit`], an open task assigned to someone who
    /// wasn't assigned it before notifies them (`task_assigned`);
    /// `assignment_changes` says what counts as new.
    ///
    /// The edit's user, when known, becomes the actor on the notification,
    /// and suppresses it when someone assigns a task to themselves. It is
    /// `None` on the live-editing path — the CRDT room persists updates
    /// without an editor identity, and the reindex worker that drains its
    /// dirty-doc channel receives only a doc id — so self-assignment is
    /// **not** suppressed there: a task assigned to yourself while
    /// co-editing still produces a `task_assigned` row with a NULL actor.
    /// The guard only has an effect where the request knows its user, as
    /// on the doc page's Markdown import.
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
        write: TaskWrite,
    ) -> Result<()>;

    /// [`upsert_for_doc`](Self::upsert_for_doc) for a refresh that isn't an
    /// edit: it inserts, updates and deletes only the rows whose values
    /// change, so a row that stays the same keeps its `updated_at` and
    /// `/tasks` (newest first) doesn't reshuffle. It notifies nobody.
    async fn sync_for_doc_quiet(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<()>;

    /// A user's open tasks across the workspace (all of them with
    /// `include_completed`), with their docs' titles. Tasks in archived docs
    /// and in the `hidden` docs (those the user can't read) are left out.
    async fn list_for_assignee(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        include_completed: bool,
        hidden: &[Uuid],
    ) -> Result<Vec<AssignedTask>>;

    /// All tasks in a doc, useful for the per-doc rendering surface.
    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<DocTask>>;
}

pub struct PgTaskStore {
    pool: PgPool,
}

impl PgTaskStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Replace the task set for `doc_id` with `items`, notifying as `write`
    /// says. With `quiet`, a row whose values stay the same is left as it
    /// is.
    async fn write_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
        write: TaskWrite,
        quiet: bool,
    ) -> Result<()> {
        let mut tx = crate::begin(&self.pool).await?;

        // Who was assigned what before this write, to tell a new assignment
        // from one that moved, was edited or was reopened.
        let previous: Vec<PreviousTask> = match write {
            TaskWrite::Edit(_) => {
                sqlx::query_as("SELECT text, assignee_user_id FROM doc_tasks WHERE doc_id = $1")
                    .bind(doc_id)
                    .fetch_all(&mut *tx)
                    .await?
            }
            TaskWrite::Copy => Vec::new(),
        };

        // Build the new ID set so we can prune rows that fell out.
        let new_ids: Vec<String> = items
            .iter()
            .map(|i| task_id(doc_id, i.item_index))
            .collect();

        // Delete rows for this doc that aren't in the new set. NOT IN with
        // an empty array would be a no-op, so handle that case explicitly.
        if new_ids.is_empty() {
            sqlx::query("DELETE FROM doc_tasks WHERE doc_id = $1")
                .bind(doc_id)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("DELETE FROM doc_tasks WHERE doc_id = $1 AND NOT (id = ANY($2))")
                .bind(doc_id)
                .bind(&new_ids)
                .execute(&mut *tx)
                .await?;
        }

        // Upsert each task. `completed_at` flips to NOW() when `checked`
        // transitions false → true, and to NULL on true → false. `$9`
        // (quiet) skips the update of a row whose values are the same.
        for item in items {
            let id = task_id(doc_id, item.item_index);
            sqlx::query(
                "INSERT INTO doc_tasks (id, workspace_id, doc_id, item_index, text, assignee_user_id, checked, completed_at, due_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, CASE WHEN $7 THEN NOW() ELSE NULL END, $8) \
                 ON CONFLICT (id) DO UPDATE SET \
                   text = EXCLUDED.text, \
                   assignee_user_id = EXCLUDED.assignee_user_id, \
                   checked = EXCLUDED.checked, \
                   due_at = EXCLUDED.due_at, \
                   completed_at = CASE \
                     WHEN doc_tasks.checked = EXCLUDED.checked THEN doc_tasks.completed_at \
                     WHEN EXCLUDED.checked THEN NOW() \
                     ELSE NULL \
                   END, \
                   updated_at = NOW() \
                 WHERE NOT $9 \
                    OR (doc_tasks.text, doc_tasks.assignee_user_id, doc_tasks.checked, doc_tasks.due_at) \
                       IS DISTINCT FROM (EXCLUDED.text, EXCLUDED.assignee_user_id, EXCLUDED.checked, EXCLUDED.due_at)",
            )
            .bind(&id)
            .bind(workspace_id)
            .bind(doc_id)
            .bind(item.item_index)
            .bind(&item.text)
            .bind(item.assignee_user_id)
            .bind(item.checked)
            .bind(item.due_at)
            .bind(quiet)
            .execute(&mut *tx)
            .await?;
        }

        if let TaskWrite::Edit(actor_id) = write {
            let changes = assignment_changes(&previous, items);
            // Edits first: a fresh task written under the key an edited
            // one moves to would leave the edited one's notification behind.
            for (before, item) in changes.edited {
                follow_edit(&mut tx, doc_id, before, item).await?;
            }
            for item in changes.fresh {
                notify_assignee(&mut tx, workspace_id, doc_id, item, actor_id).await?;
            }
        }

        tx.commit().await?;
        Ok(())
    }
}

fn task_id(doc_id: Uuid, item_index: i32) -> String {
    format!("{doc_id}:{item_index}")
}

#[async_trait]
impl TaskStore for PgTaskStore {
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
        write: TaskWrite,
    ) -> Result<()> {
        self.write_for_doc(workspace_id, doc_id, items, write, false)
            .await
    }

    async fn sync_for_doc_quiet(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<()> {
        self.write_for_doc(workspace_id, doc_id, items, TaskWrite::Copy, true)
            .await
    }

    async fn list_for_assignee(
        &self,
        workspace_id: Uuid,
        user_id: Uuid,
        include_completed: bool,
        hidden: &[Uuid],
    ) -> Result<Vec<AssignedTask>> {
        // `$3` (include_completed) keeps completed tasks and sorts checked
        // ones last; without it the order is by due date alone.
        let rows: Vec<AssignedTask> = sqlx::query_as::<_, AssignedTask>(
            "SELECT t.id, t.workspace_id, t.doc_id, t.item_index, t.text, t.assignee_user_id, t.checked, \
                    t.completed_at, t.due_at, t.created_at, t.updated_at, d.title AS doc_title \
             FROM doc_tasks t \
             JOIN documents d ON d.id = t.doc_id \
             WHERE t.workspace_id = $1 AND t.assignee_user_id = $2 \
             AND ($3 OR t.completed_at IS NULL) \
             AND d.archived_at IS NULL \
             AND NOT (d.id = ANY($4)) \
             ORDER BY ($3 AND t.checked), t.due_at NULLS LAST, t.updated_at DESC",
        )
        .bind(workspace_id)
        .bind(user_id)
        .bind(include_completed)
        .bind(hidden)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<DocTask>> {
        let rows: Vec<DocTask> = sqlx::query_as::<_, DocTask>(
            "SELECT id, workspace_id, doc_id, item_index, text, assignee_user_id, checked, completed_at, due_at, created_at, updated_at \
             FROM doc_tasks WHERE doc_id = $1 ORDER BY item_index",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}

/// A task row as it was before a write replaced it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct PreviousTask {
    text: String,
    assignee_user_id: Option<Uuid>,
}

/// What a write of a doc's tasks changes about who is assigned what.
#[derive(Debug, Default)]
struct AssignmentChanges<'a> {
    /// Open tasks whose assignee wasn't assigned them before.
    fresh: Vec<&'a DocTaskInput>,
    /// Open tasks their assignee already had, with the text they had then.
    edited: Vec<(&'a str, &'a DocTaskInput)>,
}

/// Compares the tasks a write brings, `items`, with the rows it replaces,
/// `previous`, assignee by assignee:
///
/// - A task with the same assignee and text as a previous row is that task,
///   moved or reopened or as it was. Row ids are `<doc_id>:<item_index>`,
///   so a task that moved has a new id; it is matched on its text instead.
/// - Of the rest, one whose text is like (see [`similarity`]) a previous
///   row's that is still unmatched is that task, edited. The reindex runs every two
///   seconds while someone types, so a task is often assigned before its
///   text is written.
/// - Any other open task is newly assigned.
fn assignment_changes<'a>(
    previous: &'a [PreviousTask],
    items: &'a [DocTaskInput],
) -> AssignmentChanges<'a> {
    let mut unmatched: Vec<&PreviousTask> = previous
        .iter()
        .filter(|row| row.assignee_user_id.is_some())
        .collect();
    let mut changed: Vec<&DocTaskInput> = Vec::new();
    for item in items.iter().filter(|item| item.assignee_user_id.is_some()) {
        let same = unmatched
            .iter()
            .position(|row| row.assignee_user_id == item.assignee_user_id && row.text == item.text);
        match same {
            Some(index) => {
                unmatched.swap_remove(index);
            }
            None => changed.push(item),
        }
    }
    // Each of the rest is matched to the unmatched row of its assignee it
    // is most like, the closest pairs first, so a task added next to an
    // edited one can't take that one's row.
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (item_at, item) in changed.iter().enumerate() {
        for (row_at, row) in unmatched.iter().enumerate() {
            if row.assignee_user_id == item.assignee_user_id
                && let Some(score) = similarity(&row.text, &item.text)
            {
                pairs.push((score, item_at, row_at));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut edit_of: Vec<Option<usize>> = vec![None; changed.len()];
    let mut row_taken = vec![false; unmatched.len()];
    for (_, item_at, row_at) in pairs {
        if edit_of[item_at].is_none() && !row_taken[row_at] {
            edit_of[item_at] = Some(row_at);
            row_taken[row_at] = true;
        }
    }
    let mut changes = AssignmentChanges::default();
    for (item, edit_of) in changed.into_iter().zip(edit_of) {
        match edit_of {
            Some(row_at) if !item.checked => changes.edited.push((&unmatched[row_at].text, item)),
            None if !item.checked => changes.fresh.push(item),
            _ => {}
        }
    }
    changes
}

/// How much `after` reads as an edit of `before`, or `None` when it reads
/// as another task: what the two share at their start and end must cover
/// at least half of the shorter one. Typing, fixing a typo or adding words
/// keeps a task; writing different words doesn't. The score is the share
/// of the longer text they have in common, 0 for a task whose text was
/// empty before (assigned before it was typed).
fn similarity(before: &str, after: &str) -> Option<f64> {
    let before: Vec<char> = before.chars().collect();
    let after: Vec<char> = after.chars().collect();
    let shorter = before.len().min(after.len());
    let longer = before.len().max(after.len());
    let prefix = before
        .iter()
        .zip(&after)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let shared = prefix + suffix;
    (2 * shared >= shorter).then(|| shared as f64 / longer.max(1) as f64)
}

/// Writes a `task_assigned` notification for `item`'s assignee, unless they
/// assigned it to themselves. On the task write's connection, so it lands
/// with the rows that say the task is theirs.
async fn notify_assignee(
    conn: &mut sqlx::PgConnection,
    workspace_id: Uuid,
    doc_id: Uuid,
    item: &DocTaskInput,
    actor_id: Option<Uuid>,
) -> Result<()> {
    let Some(assignee) = item.assignee_user_id else {
        return Ok(());
    };
    if Some(assignee) == actor_id {
        return Ok(());
    }
    let res = sqlx::query(
        "INSERT INTO notifications \
           (workspace_id, user_id, actor_id, kind, doc_id, target_kind, target_id, dedupe_key, data) \
         VALUES ($1, $2, $3, 'task_assigned', $4, 'task', $5, $6, $7) \
         ON CONFLICT (user_id, dedupe_key) DO NOTHING",
    )
    .bind(workspace_id)
    .bind(assignee)
    .bind(actor_id)
    .bind(doc_id)
    .bind(task_id(doc_id, item.item_index))
    .bind(task_assigned_dedupe_key(doc_id, &item.text, assignee))
    .bind(serde_json::json!({ "excerpt": item.text }))
    .execute(conn)
    .await?;
    // Counted here as `NotificationStore::emit` counts its own rows: only
    // the ones actually written.
    if res.rows_affected() == 1 {
        metrics::counter!("knot_notifications_emitted_total", "kind" => "task_assigned")
            .increment(1);
    }
    Ok(())
}

/// Moves an unread `task_assigned` notification along with an edit of its
/// task, from the text it had, `before`, to `item`'s: the notification
/// shows the task as it reads now. A read one stays as it was.
async fn follow_edit(
    conn: &mut sqlx::PgConnection,
    doc_id: Uuid,
    before: &str,
    item: &DocTaskInput,
) -> Result<()> {
    let Some(assignee) = item.assignee_user_id else {
        return Ok(());
    };
    sqlx::query(
        "UPDATE notifications \
         SET dedupe_key = $3, target_id = $4, \
             data = jsonb_set(data, '{excerpt}', to_jsonb($5::text)) \
         WHERE user_id = $1 AND dedupe_key = $2 AND read_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM notifications WHERE user_id = $1 AND dedupe_key = $3)",
    )
    .bind(assignee)
    .bind(task_assigned_dedupe_key(doc_id, before, assignee))
    .bind(task_assigned_dedupe_key(doc_id, &item.text, assignee))
    .bind(task_id(doc_id, item.item_index))
    .bind(&item.text)
    .execute(conn)
    .await?;
    Ok(())
}

/// The `dedupe_key` of a `task_assigned` notification:
/// `task_assigned:<doc_id>:<first 8 bytes of sha256(text) as hex>:<assignee>`.
/// Keyed on the text rather than the row id, which changes whenever the
/// task moves, so that two replicas indexing the same edit write one row.
/// Two open tasks with the same text and assignee in one doc share a key,
/// and notify once.
fn task_assigned_dedupe_key(doc_id: Uuid, text: &str, assignee: Uuid) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(text.as_bytes());
    let short: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    format!("task_assigned:{doc_id}:{short}:{assignee}")
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for DocTask {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> std::result::Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(DocTask {
            id: row.try_get("id")?,
            workspace_id: row.try_get("workspace_id")?,
            doc_id: row.try_get("doc_id")?,
            item_index: row.try_get("item_index")?,
            text: row.try_get("text")?,
            assignee_user_id: row.try_get("assignee_user_id")?,
            checked: row.try_get("checked")?,
            completed_at: row.try_get("completed_at")?,
            due_at: row.try_get("due_at")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
        })
    }
}

impl<'r> sqlx::FromRow<'r, sqlx::postgres::PgRow> for AssignedTask {
    fn from_row(row: &'r sqlx::postgres::PgRow) -> std::result::Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(AssignedTask {
            task: DocTask::from_row(row)?,
            doc_title: row.try_get("doc_title")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_small_fixes_are_edits() {
        assert!(similarity("", "Review the spec").is_some());
        assert!(similarity("Rev", "Review the spec").is_some());
        assert!(similarity("spec", "Review the spec").is_some());
        assert!(similarity("Reveiw the spec", "Review the spec").is_some());
        assert!(similarity("Review the spec", "Review the spec today").is_some());
        assert!(similarity("Jörg prüft 日本語", "Jörg prüft das 日本語").is_some());
    }

    #[test]
    fn different_words_are_another_task() {
        assert!(similarity("ship it", "book the venue for friday").is_none());
        assert!(similarity("Review the spec", "Call the venue").is_none());
    }

    fn row(text: &str, assignee: Uuid) -> PreviousTask {
        PreviousTask {
            text: text.into(),
            assignee_user_id: Some(assignee),
        }
    }

    fn item(item_index: i32, text: &str, assignee: Uuid, checked: bool) -> DocTaskInput {
        DocTaskInput {
            item_index,
            text: text.into(),
            assignee_user_id: Some(assignee),
            checked,
            due_at: None,
        }
    }

    #[test]
    fn a_moved_task_matches_on_its_text_before_any_edit_is_considered() {
        let bob = Uuid::new_v4();
        // "ship it now" is similar to "ship it", but "ship it" is still
        // there, moved: the new line is the fresh one.
        let previous = [row("ship it", bob)];
        let items = [
            item(0, "ship it now", bob, false),
            item(1, "ship it", bob, false),
        ];
        let changes = assignment_changes(&previous, &items);
        assert_eq!(changes.fresh.len(), 1);
        assert_eq!(changes.fresh[0].text, "ship it now");
        assert!(changes.edited.is_empty());
    }

    #[test]
    fn an_edit_keeps_its_row_from_a_task_added_beside_it() {
        let bob = Uuid::new_v4();
        let previous = [row("Review the spec", bob)];
        let items = [
            item(0, "Review the budget", bob, false),
            item(1, "Review the spec!", bob, false),
        ];
        let changes = assignment_changes(&previous, &items);
        assert_eq!(changes.fresh.len(), 1);
        assert_eq!(changes.fresh[0].text, "Review the budget");
        assert_eq!(changes.edited.len(), 1);
        assert_eq!(changes.edited[0].1.text, "Review the spec!");
    }

    #[test]
    fn checking_off_an_edited_task_is_neither_fresh_nor_followed() {
        let bob = Uuid::new_v4();
        let previous = [row("ship it", bob)];
        let items = [item(0, "ship it today", bob, true)];
        let changes = assignment_changes(&previous, &items);
        assert!(changes.fresh.is_empty());
        assert!(changes.edited.is_empty());
    }

    #[test]
    fn an_assignee_does_not_inherit_another_assignees_task() {
        let (bob, carol) = (Uuid::new_v4(), Uuid::new_v4());
        let previous = [row("ship it", bob)];
        let items = [item(0, "ship it today", carol, false)];
        let changes = assignment_changes(&previous, &items);
        assert_eq!(changes.fresh.len(), 1);
        assert!(changes.edited.is_empty());
    }
}
