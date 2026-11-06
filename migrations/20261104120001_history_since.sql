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
-- between the two migrations already exist here and get it too. Almost all
-- of them have a later created_at, so they count as new pages, which they
-- are: the trigger has kept all their edits. created_at is the creating
-- transaction's start (DEFAULT now()), so a create already in progress when
-- 20261104120000 locked documents has an earlier one and counts as
-- predating history, which errs on the safe side. Then the default becomes
-- now() for every later page, and the meta table goes.
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
-- most 100 ms. When an autovacuum worker holds documents, the loop waits
-- for the lock alone for deadlock_timeout + 500 ms, at most every 5 s per
-- worker, so the deadlock check cancels it; all access to documents queues
-- meanwhile. Detection, the anti-wraparound skip and the snapshot refresh
-- are as in 20261104120000, item 1. The meta table's ACCESS SHARE is the
-- only other lock held, and no app touches that table. A long reader
-- (pg_dump, a slow export) or an anti-wraparound autovacuum can outlast the
-- loop; 20261104120000 has committed by then, so re-run the upgrade once it
-- is gone (see the Helm chart README). The error names up to three sessions
-- holding a lock on documents, oldest transaction first.
SET LOCAL lock_timeout = '5s';

DO $$
DECLARE
  deadline  timestamptz := clock_timestamp() + interval '45 seconds';
  lone_wait text := (SELECT (setting::int + 500) || 'ms' FROM pg_settings WHERE name = 'deadlock_timeout');
  attempts  int := 0;
  lone      int := 0;
  since     timestamptz;
  vacuum    int;
  waited    int[] := '{}';          -- pid of each lone wait ...
  waited_at timestamptz[] := '{}';  -- ... and when it started
  holders   text;
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
    -- An autovacuum worker on documents, as in 20261104120000.
    PERFORM pg_stat_clear_snapshot();
    SELECT l.pid INTO vacuum
      FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
     WHERE l.locktype = 'relation' AND l.relation = 'documents'::regclass AND l.granted
       AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
       AND (a.backend_type = 'autovacuum worker' OR (a.backend_type IS NULL AND a.usesysid IS NULL))
       AND coalesce(a.query, '') NOT LIKE '%to prevent wraparound%'
       AND NOT EXISTS (SELECT 1 FROM unnest(waited, waited_at) AS w(p, at)
                        WHERE w.p = l.pid AND w.at > clock_timestamp() - interval '5 seconds')
     LIMIT 1;
    IF vacuum IS NOT NULL AND clock_timestamp() < deadline THEN
      waited := waited || vacuum;
      waited_at := waited_at || clock_timestamp();
      lone := lone + 1;
      BEGIN
        PERFORM set_config('lock_timeout', lone_wait, true);
        LOCK TABLE documents IN ACCESS EXCLUSIVE MODE;
        RAISE EXCEPTION USING ERRCODE = 'KHLW1';
      EXCEPTION WHEN SQLSTATE 'KHLW1' OR lock_not_available THEN
        NULL;
      END;
      CONTINUE;
    END IF;
    IF clock_timestamp() >= deadline THEN
      -- Up to three holders of any lock on documents (all conflict with
      -- ACCESS EXCLUSIVE), oldest transaction first.
      PERFORM pg_stat_clear_snapshot();
      SELECT string_agg(
               CASE WHEN x.pid IS NULL THEN 'a prepared transaction'
                    ELSE format('pid %s (%s%s)', x.pid,
                      CASE WHEN x.apid IS NULL THEN 'a session that has ended'
                           WHEN x.backend_type IS NOT NULL THEN x.backend_type
                           WHEN x.usesysid IS NULL THEN 'autovacuum or another background process'
                           ELSE 'a session of role ' || x.usename END,
                      CASE WHEN x.query IS NULL OR x.query = '<insufficient privilege>' THEN ''
                           ELSE ', ' || coalesce(x.state, '?') || ': ' || left(regexp_replace(x.query, '\s+', ' ', 'g'), 100) END)
               END, '; ' ORDER BY x.since NULLS FIRST)
        INTO holders
        FROM (SELECT h.pid, a.pid AS apid, a.backend_type, a.usesysid, a.usename, a.state, a.query,
                     coalesce(a.xact_start, a.backend_start) AS since
                FROM (SELECT DISTINCT pid, virtualtransaction FROM pg_locks
                       WHERE locktype = 'relation' AND relation = 'documents'::regclass AND granted
                         AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                         AND pid IS DISTINCT FROM pg_backend_pid()) h
                LEFT JOIN pg_stat_activity a ON a.pid = h.pid
               ORDER BY since NULLS FIRST
               LIMIT 3) x;
      RAISE EXCEPTION 'history_since: could not get ACCESS EXCLUSIVE on documents within 45 s (% attempts, % lone waits); held by %. The migrate Job will retry.',
        attempts, lone, coalesce(holders, 'no one any more');
    END IF;
    PERFORM pg_sleep(0.5);
  END LOOP;
  PERFORM set_config('lock_timeout', '5s', true);
  RAISE NOTICE 'history_since: locked documents after % attempt(s), % lone wait(s)', attempts, lone;
END;
$$;

-- documents is already locked ACCESS EXCLUSIVE; this changes only the
-- default for new rows, not the value existing rows read.
ALTER TABLE documents ALTER COLUMN history_since SET DEFAULT now();

DROP TABLE doc_history_meta;
