-- Document archive with retention (#37).
-- See docs/superpowers/specs/2026-10-13-document-archive-retention-design.md.
--
-- Deleting a doc archives it together with its live subtree. Every row of
-- that set carries the doc the user deleted in `archive_root_id` (equal to
-- `id` on that doc), so restore brings the same set back and purge deletes
-- the root row and lets the parent_id cascade take the rest.
ALTER TABLE documents
    ADD COLUMN archived_by        uuid NULL REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN archive_root_id    uuid NULL,
    -- Set on an archive root (and the roots inside it) once a purge has
    -- started deleting blob bytes outside the database: restore is refused
    -- from then on, and the purge job retries it until it completes.
    ADD COLUMN purge_requested_at timestamptz NULL;

-- Docs soft-deleted before this release become archive roots with a fresh
-- retention clock, so nothing is purged the moment the new version starts.
-- The original deletion time stays in audit_events (doc.archive).
UPDATE documents
   SET archive_root_id = id, archived_at = now()
 WHERE archived_at IS NOT NULL;

-- Their live children are what the sidebar shows at the top level today
-- (buildTree promotes docs whose parent is missing). Detach them for real,
-- or purging the archived parent would cascade-delete pages still in use.
-- UNIQUE (workspace_id, parent_id, sort_key) treats NULL parents as
-- distinct, so keeping their sort_key cannot conflict.
UPDATE documents c
   SET parent_id = NULL
  FROM documents p
 WHERE c.parent_id = p.id
   AND c.archived_at IS NULL
   AND p.archived_at IS NOT NULL;

ALTER TABLE documents
    ADD CONSTRAINT documents_archive_consistent
    CHECK ((archived_at IS NULL) = (archive_root_id IS NULL));

-- Archive list (per workspace, newest first) and the purge scan (oldest
-- first across workspaces) both read archive roots only.
CREATE INDEX documents_archive_roots_idx
    ON documents (workspace_id, archived_at)
 WHERE archive_root_id = id;
