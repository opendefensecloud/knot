-- history_since
-- Created 2026-11-04

-- documents.history_since: when page history started keeping every edit.
-- Pages with created_at < history_since predate history: their earliest
-- edits may already be gone. A page created later has created_at >= it, and
-- its history is complete. See
-- docs/superpowers/specs/2026-11-04-page-history-architecture.md.
--
-- Existing pages get doc_history_meta.keeps_since, the moment
-- 20261104120000_history_keep_updates made deletes impossible (see its
-- header, item 4), however long ago that committed. It is a constant
-- default, so the column is added without rewriting the table. Pages created
-- between the two migrations already exist here and get it too; their
-- created_at is later, so they count as new pages, which they are: the
-- trigger has kept all their edits. Then the default becomes now(), the
-- insert time of every later page, and the meta table goes.
--
-- A migration of its own: adding a column takes ACCESS EXCLUSIVE on
-- documents, and as the last statement of 20261104120000, which holds SHARE
-- ROW EXCLUSIVE on documents, that upgrade deadlocked with page writes that
-- had read or row-locked documents and then waited to write it. Here the
-- lock is taken in a retry loop, holding nothing the app needs while it
-- waits: each attempt is a subtransaction that waits at most 100 ms for the
-- lock, then adds the column. A failed attempt rolls back and releases what
-- it took; the loop sleeps 0.5 s and tries again, and gives up after 45 s so
-- the Job retries. While an attempt waits, all access to documents queues
-- behind it, page reads and edit saves (their FK checks) included, for at
-- most 100 ms. A long reader (pg_dump, a slow export) can outlast the loop;
-- 20261104120000 has committed by then, so re-run the upgrade once the
-- reader is gone (see the Helm chart README).
SET LOCAL lock_timeout = '5s';

DO $$
DECLARE
  deadline timestamptz := clock_timestamp() + interval '45 seconds';
  attempts int := 0;
  since    timestamptz;
BEGIN
  SELECT keeps_since INTO STRICT since FROM doc_history_meta;
  -- ISO output, so the literal below reads back as the same instant.
  PERFORM set_config('DateStyle', 'ISO, YMD', true);
  LOOP
    attempts := attempts + 1;
    BEGIN
      PERFORM set_config('lock_timeout', '100ms', true);
      LOCK TABLE documents IN ACCESS EXCLUSIVE MODE;
      EXECUTE format(
        'ALTER TABLE documents ADD COLUMN history_since timestamptz NOT NULL DEFAULT %L::timestamptz',
        since);
      EXIT;
    EXCEPTION WHEN lock_not_available THEN
      -- The subtransaction rolled back: documents is released again.
      NULL;
    END;
    IF clock_timestamp() >= deadline THEN
      RAISE EXCEPTION 'history_since: could not get ACCESS EXCLUSIVE on documents within 45 s (% attempts; a long transaction, such as pg_dump, holds it); the migrate Job will retry',
        attempts;
    END IF;
    PERFORM pg_sleep(0.5);
  END LOOP;
  PERFORM set_config('lock_timeout', '5s', true);
  RAISE NOTICE 'history_since: locked documents after % attempt(s)', attempts;
END;
$$;

-- documents is already locked ACCESS EXCLUSIVE; this changes only the
-- default for new rows, not the value existing rows read.
ALTER TABLE documents ALTER COLUMN history_since SET DEFAULT now();

DROP TABLE doc_history_meta;
