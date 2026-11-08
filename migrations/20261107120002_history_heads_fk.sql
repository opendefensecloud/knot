-- history_heads_fk
-- Created 2026-11-07

-- The second half of 20261107120001_history_heads: the heads table's foreign
-- key, and the snapshot trigger that checks a snapshot's content epoch and
-- keeps stamped snapshots immutable. See
-- docs/superpowers/specs/2026-11-04-page-history-architecture.md
-- (storage_model) and the M1.2 plan's Global Constraints and Rulings.
--
-- sqlx runs this file in one transaction, as the Helm pre-upgrade Job, while
-- 0.9.9 pods keep serving and writing.
--   1. knot_lock_tables (20261107120000) takes SHARE ROW EXCLUSIVE on
--      doc_history_heads, documents and doc_snapshots, in that order. It
--      waits up to 100 ms only for the first table and asks for the others
--      with NOWAIT, so the wait goes to doc_history_heads, the busy one:
--      every in-flight 0.9.9 save holds ROW EXCLUSIVE on it through
--      doc_updates_assign_rev's heads upsert. While the attempt waits, new
--      saves queue behind it and the in-flight ones finish, as
--      20261107120001's wait on doc_updates does; documents is contended
--      only by page writes, doc_snapshots by snapshot writes. The helper
--      never waits for a table while holding another, so this table order is
--      no deadlock risk, and the row-lock order the app takes (page row,
--      then heads row) is untouched.
--   2. Orphan heads rows go: pages purged since 20261107120001 seeded or
--      numbered them, while the table had no foreign key. With documents
--      locked, no page can be deleted between this delete and the FK check.
--   3. The FK, ON DELETE CASCADE: from here on a purge takes the page's
--      heads row with it. Adding it checks every heads row against
--      documents once, under the locks already held, with no row locks.
--   4. doc_snapshots_epoch, BEFORE INSERT OR UPDATE on doc_snapshots, in
--      order:
--      (o) rewriting: the M1.1 knot.history_rewrite flag, which only an
--          erase sets. It is read through coalesce(.., ''): a session that
--          never set the flag (every fresh pool connection) reads the
--          setting as NULL, NULL = 'on' is NULL, and a NULL rewriting would
--          make NOT rewriting NULL too, which is not true, so the
--          immutability guard and the epoch check would both be skipped.
--      (i) A stamped snapshot (snapshot_rev set) is immutable: an UPDATE of
--          one returns NULL, which skips it without an error. 0.9.9's
--          upsert sets only the bytes, the vector and created_at, so on a
--          snapshot_seq collision it would keep snapshot_rev = R over a
--          state that is not the log up to R (unflushed or dropped edits, a
--          late-seq row it skipped), and 0.9.10 boots would read only rows
--          after R from then on. Two 0.9.10 writers at one seq are at one
--          rev and hold the same state, so skipping theirs loses nothing. A
--          legacy snapshot (snapshot_rev NULL) still takes 0.9.9's upsert,
--          and a stamped upsert at its seq promotes it. The rewrite flag
--          skips this guard, since an erase rewrites snapshots.
--      (ii) The page row, FOR KEY SHARE, before the heads row, as every
--          M1.2 path takes them (so a purge makes the write wait instead of
--          deadlocking); no page raises 23503, the FK's SQLSTATE. This runs
--          with or without the rewrite flag; an upsert has already run it in
--          its BEFORE INSERT firing.
--      (iii) The page's content_epoch, FOR KEY SHARE on its heads row (no
--          row reads as 0). That conflicts only with an erase's FOR UPDATE,
--          not with a flush's FOR NO KEY UPDATE; a write that queued behind
--          an erase re-reads the row once the erase commits and sees its new
--          epoch. Unless rewriting, the snapshot is refused with KH002 when
--          it carries no epoch (0.9.9) and the page's epoch is above 0, or
--          carries one that differs. The rewrite flag skips the check: an
--          erase writes its fresh snapshot at the new epoch before it bumps
--          content_epoch.
-- What waits until COMMIT: page writes (creates, renames, moves, archives,
-- purges, duplicates), edit saves and snapshot writes. Every 0.9.9 edit save
-- goes through doc_updates_assign_rev, whose heads upsert needs ROW
-- EXCLUSIVE on doc_history_heads, so each waiting save holds a pool
-- connection, ROW EXCLUSIVE on doc_updates and a documents KEY SHARE, and on
-- 0.9.9 pods an open page awaiting a task toggle, accept, import or
-- snapshot is held up too. Nothing is counted here, so the pause is one pass
-- over the heads rows. Page loads go on.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('history_heads_fk',
                        ARRAY['doc_history_heads', 'documents', 'doc_snapshots']::regclass[],
                        'SHARE ROW EXCLUSIVE');

DELETE FROM doc_history_heads h
WHERE NOT EXISTS (SELECT 1 FROM documents d WHERE d.id = h.doc_id);

ALTER TABLE doc_history_heads
  ADD CONSTRAINT doc_history_heads_doc_fk
  FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE;

CREATE FUNCTION doc_snapshots_epoch() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  rewriting     boolean NOT NULL := coalesce(current_setting('knot.history_rewrite', true), '') = 'on';
  current_epoch int;
BEGIN
  IF TG_OP = 'UPDATE' AND OLD.snapshot_rev IS NOT NULL AND NOT rewriting THEN
    RETURN NULL;
  END IF;
  PERFORM 1 FROM documents WHERE id = NEW.doc_id FOR KEY SHARE;
  IF NOT FOUND THEN
    RAISE EXCEPTION 'page % does not exist', NEW.doc_id USING ERRCODE = '23503';
  END IF;
  SELECT content_epoch INTO current_epoch
    FROM doc_history_heads WHERE doc_id = NEW.doc_id FOR KEY SHARE;
  IF NOT FOUND THEN
    current_epoch := 0;
  END IF;
  IF NOT rewriting
     AND ((NEW.epoch IS NULL AND current_epoch > 0)
          OR (NEW.epoch IS NOT NULL AND NEW.epoch <> current_epoch)) THEN
    RAISE EXCEPTION 'page history epoch changed; reload the page' USING ERRCODE = 'KH002';
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER doc_snapshots_epoch
  BEFORE INSERT OR UPDATE ON doc_snapshots
  FOR EACH ROW EXECUTE FUNCTION doc_snapshots_epoch();
