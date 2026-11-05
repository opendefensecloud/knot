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
-- keeps the server usable meanwhile:
--   1. lock_timeout: a blocked lock request fails and the Job retries,
--      instead of queueing the whole server behind it.
--   2. The trigger comes before the copy, so no delete can land between the
--      copy and COMMIT. It blocks doc_updates inserts (edit saves) until
--      COMMIT; that pause is the copy time.
--   3. The baselines table is created without its foreign key, so documents
--      stays writable during the copy; the FK is added after.
--   4. documents.history_since takes ACCESS EXCLUSIVE on documents, so it
--      goes last. now() is the transaction start time wherever it sits.
SET LOCAL lock_timeout = '5s';

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

-- Pages with created_at < history_since predate history: their earliest
-- edits may already be gone. A page created later has created_at >= it.
ALTER TABLE documents ADD COLUMN history_since timestamptz NOT NULL DEFAULT now();
