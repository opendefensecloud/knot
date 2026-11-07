-- history_rev_index_check
-- Created 2026-11-07

-- Makes sure 20261107120003's unique index doc_updates_doc_rev_uq exists and
-- is valid, and rebuilds it if not. See
-- docs/superpowers/specs/2026-11-04-page-history-architecture.md
-- (storage_model) and the M1.2 plan's Global Constraints and Rulings.
--
-- Why the index: rev numbers a page's rows in commit order. The heads row
-- hands out revs, and the index makes Postgres refuse a second row at a
-- page's rev (23505) and lets 0.9.10 read a page's rows by rev. It is
-- partial (WHERE rev IS NOT NULL): rows saved before 20261107120001 keep a
-- NULL rev until 0.9.10 first opens their page, so during the upgrade the
-- index holds only rows written since that migration.
--
-- Why CONCURRENTLY: a plain CREATE INDEX takes SHARE on doc_updates and
-- stops every save for the whole build; CREATE INDEX CONCURRENTLY (CIC)
-- takes SHARE UPDATE EXCLUSIVE, so page loads and saves go on while it
-- builds. CIC cannot run inside a transaction, so 20261107120003 is a
-- no-transaction migration of one statement, and this check, which can run
-- in a transaction, lives in its own file.
--
-- What CIC waits for: transactions older than itself. It waits for every
-- transaction that writes doc_updates when it starts, and at the end for
-- every transaction in this database whose snapshot is older than its own.
-- So it does not finish during a pg_dump (one long snapshot) and should not
-- run while a knot-server pod starts: every knot-server start runs the
-- migrations, and sqlx holds a session advisory lock for the whole run, so
-- a pod starting while the Job builds the index waits on that advisory
-- lock with a live snapshot, which the build waits on in turn. The deadlock
-- detector then aborts one of the two. If it aborts the pod's wait, the pod
-- restarts and the build finishes. If it aborts the build, or the build is
-- cancelled, the build leaves an invalid index behind, the migration is not
-- recorded, and the Job fails; on the re-run, 20261107120003's IF NOT
-- EXISTS sees the invalid index, skips it and is recorded as done.
-- A Job killed during the build usually does not stop it: the backend does
-- not notice its client is gone (client_connection_check_interval is 0 by
-- default), keeps building, keeps holding the advisory lock, and the index
-- becomes valid if the build finishes. The re-run Job's migrator waits on
-- that advisory lock meanwhile; if the detector aborts one side, the next
-- run converges. Operators check pg_stat_progress_create_index before
-- re-running the Job, and wait for the build or pg_terminate_backend it.
--
-- When this check does anything: only after an interrupted or aborted index
-- build left doc_updates_doc_rev_uq invalid, or after it was dropped by
-- hand. Then it takes ACCESS EXCLUSIVE on doc_updates through
-- knot_lock_tables (20261107120000), which tries for 100 ms at a time and
-- lets go in between, so a page write that holds a page row and then reads
-- doc_updates never queues behind it while a save waits on that page row
-- (the deadlock the helper exists for). Every session that holds a lock on
-- the index also holds one on doc_updates, so with doc_updates locked
-- nothing else waits outside the helper's loop. It drops the index and
-- builds it again without CONCURRENTLY. Page loads and saves both pause
-- until this transaction commits: one scan of doc_updates.
-- Otherwise it only reads the catalog and takes no lock on doc_updates.
SET LOCAL lock_timeout = '5s';

DO $$
BEGIN
  IF NOT EXISTS (
    SELECT 1
      FROM pg_index i
      JOIN pg_class c ON c.oid = i.indexrelid
     WHERE i.indrelid = 'doc_updates'::regclass
       AND c.relname = 'doc_updates_doc_rev_uq'
       AND i.indisvalid
  ) THEN
    PERFORM knot_lock_tables('history_rev_index_check',
                             ARRAY['doc_updates']::regclass[],
                             'ACCESS EXCLUSIVE');
    DROP INDEX IF EXISTS doc_updates_doc_rev_uq;
    CREATE UNIQUE INDEX doc_updates_doc_rev_uq ON doc_updates (doc_id, rev) WHERE rev IS NOT NULL;
    RAISE NOTICE 'history_rev_index_check: rebuilt doc_updates_doc_rev_uq';
  END IF;
END
$$;
