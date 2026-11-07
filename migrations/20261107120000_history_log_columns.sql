-- history_log_columns
-- Created 2026-11-07

-- Page history M1.2 labels and numbers the log. Every doc_updates row gets
--   rev     its page's gapless revision, in commit order (filled from
--           20261107120001_history_heads on; NULL on rows written before),
--   source  what made it: edit, task, suggestion, restore, restore_fallback,
--           import, template or copy (NULL on rows of 0.9.9 and earlier),
--   meta    ids, timestamps and page titles that say more about the source
--           (never page text, never person names),
--   epoch   the content epoch its writer saw, which a later erase bumps;
-- and every doc_snapshots row gets snapshot_rev (the rev it holds the log up
-- to, NULL on a legacy snapshot) and epoch. See
-- docs/superpowers/specs/2026-11-04-page-history-architecture.md
-- (storage_model, attribution -> TRUSTWORTHY LOG).
--
-- sqlx runs this file in one transaction, as the Helm pre-upgrade Job, while
-- 0.9.9 pods keep serving and writing. Adding a column needs ACCESS
-- EXCLUSIVE, which blocks every read of the table too (page loads read both
-- doc_snapshots and doc_updates), so this migration does nothing but catalog
-- changes under it: nullable columns without a default and a NOT VALID
-- check are metadata only, with no scan and no rewrite. The heads table, its
-- seed and the rev trigger come in 20261107120001 under a weaker lock. The
-- columns are NULL on every existing row and 0.9.9's statements never name
-- them, so its inserts, reads and snapshot upserts keep working.
--
-- knot_lock_tables(who, tables, lock_mode, give_up_after) is the lock loop of
-- 20261104120000_history_keep_updates, factored out for this and the later
-- M1.2 migrations (that file stays as shipped). It takes lock_mode on every
-- table in `tables`, in order, and holds the locks until the caller's
-- transaction ends; call it as
--   SET LOCAL lock_timeout = '5s'; SELECT knot_lock_tables(...);
-- Its contract:
--   * It never waits for one table while holding another, so it cannot
--     deadlock with the app whatever order the app takes locks in. Each
--     attempt is a subtransaction that waits at most 100 ms for tables[1]
--     and asks for the rest with NOWAIT; a failed attempt rolls back,
--     releasing everything it got, sleeps 0.5 s and tries again. Put the
--     busiest table first, so new lock requests queue behind the attempt
--     and its in-flight holders can finish.
--   * lock_mode is one of SHARE, SHARE ROW EXCLUSIVE, ACCESS EXCLUSIVE (it
--     is spliced into the LOCK statement); anything else raises 22023.
--   * Postgres cancels an autovacuum worker in the way only from a waiter's
--     deadlock check, deadlock_timeout into a wait, which 100 ms waits never
--     reach. When an attempt fails on a table such a worker holds (not
--     visibly anti-wraparound, which Postgres never cancels; not waited on
--     for that table in the last 5 s), the loop waits for that table alone,
--     holding nothing else, for deadlock_timeout + 500 ms, then rolls back.
--     This lone wait asks for SHARE UPDATE EXCLUSIVE whatever lock_mode is:
--     that mode conflicts with autovacuum's own lock, so the waiter's
--     deadlock check still cancels the worker, and it conflicts with no
--     reader or writer of the table. Waiting in the target mode, as the M1.1
--     loop did, would stop every page load for about 1.5 s per lone wait
--     under ACCESS EXCLUSIVE. A worker is backend_type 'autovacuum worker'
--     or, without pg_read_all_stats (which hides other roles'
--     backend_type), any lock holder with no role, i.e. a background
--     process. Every read of pg_stat_activity is preceded by
--     pg_stat_clear_snapshot(): it is otherwise frozen for the transaction.
--   * After give_up_after (45 s by default) it raises, naming up to three
--     sessions that hold a lock conflicting with lock_mode on the table it
--     could not get, oldest transaction first, so the Job fails and retries.
--   * On success it sets lock_timeout to 5 s for the rest of the
--     transaction and raises a NOTICE with the attempt counts.
CREATE OR REPLACE FUNCTION knot_lock_tables(
  who           text,
  tables        regclass[],
  lock_mode     text,
  give_up_after interval DEFAULT '45 seconds'
) RETURNS void
LANGUAGE plpgsql AS $$
DECLARE
  deadline    timestamptz := clock_timestamp() + give_up_after;
  lone_wait   text := (SELECT (setting::int + 500) || 'ms' FROM pg_settings WHERE name = 'deadlock_timeout');
  attempts    int := 0;
  lone        int := 0;
  got         boolean := false;
  missing     regclass;
  vacuum      int;
  waited      text[] := '{}';         -- 'pid:relation' of each lone wait ...
  waited_at   timestamptz[] := '{}';  -- ... and when it started
  conflicting text[];                 -- granted modes that keep lock_mode out
  holders     text;
BEGIN
  IF lock_mode IS NULL OR lock_mode NOT IN ('SHARE', 'SHARE ROW EXCLUSIVE', 'ACCESS EXCLUSIVE') THEN
    RAISE EXCEPTION '%: unsupported lock mode %', who, coalesce(quote_literal(lock_mode), 'NULL')
      USING ERRCODE = 'invalid_parameter_value';
  END IF;
  IF coalesce(cardinality(tables), 0) = 0 THEN
    RAISE EXCEPTION '%: no tables to lock', who USING ERRCODE = 'invalid_parameter_value';
  END IF;
  conflicting := CASE lock_mode
    WHEN 'SHARE' THEN ARRAY['RowExclusiveLock', 'ShareUpdateExclusiveLock',
                            'ShareRowExclusiveLock', 'ExclusiveLock', 'AccessExclusiveLock']
    WHEN 'SHARE ROW EXCLUSIVE' THEN ARRAY['RowExclusiveLock', 'ShareUpdateExclusiveLock', 'ShareLock',
                                          'ShareRowExclusiveLock', 'ExclusiveLock', 'AccessExclusiveLock']
    ELSE ARRAY['AccessShareLock', 'RowShareLock', 'RowExclusiveLock', 'ShareUpdateExclusiveLock',
               'ShareLock', 'ShareRowExclusiveLock', 'ExclusiveLock', 'AccessExclusiveLock']
  END;
  LOOP
    attempts := attempts + 1;
    BEGIN
      PERFORM set_config('lock_timeout', '100ms', true);
      FOR i IN 1 .. cardinality(tables) LOOP
        missing := tables[i];
        EXECUTE format('LOCK TABLE %s IN %s MODE%s', missing, lock_mode, CASE WHEN i = 1 THEN '' ELSE ' NOWAIT' END);
      END LOOP;
      got := true;
    EXCEPTION WHEN lock_not_available THEN
      -- The subtransaction rolled back: every table it got is released.
      NULL;
    END;
    EXIT WHEN got;
    -- An autovacuum worker on the table this attempt didn't get, not
    -- visibly anti-wraparound, not waited on for this table in the last 5 s.
    PERFORM pg_stat_clear_snapshot();
    SELECT l.pid INTO vacuum
      FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid
     WHERE l.locktype = 'relation' AND l.relation = missing AND l.granted
       AND l.database = (SELECT oid FROM pg_database WHERE datname = current_database())
       AND (a.backend_type = 'autovacuum worker' OR (a.backend_type IS NULL AND a.usesysid IS NULL))
       AND coalesce(a.query, '') NOT LIKE '%to prevent wraparound%'
       AND NOT EXISTS (SELECT 1 FROM unnest(waited, waited_at) AS w(k, at)
                        WHERE w.k = l.pid || ':' || l.relation
                          AND w.at > clock_timestamp() - interval '5 seconds')
     LIMIT 1;
    IF vacuum IS NOT NULL AND clock_timestamp() < deadline THEN
      -- The lone wait: this table alone, in a mode that conflicts with the
      -- worker and with no reader or writer, long enough for the deadlock
      -- check to cancel the worker, then released by rolling back.
      waited := waited || (vacuum || ':' || missing::oid);
      waited_at := waited_at || clock_timestamp();
      lone := lone + 1;
      BEGIN
        PERFORM set_config('lock_timeout', lone_wait, true);
        EXECUTE format('LOCK TABLE %s IN SHARE UPDATE EXCLUSIVE MODE', missing);
        RAISE EXCEPTION USING ERRCODE = 'KHLW1';
      EXCEPTION WHEN SQLSTATE 'KHLW1' OR lock_not_available THEN
        NULL;
      END;
      CONTINUE;
    END IF;
    IF clock_timestamp() >= deadline THEN
      -- Up to three holders of a lock that conflicts with lock_mode, oldest
      -- transaction first.
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
                       WHERE locktype = 'relation' AND relation = missing AND granted
                         AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
                         AND pid IS DISTINCT FROM pg_backend_pid()
                         AND mode = ANY (conflicting)) h
                LEFT JOIN pg_stat_activity a ON a.pid = h.pid
               ORDER BY since NULLS FIRST
               LIMIT 3) x;
      RAISE EXCEPTION '%: could not get % on % within % s (% attempts, % lone waits); held by %. The migrate Job will retry.',
        who, lock_mode, missing, trim_scale(extract(epoch FROM give_up_after)), attempts, lone,
        coalesce(holders, 'no one any more');
    END IF;
    PERFORM pg_sleep(0.5);
  END LOOP;
  PERFORM set_config('lock_timeout', '5s', true);
  RAISE NOTICE '%: locked % after % attempt(s), % lone wait(s)',
    who, array_to_string(tables::text[], ', '), attempts, lone;
END;
$$;

SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('history_log_columns', ARRAY['doc_updates', 'doc_snapshots']::regclass[], 'ACCESS EXCLUSIVE');

ALTER TABLE doc_updates
  ADD COLUMN rev    bigint NULL,
  ADD COLUMN source text   NULL,
  ADD COLUMN meta   jsonb  NULL,
  ADD COLUMN epoch  int    NULL,
  ADD CONSTRAINT doc_updates_source_check
    CHECK (source IN ('edit', 'task', 'suggestion', 'restore', 'restore_fallback', 'import', 'template', 'copy'))
    NOT VALID;

ALTER TABLE doc_snapshots
  ADD COLUMN snapshot_rev bigint NULL,
  ADD COLUMN epoch        int    NULL;
