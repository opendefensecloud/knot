//! Storing a "Move to subpage" (`2026-11-13-move-to-subpage-design.md`):
//! the new child page with the moved blocks as its content, and the
//! comments and suggestions anchored entirely inside them, in one
//! transaction. The source's cut is the room's (`knot_crdt::Event::
//! MoveBlocksOut`), after this commits.

use sqlx::types::JsonValue;
use sqlx::{AssertSqlSafe, PgPool};
use uuid::Uuid;

use crate::doc_store::{COLS, DocRow, DocStoreError, Document, doc_from_row, map_unique};
use crate::placement::{self, Placement};
use crate::updates_store::{NewUpdate, UpdateSource, append_in_tx};
use crate::{audit, invalidations};

/// The page to create.
#[derive(Debug, Clone)]
pub struct SubpagePlan {
    pub ws: Uuid,
    pub actor: Uuid,
    /// The page the blocks move out of: the new page's parent.
    pub source_id: Uuid,
    pub new_id: Uuid,
    pub title: String,
    /// The new page's content as one Yjs update.
    pub state: Vec<u8>,
}

#[derive(Debug)]
pub struct SubpageOutcome {
    pub doc: Document,
    /// The seq of the update holding its content.
    pub seq: i64,
    /// Threads moved.
    pub comments: usize,
    pub suggestions: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SubpageError {
    /// A suggestion to move is being applied to the source right now; its
    /// applied marker lives there. Retrying can succeed.
    #[error("suggestion applying")]
    SuggestionApplying,
    #[error(transparent)]
    Store(#[from] DocStoreError),
}

impl From<sqlx::Error> for SubpageError {
    fn from(e: sqlx::Error) -> Self {
        SubpageError::Store(e.into())
    }
}

/// Creates the page of `plan`, last under its source, and moves to it the
/// comment threads and suggestions of the source whose anchors (start, end)
/// `inside` accepts. Threads without an anchor (whole-page) stay.
pub async fn create_subpage(
    pool: &PgPool,
    plan: &SubpagePlan,
    inside: impl Fn(&[u8], &[u8]) -> bool,
) -> Result<SubpageOutcome, SubpageError> {
    let mut tx = crate::begin(pool).await?;
    // Under a key no one else sees before the commit; the real one comes
    // last, under the group's lock (as `duplicate`).
    sqlx::query(
        "INSERT INTO documents (id, workspace_id, parent_id, title, sort_key, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(plan.new_id)
    .bind(plan.ws)
    .bind(plan.source_id)
    .bind(&plan.title)
    .bind(format!("~{}", plan.new_id))
    .bind(plan.actor)
    .execute(&mut *tx)
    .await?;
    let row = NewUpdate {
        by_user_id: Some(plan.actor),
        bytes: plan.state.clone(),
        source: UpdateSource::Copy,
        meta: Some(serde_json::json!({ "moved_from": plan.source_id })),
    };
    let seq = append_in_tx(&mut tx, plan.new_id, 0, &[row])
        .await
        .map_err(DocStoreError::from)?[0]
        .seq;

    // The rows to move, read under lock so none changes meanwhile.
    let threads: Vec<(Uuid, Vec<u8>, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT id, position_y, position_y_end FROM comments
          WHERE doc_id = $1 AND parent_id IS NULL AND position_y IS NOT NULL
          FOR UPDATE",
    )
    .bind(plan.source_id)
    .fetch_all(&mut *tx)
    .await?;
    let threads: Vec<Uuid> = threads
        .into_iter()
        .filter(|(_, start, end)| inside(start, end.as_deref().unwrap_or(start)))
        .map(|(id, ..)| id)
        .collect();
    let suggestions: Vec<(Uuid, String, Vec<u8>, Vec<u8>)> = sqlx::query_as(
        "SELECT id, status, position_y, position_y_end FROM suggestions
          WHERE doc_id = $1 FOR UPDATE",
    )
    .bind(plan.source_id)
    .fetch_all(&mut *tx)
    .await?;
    let suggestions: Vec<(Uuid, String)> = suggestions
        .into_iter()
        .filter(|(_, _, start, end)| inside(start, end))
        .map(|(id, status, ..)| (id, status))
        .collect();
    if suggestions.iter().any(|(_, status)| status == "applying") {
        return Err(SubpageError::SuggestionApplying);
    }
    let suggestions: Vec<Uuid> = suggestions.into_iter().map(|(id, _)| id).collect();

    // Whole threads: replies carry their own doc_id.
    let mut moved: Vec<String> = sqlx::query_scalar(
        "UPDATE comments SET doc_id = $2 WHERE doc_id = $1 AND thread_id = ANY($3)
         RETURNING id::text",
    )
    .bind(plan.source_id)
    .bind(plan.new_id)
    .bind(&threads)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("UPDATE suggestions SET doc_id = $2 WHERE doc_id = $1 AND id = ANY($3)")
        .bind(plan.source_id)
        .bind(plan.new_id)
        .bind(&suggestions)
        .execute(&mut *tx)
        .await?;
    // Mentions and replies about them now link to the new page.
    moved.extend(suggestions.iter().map(Uuid::to_string));
    sqlx::query("UPDATE notifications SET doc_id = $2 WHERE doc_id = $1 AND target_id = ANY($3)")
        .bind(plan.source_id)
        .bind(plan.new_id)
        .bind(&moved)
        .execute(&mut *tx)
        .await?;

    let data = JsonValue::from_iter([("source_id", JsonValue::from(plan.source_id.to_string()))]);
    audit::record_in_tx_with_data(
        &mut tx,
        plan.ws,
        Some(plan.actor),
        "doc.move_to_subpage",
        "doc",
        plan.new_id,
        &data,
    )
    .await?;
    invalidations::record_in_tx(&mut tx, plan.ws, plan.new_id, "create").await?;
    let sort_key = placement::place(
        &mut tx,
        plan.ws,
        Some(plan.source_id),
        Placement::Last,
        Some(plan.new_id),
    )
    .await?;
    let row = sqlx::query_as::<_, DocRow>(AssertSqlSafe(format!(
        "UPDATE documents SET sort_key = $2 WHERE id = $1 RETURNING {COLS}"
    )))
    .bind(plan.new_id)
    .bind(&sort_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_unique)?;
    tx.commit().await?;
    Ok(SubpageOutcome {
        doc: doc_from_row(row),
        seq,
        comments: threads.len(),
        suggestions: suggestions.len(),
    })
}
