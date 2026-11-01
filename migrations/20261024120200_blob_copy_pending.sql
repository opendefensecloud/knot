-- blob_copy_pending
-- Created 2026-10-24
--
-- Objects Duplicate copies in a blob store outside the database (S3), so that
-- none outlives a duplicate that never committed.
--
-- The copies are made before the duplicate's transaction opens. Each copy's
-- id is recorded here, and committed, before any copy starts, so a copy
-- whose response was lost is covered too. The transaction that inserts the
-- copies' `blobs` rows deletes their entries.
--
-- Nothing is deleted on the request path: a commit whose acknowledgement was
-- lost must keep its files. The hourly purge job takes the entries older than
-- an hour instead: with no `blobs` row it deletes the object, then the entry;
-- otherwise just the entry.
CREATE TABLE blob_copy_pending (
    blob_id    uuid PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT now()
);
