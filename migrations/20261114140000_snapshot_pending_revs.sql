-- Room snapshots name the log rows that hold their pending content.
--
-- yrs 0.27.3 encodes a doc's pending content at shifted clocks once its
-- pending update holds overlapping Skips, so a room no longer writes pending
-- content into a snapshot's bytes. It records instead the revs of the log
-- rows whose original bytes re-park that content when boot applies them after
-- the snapshot. NULL marks a snapshot written before this migration, or by an
-- older release while the upgrade rolls out (an older pod's stamped NULL row
-- with created_at at or after this migration's installed_on, which the store
-- reads as DocSnapshot.older_writer).
--
-- Forward-only. sqlx runs this file in one transaction, as the Helm
-- pre-upgrade Job, while the previous release keeps serving. ADD COLUMN needs
-- ACCESS EXCLUSIVE on doc_snapshots, which stops snapshot writes, reads and GC
-- until COMMIT, so this file does nothing else and nothing slow: a nullable
-- column with no DEFAULT is a catalog change only, with no scan and no
-- rewrite. The previous release names its columns in every statement, so its
-- statements keep working. The doc_snapshots_epoch trigger is unchanged: a
-- stamped row stays immutable, pending_revs included.
--
-- knot_lock_tables (20261107120000) takes the lock in bounded attempts, up
-- front, and never waits for one table while holding another; if a long
-- transaction on doc_snapshots keeps it from the lock it gives up after 45 s,
-- naming the holder, and the Job retries.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261114140000_snapshot_pending_revs', ARRAY['doc_snapshots']::regclass[], 'ACCESS EXCLUSIVE');

ALTER TABLE doc_snapshots ADD COLUMN pending_revs bigint[] NULL;

COMMENT ON COLUMN doc_snapshots.pending_revs IS
  'NULL: the snapshot was written before this column existed, or by an older release during the upgrade, and its bytes may hold pending content in yrs''s shifted encoding. '
  'Empty: nothing was pending when the snapshot was taken. '
  'Non-empty: the ascending revs, each at or below snapshot_rev, of the log rows whose original bytes, re-applied after the snapshot''s bytes, re-park what the room held pending.';
