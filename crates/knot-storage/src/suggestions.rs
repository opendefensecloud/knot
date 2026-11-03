//! Change suggestions on documents, and their reply threads.
//!
//! Schema:
//!   suggestions(id, doc_id, author_id, kind, position_y BYTEA, position_y_end BYTEA,
//!               original_text, context_before, proposed_text, reason NULL,
//!               status, decided_by NULL, decided_at NULL, apply_token NULL,
//!               created_at, updated_at)
//!   suggestion_replies(id, suggestion_id, author_id, body,
//!                      created_at, updated_at, deleted_at NULL)
//!
//! A suggestion never touches the Y.Doc until it is accepted. The status
//! column is the only coordination between replicas: every transition is one
//! conditional `UPDATE … WHERE status = <from> RETURNING …`, so two owners
//! clicking Accept at once move the row exactly once.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

/// Longest `original_text` / `proposed_text`, in characters.
pub const MAX_TEXT_CHARS: usize = 10_000;
/// Longest `context_before`, in characters.
pub const MAX_CONTEXT_CHARS: usize = 200;
/// Longest `reason`, in characters.
pub const MAX_REASON_CHARS: usize = 4_096;
/// Longest reply body, in characters.
pub const MAX_REPLY_CHARS: usize = 4_096;

/// serde helper: serialize `Vec<u8>` as a base64 string, so JS clients see
/// `position_y: "AbCd..."` instead of `[1,2,3,...]`. The non-`Option` sibling
/// of `comments::base64_opt`.
mod base64_bytes {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// What a suggestion does to the anchored range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionKind {
    /// Replace the range with `proposed_text`.
    Replace,
    /// Insert `proposed_text` at a collapsed anchor.
    Insert,
    /// Delete the range.
    Delete,
}

impl SuggestionKind {
    /// The value stored in `suggestions.kind` and sent on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Insert => "insert",
            Self::Delete => "delete",
        }
    }

    /// Inverse of [`Self::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "replace" => Some(Self::Replace),
            "insert" => Some(Self::Insert),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

/// Where a suggestion is in its life. `Applying` is the accept claim held
/// while the room applies the change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SuggestionStatus {
    /// Waiting for an owner's decision; the only state a decision starts from.
    Pending,
    /// Claimed for accept; the room is applying it to the Y.Doc.
    Applying,
    /// Applied to the document. Terminal.
    Accepted,
    /// Declined by an owner. Terminal.
    Rejected,
    /// Taken back by its author. Terminal.
    Withdrawn,
}

impl SuggestionStatus {
    /// The value stored in `suggestions.status` and sent on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Applying => "applying",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Withdrawn => "withdrawn",
        }
    }

    /// Inverse of [`Self::as_str`].
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "applying" => Some(Self::Applying),
            "accepted" => Some(Self::Accepted),
            "rejected" => Some(Self::Rejected),
            "withdrawn" => Some(Self::Withdrawn),
            _ => None,
        }
    }
}

/// A stored change suggestion.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Suggestion {
    /// Primary key.
    pub id: Uuid,
    /// The document the suggestion is anchored in.
    pub doc_id: Uuid,
    /// The user who suggested it.
    pub author_id: Uuid,
    /// What the suggestion does to the range.
    pub kind: SuggestionKind,
    /// Yjs RelativePosition of the range start, base64 on the wire.
    #[serde(with = "base64_bytes")]
    pub position_y: Vec<u8>,
    /// Yjs RelativePosition of the range end (equal to the start for an
    /// insertion), base64 on the wire.
    #[serde(with = "base64_bytes")]
    pub position_y_end: Vec<u8>,
    /// The text of the range when suggested; empty for an insertion.
    pub original_text: String,
    /// A short snippet before the anchor, for insertion cards.
    pub context_before: String,
    /// The replacement; empty for a deletion.
    pub proposed_text: String,
    /// The author's optional explanation.
    pub reason: Option<String>,
    /// Where it is in its life.
    pub status: SuggestionStatus,
    /// The owner who claimed, accepted or rejected it.
    pub decided_by: Option<Uuid>,
    /// When it was accepted, rejected or withdrawn.
    pub decided_at: Option<DateTime<Utc>>,
    /// The token the first accept claim minted; the room records it as the
    /// value of the doc's applied marker, so only a marker holding it counts
    /// (clients can write that map). Server-side only: never serialized,
    /// since a client that knew it before the apply could forge the marker.
    #[serde(skip)]
    pub apply_token: Option<Uuid>,
    /// When it was suggested.
    pub created_at: DateTime<Utc>,
    /// Bumped on every status change; the stuck-claim sweep compares it.
    pub updated_at: DateTime<Utc>,
}

/// The fields a caller supplies to create a suggestion.
#[derive(Debug, Clone)]
pub struct NewSuggestion {
    /// The document to anchor in.
    pub doc_id: Uuid,
    /// The suggesting user.
    pub author_id: Uuid,
    /// What the suggestion does to the range.
    pub kind: SuggestionKind,
    /// Encoded Yjs RelativePosition of the range start.
    pub position_y: Vec<u8>,
    /// Encoded Yjs RelativePosition of the range end.
    pub position_y_end: Vec<u8>,
    /// The text of the range; empty for an insertion. At most [`MAX_TEXT_CHARS`].
    pub original_text: String,
    /// Snippet before the anchor. At most [`MAX_CONTEXT_CHARS`].
    pub context_before: String,
    /// The replacement; empty for a deletion. At most [`MAX_TEXT_CHARS`].
    pub proposed_text: String,
    /// Optional explanation. At most [`MAX_REASON_CHARS`].
    pub reason: Option<String>,
}

/// A reply in a suggestion's thread.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, sqlx::FromRow)]
pub struct SuggestionReply {
    /// Primary key.
    pub id: Uuid,
    /// The suggestion whose thread this is in.
    pub suggestion_id: Uuid,
    /// The replying user.
    pub author_id: Uuid,
    /// Plain-text body, 1..[`MAX_REPLY_CHARS`] characters.
    pub body: String,
    /// When it was posted.
    pub created_at: DateTime<Utc>,
    /// When it was last edited (equal to `created_at` until then).
    pub updated_at: DateTime<Utc>,
}

/// Errors from [`SuggestionStore`].
#[derive(Debug, Error)]
pub enum SuggestionStoreError {
    /// The suggestion or reply does not exist (or the reply is deleted).
    #[error("not found")]
    NotFound,
    /// The row exists but is not in the state the transition starts from.
    #[error("suggestion not pending")]
    NotPending,
    /// A text exceeds its character limit. Checked before SQL.
    #[error("text too long")]
    TooLong,
    /// A reply body is empty. Checked before SQL; routes refuse an empty body
    /// themselves (422 `suggestion.empty_body`) and map this to the same.
    #[error("reply body empty")]
    EmptyBody,
    /// Any other database error.
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

/// Result alias for [`SuggestionStore`] calls.
pub type Result<T> = std::result::Result<T, SuggestionStoreError>;

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Persistence for suggestions and their reply threads.
#[async_trait]
pub trait SuggestionStore: Send + Sync + 'static {
    /// Insert a pending suggestion. `TooLong` when a text exceeds its limit,
    /// checked here before SQL.
    async fn create(&self, s: &NewSuggestion) -> Result<Suggestion>;

    /// Fetch one suggestion in any status.
    async fn get(&self, id: Uuid) -> Result<Suggestion>;

    /// include_resolved=false → pending + applying only. Ordered by created_at.
    async fn list(&self, doc_id: Uuid, include_resolved: bool) -> Result<Vec<Suggestion>>;

    /// pending → applying, sets decided_by and updated_at = now(), and mints a random
    /// `apply_token` unless the row has one from an earlier claim (kept, so a re-accept of
    /// a suggestion applied after its row was released sees its own marker).
    /// NotPending if no row moved.
    async fn claim_for_accept(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion>;

    /// (applying | pending) → accepted, sets decided_at and updated_at. The doc's applied marker
    /// is authoritative, so this also finishes a row a sweep released meanwhile. NotPending otherwise.
    async fn finish_accept(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion>;

    /// applying → pending, clears decided_by, bumps updated_at. NotPending
    /// when the row is not applying.
    async fn release(&self, id: Uuid) -> Result<()>;

    /// Like release, but only if updated_at still equals `seen_updated_at` (the sweep's compare-and-set).
    /// Returns whether the row moved.
    async fn release_if_unchanged(&self, id: Uuid, seen_updated_at: DateTime<Utc>) -> Result<bool>;

    /// pending → rejected, sets decided_by and decided_at.
    async fn reject(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion>;

    /// pending → withdrawn, sets decided_at. The author withdraws, so
    /// decided_by stays NULL.
    async fn withdraw(&self, id: Uuid) -> Result<Suggestion>;

    /// Rows in `applying` whose updated_at is before `older_than`, oldest first.
    async fn list_stuck_applying(&self, older_than: DateTime<Utc>) -> Result<Vec<Suggestion>>;

    /// Add a reply. The store enforces both bounds of the 1..[`MAX_REPLY_CHARS`]
    /// body limit before SQL: `EmptyBody` when empty, `TooLong` over the limit,
    /// so neither ever reaches Postgres' CHECK as a `Sqlx` error. `NotFound`
    /// when the suggestion does not exist.
    async fn create_reply(
        &self,
        suggestion_id: Uuid,
        author_id: Uuid,
        body: &str,
    ) -> Result<SuggestionReply>;

    /// Non-deleted replies of every suggestion of the doc, oldest first.
    async fn list_replies(&self, doc_id: Uuid) -> Result<Vec<SuggestionReply>>;

    /// Fetch one non-deleted reply.
    async fn get_reply(&self, id: Uuid) -> Result<SuggestionReply>;

    /// Replace a non-deleted reply's body and bump updated_at. Same body
    /// checks as [`Self::create_reply`]; `NotFound` when missing or deleted.
    async fn update_reply(&self, id: Uuid, body: &str) -> Result<SuggestionReply>;

    /// Soft-delete a reply. Idempotent: a deleted or unknown id is `Ok`.
    async fn delete_reply(&self, id: Uuid) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Postgres implementation
// ---------------------------------------------------------------------------

/// [`SuggestionStore`] backed by Postgres.
pub struct PgSuggestionStore {
    pool: PgPool,
}

impl PgSuggestionStore {
    /// Wrap a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// The underlying pool, for tests and callers that need raw SQL.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Classify a transition that moved no row: `NotFound` when the row is
    /// gone, otherwise `NotPending`.
    async fn no_transition(&self, id: Uuid) -> SuggestionStoreError {
        let exists: std::result::Result<Option<i32>, _> =
            sqlx::query_scalar("SELECT 1 FROM suggestions WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await;
        match exists {
            Ok(Some(_)) => SuggestionStoreError::NotPending,
            Ok(None) => SuggestionStoreError::NotFound,
            Err(e) => e.into(),
        }
    }

    /// Run one conditional status UPDATE. `sql` binds the id as `$1`, `by`
    /// (when given) as `$2`, and returns [`COLS`]; zero rows is classified
    /// by `no_transition`.
    async fn transition(&self, id: Uuid, sql: String, by: Option<Uuid>) -> Result<Suggestion> {
        let mut q = sqlx::query_as::<_, SuggestionRow>(sqlx::AssertSqlSafe(sql)).bind(id);
        if let Some(by) = by {
            q = q.bind(by);
        }
        match q.fetch_optional(&self.pool).await? {
            Some(row) => row.try_into(),
            None => Err(self.no_transition(id).await),
        }
    }
}

/// The `RETURNING` / `SELECT` column list every suggestion read shares.
const COLS: &str = "id, doc_id, author_id, kind, position_y, position_y_end, \
     original_text, context_before, proposed_text, reason, status, \
     decided_by, decided_at, apply_token, created_at, updated_at";

const REPLY_COLS: &str = "id, suggestion_id, author_id, body, created_at, updated_at";

/// Internal row shape: `kind` and `status` are text in the table.
#[derive(sqlx::FromRow)]
struct SuggestionRow {
    id: Uuid,
    doc_id: Uuid,
    author_id: Uuid,
    kind: String,
    position_y: Vec<u8>,
    position_y_end: Vec<u8>,
    original_text: String,
    context_before: String,
    proposed_text: String,
    reason: Option<String>,
    status: String,
    decided_by: Option<Uuid>,
    decided_at: Option<DateTime<Utc>>,
    apply_token: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl TryFrom<SuggestionRow> for Suggestion {
    type Error = SuggestionStoreError;

    fn try_from(r: SuggestionRow) -> Result<Self> {
        let decode = |what: &str, v: &str| {
            SuggestionStoreError::Sqlx(sqlx::Error::Decode(
                format!("unknown suggestion {what} {v:?}").into(),
            ))
        };
        let kind = SuggestionKind::parse(&r.kind).ok_or_else(|| decode("kind", &r.kind))?;
        let status =
            SuggestionStatus::parse(&r.status).ok_or_else(|| decode("status", &r.status))?;
        Ok(Suggestion {
            id: r.id,
            doc_id: r.doc_id,
            author_id: r.author_id,
            kind,
            position_y: r.position_y,
            position_y_end: r.position_y_end,
            original_text: r.original_text,
            context_before: r.context_before,
            proposed_text: r.proposed_text,
            reason: r.reason,
            status,
            decided_by: r.decided_by,
            decided_at: r.decided_at,
            apply_token: r.apply_token,
            created_at: r.created_at,
            updated_at: r.updated_at,
        })
    }
}

fn too_long(s: &str, max: usize) -> bool {
    s.chars().count() > max
}

/// Enforce the reply body limit 1..[`MAX_REPLY_CHARS`] (the DB CHECK's bounds).
fn check_reply_body(body: &str) -> Result<()> {
    if body.is_empty() {
        return Err(SuggestionStoreError::EmptyBody);
    }
    if too_long(body, MAX_REPLY_CHARS) {
        return Err(SuggestionStoreError::TooLong);
    }
    Ok(())
}

/// `NotFound` for a foreign-key violation (the suggestion is gone), the
/// sqlx error otherwise.
fn map_fk(e: sqlx::Error) -> SuggestionStoreError {
    match &e {
        sqlx::Error::Database(db) if db.is_foreign_key_violation() => {
            SuggestionStoreError::NotFound
        }
        _ => e.into(),
    }
}

#[async_trait]
impl SuggestionStore for PgSuggestionStore {
    async fn create(&self, s: &NewSuggestion) -> Result<Suggestion> {
        if too_long(&s.original_text, MAX_TEXT_CHARS)
            || too_long(&s.proposed_text, MAX_TEXT_CHARS)
            || too_long(&s.context_before, MAX_CONTEXT_CHARS)
            || s.reason
                .as_deref()
                .is_some_and(|r| too_long(r, MAX_REASON_CHARS))
        {
            return Err(SuggestionStoreError::TooLong);
        }
        let sql = format!(
            "INSERT INTO suggestions \
               (id, doc_id, author_id, kind, position_y, position_y_end, \
                original_text, context_before, proposed_text, reason) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             RETURNING {COLS}"
        );
        let row: SuggestionRow = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(Uuid::new_v4())
            .bind(s.doc_id)
            .bind(s.author_id)
            .bind(s.kind.as_str())
            .bind(&s.position_y)
            .bind(&s.position_y_end)
            .bind(&s.original_text)
            .bind(&s.context_before)
            .bind(&s.proposed_text)
            .bind(&s.reason)
            .fetch_one(&self.pool)
            .await?;
        row.try_into()
    }

    async fn get(&self, id: Uuid) -> Result<Suggestion> {
        let sql = format!("SELECT {COLS} FROM suggestions WHERE id = $1");
        let row: Option<SuggestionRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.ok_or(SuggestionStoreError::NotFound)?.try_into()
    }

    async fn list(&self, doc_id: Uuid, include_resolved: bool) -> Result<Vec<Suggestion>> {
        let sql = format!(
            "SELECT {COLS} FROM suggestions \
             WHERE doc_id = $1 AND ($2 OR status IN ('pending', 'applying')) \
             ORDER BY created_at, id"
        );
        let rows: Vec<SuggestionRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(doc_id)
            .bind(include_resolved)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(Suggestion::try_from).collect()
    }

    async fn claim_for_accept(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion> {
        let sql = format!(
            "UPDATE suggestions \
             SET status = 'applying', decided_by = $2, updated_at = now(), \
                 apply_token = COALESCE(apply_token, $3) \
             WHERE id = $1 AND status = 'pending' \
             RETURNING {COLS}"
        );
        let row: Option<SuggestionRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .bind(decided_by)
            .bind(Uuid::new_v4())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => row.try_into(),
            None => Err(self.no_transition(id).await),
        }
    }

    async fn finish_accept(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion> {
        let sql = format!(
            "UPDATE suggestions \
             SET status = 'accepted', decided_by = $2, decided_at = now(), updated_at = now() \
             WHERE id = $1 AND status IN ('applying', 'pending') \
             RETURNING {COLS}"
        );
        self.transition(id, sql, Some(decided_by)).await
    }

    async fn release(&self, id: Uuid) -> Result<()> {
        let moved = sqlx::query(
            "UPDATE suggestions \
             SET status = 'pending', decided_by = NULL, updated_at = now() \
             WHERE id = $1 AND status = 'applying'",
        )
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if moved == 0 {
            return Err(self.no_transition(id).await);
        }
        Ok(())
    }

    async fn release_if_unchanged(&self, id: Uuid, seen_updated_at: DateTime<Utc>) -> Result<bool> {
        let moved = sqlx::query(
            "UPDATE suggestions \
             SET status = 'pending', decided_by = NULL, updated_at = now() \
             WHERE id = $1 AND status = 'applying' AND updated_at = $2",
        )
        .bind(id)
        .bind(seen_updated_at)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(moved == 1)
    }

    async fn reject(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion> {
        let sql = format!(
            "UPDATE suggestions \
             SET status = 'rejected', decided_by = $2, decided_at = now(), updated_at = now() \
             WHERE id = $1 AND status = 'pending' \
             RETURNING {COLS}"
        );
        self.transition(id, sql, Some(decided_by)).await
    }

    async fn withdraw(&self, id: Uuid) -> Result<Suggestion> {
        let sql = format!(
            "UPDATE suggestions \
             SET status = 'withdrawn', decided_at = now(), updated_at = now() \
             WHERE id = $1 AND status = 'pending' \
             RETURNING {COLS}"
        );
        self.transition(id, sql, None).await
    }

    async fn list_stuck_applying(&self, older_than: DateTime<Utc>) -> Result<Vec<Suggestion>> {
        let sql = format!(
            "SELECT {COLS} FROM suggestions \
             WHERE status = 'applying' AND updated_at < $1 \
             ORDER BY updated_at"
        );
        let rows: Vec<SuggestionRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(older_than)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter().map(Suggestion::try_from).collect()
    }

    async fn create_reply(
        &self,
        suggestion_id: Uuid,
        author_id: Uuid,
        body: &str,
    ) -> Result<SuggestionReply> {
        check_reply_body(body)?;
        let sql = format!(
            "INSERT INTO suggestion_replies (id, suggestion_id, author_id, body) \
             VALUES ($1, $2, $3, $4) \
             RETURNING {REPLY_COLS}"
        );
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(Uuid::new_v4())
            .bind(suggestion_id)
            .bind(author_id)
            .bind(body)
            .fetch_one(&self.pool)
            .await
            .map_err(map_fk)
    }

    async fn list_replies(&self, doc_id: Uuid) -> Result<Vec<SuggestionReply>> {
        let rows = sqlx::query_as(
            "SELECT r.id, r.suggestion_id, r.author_id, r.body, r.created_at, r.updated_at \
             FROM suggestion_replies r \
             JOIN suggestions s ON s.id = r.suggestion_id \
             WHERE s.doc_id = $1 AND r.deleted_at IS NULL \
             ORDER BY r.created_at, r.id",
        )
        .bind(doc_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn get_reply(&self, id: Uuid) -> Result<SuggestionReply> {
        let sql = format!(
            "SELECT {REPLY_COLS} FROM suggestion_replies \
             WHERE id = $1 AND deleted_at IS NULL"
        );
        let row: Option<SuggestionReply> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.ok_or(SuggestionStoreError::NotFound)
    }

    async fn update_reply(&self, id: Uuid, body: &str) -> Result<SuggestionReply> {
        check_reply_body(body)?;
        let sql = format!(
            "UPDATE suggestion_replies \
             SET body = $2, updated_at = now() \
             WHERE id = $1 AND deleted_at IS NULL \
             RETURNING {REPLY_COLS}"
        );
        let row: Option<SuggestionReply> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .bind(body)
            .fetch_optional(&self.pool)
            .await?;
        row.ok_or(SuggestionStoreError::NotFound)
    }

    async fn delete_reply(&self, id: Uuid) -> Result<()> {
        sqlx::query(
            "UPDATE suggestion_replies SET deleted_at = now() \
             WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
