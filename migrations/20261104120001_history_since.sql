-- history_since
-- Created 2026-11-04

-- documents.history_since: when page history started keeping every edit.
-- See docs/superpowers/specs/2026-11-04-page-history-architecture.md.
--
-- A migration of its own, so its own transaction: adding a column takes
-- ACCESS EXCLUSIVE on documents. In 20261104120000_history_keep_updates,
-- which already holds SHARE ROW EXCLUSIVE on documents, that upgrade
-- deadlocked with any page write that had read or row-locked documents
-- (a create's FOR SHARE on the parent, a purge's FOR UPDATE on the archive
-- root) and then waited to write it. Here the transaction holds no other
-- lock, so it can only wait, until the lock timeout fails it and the Job
-- retries. Page reads and writes queue behind it while it waits (at most
-- the lock timeout); once granted it commits at once, since a column with
-- a non-volatile default is added without rewriting the table.
--
-- It comes after the trigger migration, never before: old pods' GC still
-- deletes edits until the trigger commits, so a history_since stamped
-- earlier would claim history that is gone. Pages created between the two
-- commits get history_since > created_at and count as predating history.
-- That is conservative and harmless: the trigger already keeps their edits.
SET LOCAL lock_timeout = '5s';

-- Pages with created_at < history_since predate history: their earliest
-- edits may already be gone. A page created later has created_at >= it.
-- now() is this transaction's start time.
ALTER TABLE documents ADD COLUMN history_since timestamptz NOT NULL DEFAULT now();
