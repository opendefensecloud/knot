-- doc_contributors
-- Created 2026-10-03
--
-- Who has changed a page's CONTENT, for the doc byline ("created by …,
-- edited by …"). Every content change — live typing over the collab socket,
-- markdown import, history restore, task checkbox toggles, create-from-
-- template, workspace import — is persisted as a `doc_updates` row by the
-- room's writer, and the writer upserts this table in the same statement.
-- Renames, moves, permission changes and comments never touch doc_updates
-- and so never make anyone a contributor.
--
-- One row per (doc, user) instead of deriving the list from doc_updates:
-- snapshot GC deletes old doc_updates rows, so the log alone forgets early
-- contributors. last_edited_at is refreshed at most once per minute (see
-- PgUpdatesStore::insert_batch) so continuous typing does not rewrite the row
-- on every 250 ms writer flush.
--
-- No ON DELETE CASCADE on user_id, matching doc_updates.by_user_id: a
-- removed workspace member keeps their user row and their credit.
CREATE TABLE doc_contributors (
    doc_id          uuid        NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    user_id         uuid        NOT NULL REFERENCES users(id),
    first_edited_at timestamptz NOT NULL DEFAULT now(),
    last_edited_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (doc_id, user_id)
);

-- The point from which the contributor list is complete. Before this
-- migration live WS edits were stored without an author, so for existing docs
-- the list only knows about attributed changes (imports, restores, task
-- toggles …) — the UI shows "since <contributors_since>" for them. now() is
-- evaluated once when the column is added, so every existing row gets the
-- migration time; every doc created afterwards gets its own creation time.
ALTER TABLE documents
    ADD COLUMN contributors_since timestamptz NOT NULL DEFAULT now();

-- Backfill from whatever attribution doc_updates still holds (rows that
-- survived snapshot GC and carry an author).
INSERT INTO doc_contributors (doc_id, user_id, first_edited_at, last_edited_at)
SELECT doc_id, by_user_id, min(created_at), max(created_at)
FROM doc_updates
WHERE by_user_id IS NOT NULL
GROUP BY doc_id, by_user_id;
