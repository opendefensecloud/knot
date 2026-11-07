-- history_heads
-- Created 2026-11-07

-- One doc_history_heads row per page hands out that page's revs. Every write
-- of doc_updates rows locks the page's heads row and takes the next revs
-- from it, so a writer gets its revs only once the previous writer's
-- transaction has ended: the row lock is the order, and rev order is commit
-- order. A writer that rolls back rolls its increment back too, so revs stay
-- gapless. See docs/superpowers/specs/2026-11-04-page-history-architecture.md
-- (storage_model) and the M1.2 plan's Global Constraints.
--   head_rev       the highest rev handed out for the page;
--   legacy_rows    how many of its rows this migration found (rev NULL until
--                  0.9.10 numbers them 1..legacy_rows, by seq, when a room
--                  first boots the page);
--   content_epoch  bumped by a later erase; a writer that saw another epoch
--                  is refused with KH002;
--   dirty, last_write_at
--                  for M1.3's entry builder: pages with rows it has not read
--                  yet, oldest write first.
--
-- 0.9.10 writes revs itself. doc_updates_assign_rev numbers every row that
-- arrives without one: 0.9.9 pods during the rolling upgrade, and raw
-- INSERTs. For such a row it
--   * locks the page row (FOR KEY SHARE) before the heads row. Every M1.2
--     path takes the page row first (the page row's KEY SHARE is the first
--     lock of any transaction that locks a page-scoped history row). A purge
--     holds the page row FOR UPDATE and then cascades into the page's
--     doc_updates and heads rows; a writer that held the heads row and only
--     then waited for the page row (in the FK check) would close a cycle
--     with it. Taking the page row first, the writer waits for the purge,
--     finds no page, and fails with 23503, the SQLSTATE the FK raised before;
--   * takes the next rev from the heads row, creating the row at rev 1 for a
--     page that has none yet (a page created after the seed below);
--   * refuses the row with KH002 once the page's content_epoch is above 0:
--     0.9.9 carries no epoch;
--   * replaces created_at with clock_timestamp() when it is the column
--     default (now(), the transaction start), so created_at follows rev
--     order; explicit timestamps from tests and tools stay.
-- A row that carries its rev (0.9.10) is only checked: its epoch must equal
-- the content_epoch of the heads row its flush locked one statement before.
-- That arm takes no page lock: the flush's first statement already holds the
-- page row's KEY SHARE, and the FK check still raises 23503 for a missing
-- page, so a second lock would only cost every row of a 200-row flush on the
-- typing path.
--
-- sqlx runs this file in one transaction, as the Helm pre-upgrade Job, while
-- 0.9.9 pods keep serving and writing. The order:
--   1. knot_lock_tables (20261107120000) takes SHARE ROW EXCLUSIVE on
--      doc_updates, waiting at most 100 ms per attempt and holding nothing
--      else meanwhile. Once it holds the lock, every transaction that wrote
--      doc_updates has ended, and none can write it again until COMMIT.
--      Readers pass. Nothing below waits for a lock the app holds: the heads
--      table is new, CREATE TRIGGER needs the lock already held, and the
--      seed reads documents with ACCESS SHARE and no row locks.
--   2. The table, then the trigger, then the seed. With the trigger created
--      under the lock, no row can slip in between the seed's count and the
--      trigger: every row the seed counts is legacy (rev NULL), and every row
--      inserted after COMMIT, including a 0.9.9 save that queued on the lock,
--      goes through the trigger (a statement that waited for a lock replans
--      once it has it, and sees the new trigger).
--   3. The seed reads documents, not just doc_updates: every page gets a
--      heads row, pages whose rows the pre-M1.1 GC pruned included. M1.3's
--      builder finds its work only through doc_history_heads WHERE dirty,
--      and writes a page's baseline entries on its first visit. Seeded rows
--      are dirty with last_write_at = now() (this transaction's start), never
--      NULL, so they sort before every page written after the upgrade; not
--      max(created_at), which would read every row from the heap while saves
--      wait. A page created after the seed's snapshot gets its heads row with
--      its first row; a page purged meanwhile leaves an orphan heads row,
--      which 20261107120002 deletes.
--   4. No foreign key here: checking one per seeded row would take FOR KEY
--      SHARE on documents rows while holding doc_updates, which deadlocks
--      with a purge that holds a page row and waits for doc_updates in its
--      cascade. No snapshot trigger either: it would need doc_snapshots
--      locked for the whole seed, and 0.9.9 rooms await snapshot writes
--      inside their actor loop, so open pages would freeze. Both come in
--      20261107120002, which pauses writes for one pass over the heads rows.
--   5. The append-only trigger is re-created to fire on UPDATE of any column
--      (M1.1's fired only for update_bytes and doc_id, so by_user_id and
--      created_at could be rewritten) and to let exactly one update through:
--      numbering a legacy row, rev NULL to a value with every other column
--      unchanged. The columns are compared by name, not through to_jsonb,
--      which would hex-encode every row's update_bytes twice as the rows are
--      numbered; a test pins the column list, so a new column forces this
--      rule to be revisited.
-- What waits until COMMIT: edit saves, and page writes that insert updates
-- in the same transaction (duplicates). The pause is the seed: one pass over
-- documents and doc_updates, sized by pg_relation_size('doc_updates') (the
-- count may read the heap, see the upgrade notes). Page loads, snapshot
-- writes and other page writes go on; 0.9.9 rooms block only where they
-- await a save inline (task toggles, accepts, imports, restores).
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('history_heads', ARRAY['doc_updates']::regclass[], 'SHARE ROW EXCLUSIVE');

CREATE TABLE doc_history_heads (
  doc_id        uuid PRIMARY KEY,
  head_rev      bigint NOT NULL DEFAULT 0,
  legacy_rows   bigint NOT NULL DEFAULT 0,
  content_epoch int NOT NULL DEFAULT 0,
  dirty         bool NOT NULL DEFAULT true,
  last_write_at timestamptz NULL
);
CREATE INDEX doc_history_heads_dirty_idx ON doc_history_heads (last_write_at) WHERE dirty;

CREATE FUNCTION doc_updates_assign_rev() RETURNS trigger
LANGUAGE plpgsql AS $$
DECLARE
  next_rev      bigint;
  current_epoch int;
BEGIN
  IF NEW.rev IS NULL THEN
    -- The page row first, then the heads row (see the header).
    PERFORM 1 FROM documents WHERE id = NEW.doc_id FOR KEY SHARE;
    IF NOT FOUND THEN
      RAISE EXCEPTION 'page % does not exist', NEW.doc_id USING ERRCODE = '23503';
    END IF;
    INSERT INTO doc_history_heads AS h (doc_id, head_rev, dirty, last_write_at)
    VALUES (NEW.doc_id, 1, true, clock_timestamp())
    ON CONFLICT (doc_id) DO UPDATE
      SET head_rev = h.head_rev + 1, dirty = true, last_write_at = clock_timestamp()
    RETURNING h.head_rev, h.content_epoch INTO next_rev, current_epoch;
    IF current_epoch > 0 THEN
      RAISE EXCEPTION 'page history epoch changed; reload the page' USING ERRCODE = 'KH002';
    END IF;
    NEW.rev := next_rev;
    IF NEW.created_at = now() THEN
      NEW.created_at := clock_timestamp();
    END IF;
    RETURN NEW;
  END IF;
  -- 0.9.10: the heads row this transaction locked in its flush.
  SELECT content_epoch INTO current_epoch FROM doc_history_heads WHERE doc_id = NEW.doc_id;
  IF NOT FOUND OR NEW.epoch IS DISTINCT FROM current_epoch THEN
    RAISE EXCEPTION 'page history epoch changed; reload the page' USING ERRCODE = 'KH002';
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER doc_updates_assign_rev
  BEFORE INSERT ON doc_updates
  FOR EACH ROW EXECUTE FUNCTION doc_updates_assign_rev();

-- See 2 and 3 above.
INSERT INTO doc_history_heads (doc_id, head_rev, legacy_rows, last_write_at)
SELECT d.id, count(u.seq), count(u.seq), now()
FROM documents d LEFT JOIN doc_updates u ON u.doc_id = d.id
GROUP BY d.id;

-- See 5 above. The purge cascade arm and the knot.history_rewrite arm are
-- 20261104120000's.
CREATE OR REPLACE FUNCTION doc_updates_append_only() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'DELETE' AND pg_trigger_depth() > 1 THEN RETURN OLD; END IF;
  IF TG_OP = 'UPDATE' AND OLD.rev IS NULL AND NEW.rev IS NOT NULL
     AND NEW.seq = OLD.seq
     AND NEW.doc_id = OLD.doc_id
     AND NEW.update_bytes = OLD.update_bytes
     AND NEW.by_user_id IS NOT DISTINCT FROM OLD.by_user_id
     AND NEW.created_at = OLD.created_at
     AND NEW.source IS NOT DISTINCT FROM OLD.source
     AND NEW.meta IS NOT DISTINCT FROM OLD.meta
     AND NEW.epoch IS NOT DISTINCT FROM OLD.epoch THEN
    RETURN NEW;
  END IF;
  IF current_setting('knot.history_rewrite', true) = 'on' THEN RETURN COALESCE(NEW, OLD); END IF;
  RAISE EXCEPTION 'doc_updates is append-only (page history)' USING ERRCODE = 'KH001';
END;
$$;

CREATE OR REPLACE TRIGGER doc_updates_append_only
  BEFORE DELETE OR UPDATE ON doc_updates
  FOR EACH ROW EXECUTE FUNCTION doc_updates_append_only();
