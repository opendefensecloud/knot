-- quota_indexes (issue #88)
--
-- The optional page and file-byte quotas count, before a write, the pages a
-- user created and the bytes of the files they uploaded
-- (knot_storage::quota). Neither column had an index, so each count would
-- scan the workspace. The byte sum is index-only through INCLUDE: blob rows
-- are never updated.
--
-- CREATE INDEX takes SHARE on its table. Take both up front in bounded
-- attempts (knot_lock_tables, 20261107120000), which never waits on one
-- table while holding the other, so a long transaction on either makes this
-- migration give up instead of queueing every page write behind it. Both
-- tables are small (one row per page, one per file).
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('quota_indexes',
                        ARRAY['documents', 'blobs']::regclass[],
                        'SHARE');

CREATE INDEX IF NOT EXISTS documents_created_by_idx
    ON documents (workspace_id, created_by);
CREATE INDEX IF NOT EXISTS blobs_created_by_bytes_idx
    ON blobs (workspace_id, created_by) INCLUDE (byte_size);
