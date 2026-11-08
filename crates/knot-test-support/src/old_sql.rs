//! knot 0.9.9's write SQL, verbatim, in one place: 0.9.9 pods keep running
//! it against the M1.2 schema during the rolling upgrade to 0.9.10, so the
//! upgrade tests in knot-storage and knot-crdt replay it. Copied from the
//! `v0.9.9` tag; never edit it to fit new code, since a test that passes
//! against SQL 0.9.9 never ran proves nothing.

/// 0.9.9's `INSERT_UPDATES_SQL` (`crates/knot-storage/src/updates_store.rs`):
/// `$1` the page, `$2` the authors, `$3` the update bytes; returns the seqs.
/// The M1.2 trigger numbers its rows.
pub const OLD_INSERT_UPDATES_SQL: &str = "WITH ins AS (
             INSERT INTO doc_updates (doc_id, by_user_id, update_bytes)
             SELECT $1, u.by_user_id, u.update_bytes
             FROM UNNEST($2::uuid[], $3::bytea[])
                  WITH ORDINALITY AS u(by_user_id, update_bytes, ord)
             ORDER BY u.ord
             RETURNING seq, by_user_id
         ),
         contrib AS (
             INSERT INTO doc_contributors (doc_id, user_id)
             SELECT DISTINCT $1::uuid, ins.by_user_id
             FROM ins
             WHERE ins.by_user_id IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM doc_contributors c
                   WHERE c.doc_id = $1
                     AND c.user_id = ins.by_user_id
                     AND c.last_edited_at >= now() - interval '1 minute'
               )
             ORDER BY ins.by_user_id
             ON CONFLICT (doc_id, user_id) DO UPDATE
                 SET last_edited_at = now()
                 WHERE doc_contributors.last_edited_at < now() - interval '1 minute'
         )
         SELECT seq FROM ins ORDER BY seq";

/// 0.9.9's snapshot upsert (`PgSnapshotStore::insert`): `$1` the page,
/// `$2` the snapshot seq, `$3` the state bytes, `$4` the state vector.
pub const OLD_SNAPSHOT_UPSERT_SQL: &str =
    "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes, state_vector)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (doc_id, snapshot_seq) DO UPDATE
             SET state_bytes = EXCLUDED.state_bytes,
                 state_vector = EXCLUDED.state_vector,
                 created_at = now()";
