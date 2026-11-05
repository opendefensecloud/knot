-- history_keep_updates
-- Created 2026-11-04

-- Page history keeps every edit. From here on a doc_updates row is never
-- deleted or rewritten, except by a purge cascade (deleting the page or its
-- workspace) or inside a transaction that sets knot.history_rewrite = 'on'
-- (only the erase path, a later milestone, does). Snapshots stay a boot cache
-- that GC may thin; every snapshot that exists today is copied to
-- doc_history_baselines, which GC never touches. See
-- docs/superpowers/specs/2026-11-04-page-history-architecture.md.
--
-- sqlx runs this file in one transaction, as the Helm pre-upgrade Job, while
-- old pods still serve, so every lock below is held until COMMIT. The order
-- keeps the server usable meanwhile and cannot deadlock with the app:
--   1. Both tables are locked up front, documents then doc_updates (the
--      app's order: a duplicate inserts into documents, then doc_updates; a
--      purge deletes from documents and cascades into doc_updates), in a
--      retry loop that never waits on one table while holding the other.
--      Each attempt is a subtransaction: it waits at most 100 ms for SHARE
--      ROW EXCLUSIVE on documents, holding nothing else, then asks for
--      SHARE ROW EXCLUSIVE on doc_updates with NOWAIT. A failed attempt
--      rolls back and so releases documents; the loop sleeps 0.5 s and
--      tries again, and gives up after 45 s so the Job retries. Waiting for
--      doc_updates while holding documents deadlocked: a page write holding
--      FOR UPDATE on a page (archive, move, restore, purge) blocks an edit
--      save's FK check on that page, the edit save holds doc_updates, and
--      the page write's UPDATE or DELETE then waits on documents.
--      Each attempt pauses page writes (ROW EXCLUSIVE on documents) for at
--      most 100 ms. Readers of documents and row locks on it (FOR SHARE,
--      FOR UPDATE, FK checks: ROW SHARE) pass.
--   2. From then on every statement needs only locks this transaction
--      already holds, locks on tables it creates, or ACCESS SHARE on
--      doc_snapshots (the copy), which no app transaction conflicts with.
--      So this transaction never waits on the app again: a page write that
--      read or row-locked documents and then writes it, or an edit save,
--      waits for COMMIT, and nothing it holds is anything this needs.
--      What waits until COMMIT: edit saves and every page write that needs
--      ROW EXCLUSIVE on documents (creates, renames, moves, archives,
--      restores, duplicates, purges, template and restriction changes).
--   3. The trigger comes before the copy, so no delete can land between the
--      copy and COMMIT; that pause is the copy time.
--   4. doc_history_meta.keeps_since records clock_timestamp() right after
--      CREATE TRIGGER, the moment deletes became impossible: with SHARE ROW
--      EXCLUSIVE on doc_updates held, no DELETE on it can run until COMMIT,
--      and after COMMIT the trigger refuses one. A row that GC deleted was
--      deleted before this moment, and was created before it was deleted,
--      so every doc_updates row with created_at >= keeps_since still exists.
--      20261104120001_history_since turns it into documents.history_since
--      and drops the table.
--   5. The baselines table gets its foreign key after the copy, so the FK
--      check does not run row by row during it. Adding it takes SHARE ROW
--      EXCLUSIVE on documents, already held, and the check reads documents
--      without row locks.
SET LOCAL lock_timeout = '5s';

DO $$
DECLARE
  deadline timestamptz := clock_timestamp() + interval '45 seconds';
  attempts int := 0;
  missing  text;
BEGIN
  LOOP
    attempts := attempts + 1;
    BEGIN
      PERFORM set_config('lock_timeout', '100ms', true);
      missing := 'SHARE ROW EXCLUSIVE on documents (page writes held it)';
      LOCK TABLE documents IN SHARE ROW EXCLUSIVE MODE;
      missing := 'SHARE ROW EXCLUSIVE on doc_updates (edit saves held it)';
      LOCK TABLE doc_updates IN SHARE ROW EXCLUSIVE MODE NOWAIT;
      EXIT;
    EXCEPTION WHEN lock_not_available THEN
      -- The subtransaction rolled back: documents is released again.
      NULL;
    END;
    IF clock_timestamp() >= deadline THEN
      RAISE EXCEPTION 'history_keep_updates: could not get % within 45 s (% attempts); the migrate Job will retry',
        missing, attempts;
    END IF;
    PERFORM pg_sleep(0.5);
  END LOOP;
  PERFORM set_config('lock_timeout', '5s', true);
  RAISE NOTICE 'history_keep_updates: locked documents and doc_updates after % attempt(s)', attempts;
END;
$$;

-- Every FK cascade runs inside RI_FKey_cascade_del, so a cascaded delete
-- arrives with pg_trigger_depth() > 1; a top-level statement has depth 1.
CREATE FUNCTION doc_updates_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND pg_trigger_depth() > 1 THEN RETURN OLD; END IF;
  IF current_setting('knot.history_rewrite', true) = 'on' THEN RETURN COALESCE(NEW, OLD); END IF;
  RAISE EXCEPTION 'doc_updates is append-only (page history)' USING ERRCODE = 'KH001';
END;
$$;

CREATE TRIGGER doc_updates_append_only
  BEFORE DELETE OR UPDATE OF update_bytes, doc_id ON doc_updates
  FOR EACH ROW EXECUTE FUNCTION doc_updates_append_only();

-- See 4 above. One row; 20261104120001_history_since reads it and drops it.
CREATE TABLE doc_history_meta (
  id          boolean PRIMARY KEY DEFAULT true CHECK (id),
  keeps_since timestamptz NOT NULL
);
INSERT INTO doc_history_meta (keeps_since) VALUES (clock_timestamp());

CREATE TABLE doc_history_baselines (
  id           bigserial PRIMARY KEY,
  doc_id       uuid NOT NULL,
  snapshot_seq bigint NOT NULL,
  state_bytes  bytea NOT NULL,
  state_vector bytea NOT NULL,
  created_at   timestamptz NOT NULL,
  redacted_at  timestamptz NULL,
  UNIQUE (doc_id, snapshot_seq)
);
CREATE INDEX doc_history_baselines_doc_idx ON doc_history_baselines (doc_id, created_at DESC);

INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at)
SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at
FROM doc_snapshots;

ALTER TABLE doc_history_baselines
  ADD CONSTRAINT doc_history_baselines_doc_fk
  FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE;
