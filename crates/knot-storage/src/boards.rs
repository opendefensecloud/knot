//! Excalidraw-style boards: persistence + Yjs update log + snapshots.
//!
//! A board is a Yjs sub-document attached to a parent document. The
//! `boards` row holds metadata (parent doc, label, latest rendered SVG
//! preview); `board_updates` is the append-only y-update log; and
//! `board_snapshots` is the periodic compaction ladder.
//!
//! ACL is inherited from the parent document — there is no separate
//! grants table.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Board {
    pub id: Uuid,
    pub doc_id: Uuid,
    pub created_by: Uuid,
    pub label: Option<String>,
    pub svg_seq: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Error)]
pub enum BoardStoreError {
    #[error("not found")]
    NotFound,
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

pub type Result<T> = std::result::Result<T, BoardStoreError>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

#[async_trait]
pub trait BoardStore: Send + Sync + 'static {
    /// Create a new board attached to a parent doc.
    async fn create(&self, doc_id: Uuid, created_by: Uuid, label: Option<String>) -> Result<Board>;

    async fn get(&self, id: Uuid) -> Result<Board>;

    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<Board>>;

    async fn delete(&self, id: Uuid) -> Result<()>;

    /// Append a y-update for a board. Returns the new seq.
    async fn append_update(&self, id: Uuid, bytes: &[u8]) -> Result<i64>;

    /// Load updates for replay on room boot, in seq order.
    async fn load_updates(&self, id: Uuid) -> Result<Vec<Vec<u8>>>;

    /// Highest update seq, or 0 if none.
    async fn max_update_seq(&self, id: Uuid) -> Result<i64>;

    /// Persist a snapshot at a given seq.
    async fn put_snapshot(&self, id: Uuid, seq: i64, state: &[u8]) -> Result<()>;

    /// Latest snapshot (seq, bytes), if any.
    async fn latest_snapshot(&self, id: Uuid) -> Result<Option<(i64, Vec<u8>)>>;

    /// Store the latest client-rendered SVG preview for inline display.
    async fn set_svg(&self, id: Uuid, bytes: &[u8]) -> Result<()>;

    /// Fetch the cached SVG, if any.
    async fn get_svg(&self, id: Uuid) -> Result<Option<Vec<u8>>>;
}

// ---------------------------------------------------------------------------
// Postgres implementation
// ---------------------------------------------------------------------------

pub struct PgBoardStore {
    pool: PgPool,
}

impl PgBoardStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct BoardRow {
    id: Uuid,
    doc_id: Uuid,
    created_by: Uuid,
    label: Option<String>,
    svg_seq: i64,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn row_to_board(r: BoardRow) -> Board {
    Board {
        id: r.id,
        doc_id: r.doc_id,
        created_by: r.created_by,
        label: r.label,
        svg_seq: r.svg_seq,
        created_at: r.created_at,
        updated_at: r.updated_at,
    }
}

#[async_trait]
impl BoardStore for PgBoardStore {
    async fn create(&self, doc_id: Uuid, created_by: Uuid, label: Option<String>) -> Result<Board> {
        let id = Uuid::new_v4();
        let row: BoardRow = sqlx::query_as(
            "INSERT INTO boards (id, doc_id, created_by, label)
             VALUES ($1, $2, $3, $4)
             RETURNING id, doc_id, created_by, label, svg_seq, created_at, updated_at",
        )
        .bind(id)
        .bind(doc_id)
        .bind(created_by)
        .bind(label)
        .fetch_one(&self.pool)
        .await?;
        Ok(row_to_board(row))
    }

    async fn get(&self, id: Uuid) -> Result<Board> {
        let row: Option<BoardRow> = sqlx::query_as(
            "SELECT id, doc_id, created_by, label, svg_seq, created_at, updated_at
             FROM boards
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_board).ok_or(BoardStoreError::NotFound)
    }

    async fn list_for_doc(&self, doc_id: Uuid) -> Result<Vec<Board>> {
        let rows: Vec<BoardRow> = sqlx::query_as(
            "SELECT id, doc_id, created_by, label, svg_seq, created_at, updated_at
             FROM boards
             WHERE doc_id = $1 AND deleted_at IS NULL
             ORDER BY created_at",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(row_to_board).collect())
    }

    async fn delete(&self, id: Uuid) -> Result<()> {
        sqlx::query("UPDATE boards SET deleted_at = NOW() WHERE id = $1 AND deleted_at IS NULL")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn append_update(&self, id: Uuid, bytes: &[u8]) -> Result<i64> {
        let seq: i64 = sqlx::query_scalar(
            "INSERT INTO board_updates (board_id, bytes) VALUES ($1, $2) RETURNING seq",
        )
        .bind(id)
        .bind(bytes)
        .fetch_one(&self.pool)
        .await?;
        sqlx::query("UPDATE boards SET updated_at = NOW() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(seq)
    }

    async fn load_updates(&self, id: Uuid) -> Result<Vec<Vec<u8>>> {
        let rows: Vec<(Vec<u8>,)> =
            sqlx::query_as("SELECT bytes FROM board_updates WHERE board_id = $1 ORDER BY seq")
                .bind(id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().map(|(b,)| b).collect())
    }

    async fn max_update_seq(&self, id: Uuid) -> Result<i64> {
        let max: Option<i64> =
            sqlx::query_scalar("SELECT MAX(seq) FROM board_updates WHERE board_id = $1")
                .bind(id)
                .fetch_one(&self.pool)
                .await?;
        Ok(max.unwrap_or(0))
    }

    async fn put_snapshot(&self, id: Uuid, seq: i64, state: &[u8]) -> Result<()> {
        sqlx::query(
            "INSERT INTO board_snapshots (board_id, snapshot_seq, state, byte_size)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (board_id, snapshot_seq) DO NOTHING",
        )
        .bind(id)
        .bind(seq)
        .bind(state)
        .bind(state.len() as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn latest_snapshot(&self, id: Uuid) -> Result<Option<(i64, Vec<u8>)>> {
        let row: Option<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT snapshot_seq, state FROM board_snapshots
             WHERE board_id = $1
             ORDER BY snapshot_seq DESC
             LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn set_svg(&self, id: Uuid, bytes: &[u8]) -> Result<()> {
        sqlx::query(
            "UPDATE boards
             SET svg_cached = $2, svg_seq = svg_seq + 1, updated_at = NOW()
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .bind(bytes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_svg(&self, id: Uuid) -> Result<Option<Vec<u8>>> {
        let row: Option<(Option<Vec<u8>>,)> =
            sqlx::query_as("SELECT svg_cached FROM boards WHERE id = $1 AND deleted_at IS NULL")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(b,)| b))
    }
}
