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
    async fn upsert_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<()>;

    /// [`upsert_for_doc`](Self::upsert_for_doc) for a refresh that isn't an
    /// edit: it inserts, updates and deletes only the rows whose values
    /// change, so a row that stays the same keeps its `updated_at` and
    /// `/tasks` (newest first) doesn't reshuffle.
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

    /// Replace the task set for `doc_id` with `items`. With `quiet`, a row
    /// whose values stay the same is left as it is.
    async fn write_for_doc(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
        quiet: bool,
    ) -> Result<()> {
        let mut tx = crate::begin(&self.pool).await?;

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
    ) -> Result<()> {
        self.write_for_doc(workspace_id, doc_id, items, false).await
    }

    async fn sync_for_doc_quiet(
        &self,
        workspace_id: Uuid,
        doc_id: Uuid,
        items: &[DocTaskInput],
    ) -> Result<()> {
        self.write_for_doc(workspace_id, doc_id, items, true).await
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
