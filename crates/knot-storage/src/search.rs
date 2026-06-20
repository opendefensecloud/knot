//! Postgres-backed full-text search across docs in a workspace.
//!
//! Indexes:
//! - `documents.title_tsv` (STORED GENERATED, english)
//! - `doc_markdown_cache.body_tsv` (STORED GENERATED, english)
//!
//! Body search is eventually consistent — the cache lags live editor
//! state until the next snapshot. v0.1 accepts this lag.
//!
//! Query model: prefix matching via `to_tsquery` with `:*` suffix on each
//! token. The user's raw input goes through `to_prefix_tsquery` which
//! strips operator-significant characters, drops tokens under 2 chars, and
//! ANDs the remaining prefixes. Empty input after sanitization → empty
//! result list (no DB hit).

use async_trait::async_trait;
use sqlx::PgPool;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum SearchStoreError {
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub doc_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub title: String,
    pub snippet: String,
    pub rank: f32,
}

#[async_trait]
pub trait SearchStore: Send + Sync {
    async fn search(
        &self,
        workspace_id: Uuid,
        q: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, SearchStoreError>;
}

/// Build a `to_tsquery`-safe expression with `:*` prefix suffix on each
/// term. Returns `None` when the sanitized query has no usable tokens.
pub(crate) fn to_prefix_tsquery(raw: &str) -> Option<String> {
    let mut tokens: Vec<String> = raw
        .split_whitespace()
        .map(|t| {
            t.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|t| t.chars().count() >= 2)
        .map(|t| format!("{t}:*"))
        .collect();
    if tokens.is_empty() {
        return None;
    }
    // Cap clause count so a 200-word query doesn't explode planner time.
    tokens.truncate(8);
    Some(tokens.join(" & "))
}

pub struct PgSearchStore {
    pool: PgPool,
}

impl PgSearchStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SearchStore for PgSearchStore {
    async fn search(
        &self,
        workspace_id: Uuid,
        q: &str,
        limit: i64,
    ) -> Result<Vec<SearchHit>, SearchStoreError> {
        let Some(ts) = to_prefix_tsquery(q) else {
            return Ok(vec![]);
        };
        let rows: Vec<(Uuid, Option<Uuid>, String, Option<String>, f32)> = sqlx::query_as(
            r#"
            SELECT d.id,
                   d.parent_id,
                   d.title,
                   CASE
                     WHEN c.body_tsv @@ to_tsquery('english', $2) THEN
                       ts_headline('english', c.markdown_text,
                                   to_tsquery('english', $2),
                                   'MaxFragments=2,MinWords=5,MaxWords=15,StartSel=<b>,StopSel=</b>')
                     ELSE NULL
                   END AS snippet,
                   GREATEST(
                     COALESCE(ts_rank_cd(d.title_tsv, to_tsquery('english', $2)), 0.0) * 2.0,
                     COALESCE(ts_rank_cd(c.body_tsv,  to_tsquery('english', $2)), 0.0)
                   )::real AS rank
              FROM documents d
              LEFT JOIN doc_markdown_cache c ON c.doc_id = d.id
             WHERE d.workspace_id = $1
               AND d.archived_at IS NULL
               AND (
                     d.title_tsv @@ to_tsquery('english', $2)
                  OR c.body_tsv  @@ to_tsquery('english', $2)
                   )
             ORDER BY rank DESC
             LIMIT $3
            "#,
        )
        .bind(workspace_id)
        .bind(&ts)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(doc_id, parent_id, title, snippet, rank)| SearchHit {
                doc_id,
                parent_id,
                title,
                snippet: snippet.unwrap_or_default(),
                rank,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::to_prefix_tsquery;

    #[test]
    fn sanitizes_special_chars() {
        assert_eq!(to_prefix_tsquery("foo & bar"), Some("foo:* & bar:*".into()));
        assert_eq!(
            to_prefix_tsquery("'; DROP TABLE"),
            Some("DROP:* & TABLE:*".into())
        );
    }

    #[test]
    fn drops_short_tokens() {
        assert_eq!(to_prefix_tsquery("a b cd"), Some("cd:*".into()));
    }

    #[test]
    fn empty_when_no_usable_tokens() {
        assert_eq!(to_prefix_tsquery(""), None);
        assert_eq!(to_prefix_tsquery("!@#$%"), None);
        assert_eq!(to_prefix_tsquery("a b c"), None);
    }

    #[test]
    fn caps_token_count() {
        let q = (0..20)
            .map(|i| format!("word{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        let out = to_prefix_tsquery(&q).unwrap();
        assert_eq!(out.matches(":*").count(), 8);
    }
}
