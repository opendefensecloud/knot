-- acl_invalidations_index
-- Created 2026-06-19

-- The ACL-invalidation listener consumes the outbox with
--   DELETE FROM acl_invalidations WHERE doc_id = $1
-- after processing each NOTIFY. Without an index that is a sequential scan over
-- the whole table on every eviction; index doc_id so the outbox stays cheap as
-- it churns.
CREATE INDEX IF NOT EXISTS acl_invalidations_doc_id_idx
    ON acl_invalidations (doc_id);
