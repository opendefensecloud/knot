-- acl_invalidations_workspace_wide
-- Created 2026-10-30
--
-- A member's role change or removal changes their access in every doc of
-- the workspace at once. It is recorded as one invalidation that names no
-- doc (`doc_id IS NULL`), which every replica's ACL listener turns into a
-- full cache eviction and a re-check of every open socket.
ALTER TABLE acl_invalidations ALTER COLUMN doc_id DROP NOT NULL;
