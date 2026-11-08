-- no-transaction
-- history_rev_index
-- Created 2026-11-07
-- The unique (doc_id, rev) index, built concurrently so page loads and saves go on.
-- sqlx sends this file as one simple query, so it must hold exactly one statement
-- (a second one would put the build in an implicit transaction block, which fails).
-- See 20261107120004_history_rev_index_check for why and what happens if this is interrupted.
CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS doc_updates_doc_rev_uq ON doc_updates (doc_id, rev) WHERE rev IS NOT NULL;
