# Page history: technical architecture

**Date:** 2026-11-04
**Implements:** [2026-11-04-page-history-design.md](2026-11-04-page-history-design.md)
**How it was made:** three independent proposals (simplest, CRDT-native, risk-first), each checked against the yrs 0.27.3 / @tiptap/y-tiptap 3.0.9 sources, scored by three judges (CRDT correctness, operations and security, implementability), then merged from the top-scoring proposal with every fatal flaw removed. File references are as of `7e1de9d`.

**Deviations recorded in the M1.1 plan:** the `knot_replicas` heartbeat moves from M1.1 to M3 (erase), the only milestone that reads it; the `knot_history_table_bytes` gauge moves to M1.3 with the other history metrics; the append-only trigger and its function are both named `doc_updates_append_only`. M1.1's trigger is `BEFORE DELETE OR UPDATE OF update_bytes, doc_id` (no `rev`, which doesn't exist yet), and it allows a DELETE only at `pg_trigger_depth() > 1` (no NOT EXISTS arm; see storage_model). M1.1 ships two migrations, each its own transaction, and each takes its table locks up front in a bounded retry loop that never waits on one table while holding another. The first locks `documents` then `doc_updates` (SHARE ROW EXCLUSIVE), the app's lock order, and records the moment deletes became impossible in `doc_history_meta`. `documents.history_since` is the second (`20261104120001_history_since`), set to that moment for existing pages: adding it takes ACCESS EXCLUSIVE on `documents`, and as the last statement of the first migration that lock upgrade deadlocked with any page write that had read or row-locked `documents` before writing it (create, move, archive, restore, purge). See baseline.

**Deviations recorded in the M1.2 plan** ([2026-11-07-page-history-m1-2-ordered-log.md](../plans/2026-11-07-page-history-m1-2-ordered-log.md), Rulings): M1.2 ships five migrations instead of one transaction plus the CIC file, because ADD COLUMN needs ACCESS EXCLUSIVE on `doc_updates`, which would stop every page load for the seed's duration: `20261107120000_history_log_columns` (the lock helper and catalog-only column changes under ACCESS EXCLUSIVE on `doc_updates` and `doc_snapshots`), `20261107120001_history_heads` (heads table without its FK, `doc_updates_assign_rev`, the seed, the re-created append-only trigger, under SHARE ROW EXCLUSIVE on `doc_updates` alone, so reads continue and only saves wait), `20261107120002_history_heads_fk` (the FK, after deleting orphan heads rows, and `doc_snapshots_epoch`, both moved out of the seed's transaction: a FK check per seeded row would take KEY SHARE on `documents` rows while holding `doc_updates`, the M1.1 purge deadlock, and holding `doc_snapshots` for the seed would freeze 0.9.9 rooms, which await snapshot writes inside their actor loop; nothing reads `content_epoch > 0` before M3, so a snapshot written between the two is harmless), `20261107120003_history_rev_index` (`CREATE UNIQUE INDEX CONCURRENTLY`, one statement) and `20261107120004_history_rev_index_check`, a repair migration that rebuilds a missing or invalid index under ACCESS EXCLUSIVE, because `IF NOT EXISTS` skips an invalid index on a re-run, and a CIC can be aborted by the deadlock detector when a starting knot-server (every start migrates) waits on sqlx's session advisory lock with a live snapshot that the CIC's wait-for-older-snapshots phase waits on; a Job killed mid-build usually leaves the build running in Postgres, still holding the advisory lock, so the Helm README tells operators to check `pg_stat_progress_create_index` before a re-run and not to restart pods during the build. The M1.1 lock loop is factored into `knot_lock_tables(who, tables, lock_mode, give_up_after)` (M1.1's files stay as shipped); its lone wait on an autovacuum worker always requests SHARE UPDATE EXCLUSIVE, whatever the target mode, so it conflicts with the worker but with no reader or writer; migration 3 calls it with `doc_history_heads` before `documents` and `doc_snapshots`, so its one bounded wait goes to the busy table (every in-flight 0.9.9 save holds ROW EXCLUSIVE on it through the trigger). `doc_updates_source_check` is added `NOT VALID` and never validated (every existing row is NULL). The append-only trigger now fires on UPDATE of every column, and its numbering rule compares every column except `rev` explicitly (not `to_jsonb`, which would hex-encode every row's bytes twice), with a test that fails when a column is added. Both insert arms of the heads upsert number a page's first rows from 1 (trigger path `head_rev = 1`, flush path `head_rev = n`, so base = 0, where this document's `VALUES ($1)` takes the defaults and gives rev 0 and base −n) and set `dirty = true, last_write_at = clock_timestamp()`, so M1.3's `ORDER BY last_write_at` never meets a NULL; every flush rewrites `last_write_at`, the key of `doc_history_heads_dirty_idx`, so each flush leaves one non-HOT heads version for vacuum: WRITE PATH's "the heads row does not bloat" means one heads update per flush instead of one per row, not no bloat; it is kept because M3's `page_active` reads `last_write_at` as the latest write. The trigger replaces `created_at` with `clock_timestamp()` only when it equals `now()` (the column default), so explicit timestamps from tests and tools stay. Lock order is `documents` (row, KEY SHARE) → `doc_history_state` (row) → `doc_history_heads` (row) → `doc_updates` → `doc_contributors` → `doc_snapshots`, with the rule that the page row's KEY SHARE (`SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE`) is the first lock of any transaction that locks a page-scoped history row (`doc_history_state`, `doc_history_heads`, or the page's `doc_updates` rows); this amends BUILDER's "Lock order is doc_history_state → heads, the same as erase" to "documents (KEY SHARE) → doc_history_state → heads": M1.3's builder runs the KEY SHARE before its `doc_history_state` claim (`FOR UPDATE SKIP LOCKED`) and before inserting a missing `doc_history_state` row, and M3's erase before Phase B step 2's first SELECT (`… FROM doc_history_state … FOR UPDATE`), because a purge holds the page row FOR UPDATE and cascades into `doc_history_state` and `doc_history_heads`, and nothing retries an erase; M1.2 follows the rule for appends, trigger-path inserts, snapshot writes and legacy numbering, and wraps `purge` in `retry_on_deadlock` as a backstop. The trigger's `NEW.rev IS NOT NULL` arm (new code) takes no page lock, unlike its trigger path (0.9.9 pods, raw inserts): the flush's first statement already holds the page row, and the FK check still raises 23503. A stamped snapshot is immutable (`doc_snapshots_epoch` skips an UPDATE of a row with `snapshot_rev`), so a 0.9.9 upsert at the same `snapshot_seq` cannot replace its bytes and keep its `snapshot_rev`; `doc_snapshots_epoch` honours M1.1's `knot.history_rewrite` flag (it skips the immutability guard and the epoch check, not the page-row lock), so M3's erase may insert its snapshot at the new epoch before it bumps `content_epoch` (Phase B steps 5-6). `meta` may hold page titles (`source_title`, `template_title`); "never page text or names" means page content and person names. Today's Markdown restore writes source `restore` with meta `{target_snapshot_seq, target_ended_at}`, so M1.3 reads two meta shapes for `restore` until M2 replaces the route. Every server-side event (ApplyUpdate, ReplaceWithMarkdown, PatchTaskChecked, ApplySuggestion) persists before it fans out and evicts the room when the persist fails; the room awaits that persist, so it is bounded: a server-side job's deadline (`server_job_deadline`, 5 s) is armed by the first append attempt that fails after the job arrived, not by its arrival, so a slow attempt that succeeds or a backlog ahead of it never fails it; during an outage a server-side edit fails after about 5 to 25 s and the room evicts. `ApplyUpdate` persists its effective update (`apply_update_measuring(..).persist`, or the raw bytes when the apply parked or drained pending content), not the update `observe_update_v1` reports, which yrs omits when blocks park; an `ApplyUpdate` past `MAX_NESTING` is refused and evicts. An append attempt is bounded inside Postgres (`lock_timeout` and `statement_timeout` of 8 s set in the append transaction, and `pg_cancel_backend` from a drop guard for an attempt the writer abandoned after its 10 s `attempt_timeout`), so a stalled attempt holds no pool connection or locks for long; deterministic store errors (SQLSTATE classes 22, 42, XX; sqlx decode errors) are permanent, and timeouts (57014, class 54) halve the writer's batch, down to one row. An attempt abandoned after its COMMIT reached Postgres, or one whose connection broke during COMMIT, is retried, so a batch may land twice: the same yrs update at two revs, which replay applies idempotently, so M1.3 must show no entry for a row that integrates nothing. An Evict Fatal (buffer overflow or a missed server-job deadline) still writes, with ordered retries and no deadline, the jobs buffered ahead of its cause; only a Permanent Fatal fails them all. A new room waits for the previous writer of its page on that replica to finish draining (`acquire`, up to 15 s), and `main` drains every writer for up to 10 s after the HTTP drain on SIGTERM, so the chart's default `terminationGracePeriodSeconds` goes from 30 to 50 s; the barrier is per replica, so a client that reconnects to another replica while one drains can still be credited with the drained text. An update yrs rejects part-way is still neither persisted nor fanned out, and the room stops writing snapshots for the rest of its life (`unlogged`); rows read from the log that fail to apply are passed at room boot and in catch-up (logged, counted in `knot_room_row_apply_errors_total`) and also stop snapshots, while read-only loads (Markdown refresh and its sweep, `knot-server export-check`) still fail on such a row; M1.3's replay must pass such rows too and tolerate rows that stay pending forever because they build on unlogged content. Once an unlogged room ends, edits other editors saved inside the unlogged content are gone from the live page at the next boot, the one case where the live page held content the log lacks. The room keeps `last_applied_seq` beside `applied_through_rev`, and the Markdown cache and `ExportState` keep seq semantics; the bus is published once per flush with the highest rev, over an unbounded writer→room channel. Room boot fails on any store read error (it used to boot from whatever it read) and on a legacy count mismatch (`LegacyCountMismatch`, logged as "legacy rows do not match the count taken at upgrade" and counted in `knot_history_legacy_mismatch_total`, with no automatic fallback). ROOM CHANGES' "Legacy snapshots without a snapshot_rev fall back to seq" becomes a full replay: after a legacy snapshot (or none) boot re-applies every row of the page (`RowsAfter::All`), because a 0.9.x room could snapshot without a lower-seq peer row (`room.rs:755`); this happens at a page's first room boot, and on every read-only load (Markdown refresh and sweep, Duplicate without a live room, export-check) of a page no room has opened since the upgrade, since only a room writes a stamped snapshot (a read-only load on a database from before 0.9.10 falls back to the seq read). ROOM CHANGES' "snapshot_seq = that row's seq, which keeps old replicas' seq-based hydration working" does not hold for trigger-path rows during the rollout: a 0.9.9 insert draws its seq before its trigger waits on the heads row, so its seq and rev can be inverted against a 0.9.10 flush, and during migration 3's pause this happens on many pages at once; 0.9.9 editors miss those edits until they reconnect (nothing is lost: a 0.9.10 room boots a legacy snapshot with a full replay). `ensure_legacy_numbered` runs only at room boot in M1.2 (M1.3's builder adds its own call), short-circuited by one probe for a row with `rev = legacy_rows`. Every page is seeded into `doc_history_heads` (from `documents LEFT JOIN doc_updates`, including pages whose rows the pre-M1.1 GC pruned) with `last_write_at` = the migration's `now()`, so M1.3's builder visits such pages, oldest first, before pages written after the upgrade. The snapshot invariant M1.3 and M3 may rely on is that a stamped snapshot at rev R equals `base ∪ replay(R)`, `base` being the page's pre-upgrade content no row carries (empty for pages created after the upgrade), not "a subset of the replayed log" as the M1.2 tests in milestones have it; M1.3's genesis must apply every surviving row with `created_at < history_since`, including rows with seq ≤ the baseline's `snapshot_seq` (GENESIS's "seq > its snapshot_seq" would leave out a row the late-seq skip kept out of the baseline but the first stamped snapshot holds, and M1.3 would write a spurious authorless gap entry for it on every such page; replay is idempotent, so the extra rows change nothing else), and M3's erase rewrite of snapshots must keep the base. Content that only a 0.9.9 pod's legacy snapshot held during the rollout (a 0.9.9 writer dropped a failed batch, then its room snapshotted the live doc) is lost once a stamped snapshot exists, with no gap entry. The Task 1 spike did not fail: the effective update applies like the raw one over 300 random three-client histories (3,000 in a one-off run), with one extension found by the property test: besides a changed pending fingerprint, the room falls back to the raw bytes when `encode_update_v1` does not hold exactly the blocks the transaction integrated (decoded insertions ≠ `txn.insert_set()`), which covers blocks yrs 0.27.3 integrates past a gap behind a skip (left out of `encode_update_v1`) and gap fills (which re-send blocks the log holds); `knot_room_updates_persisted_raw_total` counts these too. The spike also found that once a doc holds such a gap, a resend whose block runs from the gap into held blocks panics yrs (`block_store.rs:318`, raw path included), and that content can stay parked for good, so M1.3's replay must expect rows that stay pending. Its yjs-side half (a yjs client accepts the effective bytes) rests on the CI `playwright` job (`two-users-converge`, `ws-reconnect`), a merge gate.


## Summary

Commit-ordered log: an append-only update log with a per-page commit-ordered rev, entries built in the background by canonical replay, server-rendered ProseMirror JSON diffs, restore as a verified yrs undo, and erase by verified yrs re-derivation inside one fenced transaction

## storage_model

PRINCIPLE
The doc_updates log is the single source of truth. It is append-only and kept for the page's whole life. Everything else (checkpoints, entries, caches, boot snapshots) is derived from it and can be rebuilt, except the pre-rollout baselines. Every new table has doc_id REFERENCES documents ON DELETE CASCADE, so purge still removes all of a page's history. Live rooms stay GC'd and keep hydrating from the latest snapshot plus newer rows. Rooms never replay history.

EXISTING TABLES
doc_updates (migrations/20260602000001_v0_1_schema.sql:92-99), kept forever.
- M1.1 trigger doc_updates_append_only (migrations/20261104120000_history_keep_updates.sql): BEFORE DELETE OR UPDATE OF update_bytes, doc_id, FOR EACH ROW. rev doesn't exist until M1.2. It allows:
  (a) a DELETE at pg_trigger_depth() > 1, which is the purge FK cascade: every cascade runs inside RI_FKey_cascade_del, while a top-level DELETE has depth 1. There is no NOT EXISTS (parent) arm;
  (b) from M1.2 only: an UPDATE that only sets rev where OLD.rev IS NULL (legacy numbering). M1.2 adds this rule to the function and re-creates the trigger with UPDATE OF update_bytes, doc_id, rev (see milestones);
  (c) anything when current_setting('knot.history_rewrite', true) = 'on'. Only the erase transaction and a future compaction set this, with SET LOCAL.
  Everything else raises SQLSTATE 'KH001'. It raises rather than silently skipping, so a missed delete path fails loudly. gc.rs:38-48 already logs and continues on error, so old replicas' hourly GC fails harmlessly from the moment the first M1.1 migration commits.
- M1.2 columns:
  - rev bigint NULL: per page, gapless, in commit order (see entry_builder).
  - source text NULL CHECK IN ('edit','task','suggestion','restore','restore_fallback','import','template','copy'). NULL means a legacy or old-replica row, read as 'edit'.
  - meta jsonb NULL: ids and timestamps only, never page text or names.
    - restore: {target_entry_id, target_rev, target_ended_at}
    - suggestion: {suggestion_id, accepted_by}
    - copy: {source_doc_id, source_title, version_entry_id, version_at}
    - template: {template_doc_id, template_title}
  - epoch int NULL: the content epoch the writer believed current.
- M1.2 index: UNIQUE (doc_id, rev) WHERE rev IS NOT NULL, built with CREATE UNIQUE INDEX CONCURRENTLY in its own migration file that starts with `-- no-transaction` (supported by sqlx-core 0.9.0, migrate/source.rs:234). The existing (doc_id, seq) index stays.
- M1.2 trigger doc_updates_assign_rev, BEFORE INSERT. It only acts when NEW.rev IS NULL, i.e. old-version replicas during the rollout or any path that bypassed the new SQL. It runs `INSERT INTO doc_history_heads(doc_id) VALUES (NEW.doc_id) ON CONFLICT DO UPDATE SET head_rev = head_rev + 1, dirty = true, last_write_at = clock_timestamp() RETURNING head_rev, content_epoch`, then sets NEW.rev and NEW.created_at = clock_timestamp(). It raises 'KH002' when content_epoch > 0, because old code carries no epoch. When NEW.rev IS NOT NULL it only checks NEW.epoch = content_epoch, reading the heads row this transaction already locked, and raises 'KH002' otherwise.

doc_snapshots: a boot cache only. History never reads it.
- M1.2 adds snapshot_rev bigint NULL and epoch int NULL.
- A BEFORE INSERT OR UPDATE trigger reads `SELECT content_epoch FROM doc_history_heads WHERE doc_id = NEW.doc_id FOR KEY SHARE`. That conflicts only with the erase's FOR UPDATE, not with writers. It raises 'KH002' when NEW.epoch differs (NULL is accepted only while content_epoch = 0).
- Thinning stays as today (5 newest + 1/day for 30 days) until M1.3, then becomes the 3 newest per page. The GC loop runs under pg_try_advisory_lock, as spawn_purge_job does.

documents: M1.1 adds history_since timestamptz NOT NULL, in its second migration. Existing pages get doc_history_meta.keeps_since, the moment the first migration made deletes impossible (a constant default, so no table rewrite); then the default becomes now(), so later pages get their creation time.

users: M4 adds history_anonymized_at timestamptz NULL.

NEW TABLES
1. doc_history_baselines (M1.1): id bigserial PK, doc_id, snapshot_seq, state_bytes, state_vector, created_at, redacted_at NULL. The migration fills it with INSERT … SELECT FROM doc_snapshots. Only an erase changes it afterwards, rewriting it in place.

2. knot_replicas (M3; planned for M1.1, see the deviation note at the top): replica_id uuid PK, version text, feature_level int, seen_at. Each replica upserts it every 60 s. Erase is refused unless every replica seen in the last 3 min reports feature_level >= ERASE_FENCE. That guard is not the whole fence: replicas older than M3 never write a heartbeat, so it cannot see them. Erase is also refused until knot_replicas has shown only feature_level >= ERASE_FENCE for longer than a rolling upgrade plus the maximum pod termination grace can take (see milestones, M3).

3. doc_history_heads (M1.2): doc_id PK, head_rev bigint NOT NULL DEFAULT 0, legacy_rows bigint NOT NULL DEFAULT 0, content_epoch int NOT NULL DEFAULT 0, dirty bool NOT NULL DEFAULT true, last_write_at timestamptz. Partial index (last_write_at) WHERE dirty.
   The M1.2 migration must run its statements in this order inside one transaction:
   (i) CREATE TABLE;
   (ii) CREATE TRIGGER doc_updates_assign_rev. This takes ShareRowExclusiveLock on doc_updates and blocks old-replica inserts until the migration commits;
   (iii) INSERT INTO doc_history_heads SELECT doc_id, count(*), count(*) FROM doc_updates GROUP BY doc_id.
   No row can therefore slip between the count and the trigger.
   Legacy rows (rev IS NULL) are numbered 1..legacy_rows in seq order by ensure_legacy_numbered(doc). It is idempotent, guarded by `WHERE rev IS NULL`, and allowed by trigger rule (b). It runs on a page's first room hydration or first builder visit.

4. doc_history_checkpoints (M1.3): doc_id, rev, kind CHECK IN ('genesis','periodic','gap'), state_bytes (a GC'd encode_state_as_update_v1), state_vector, created_at; PK (doc_id, rev).
   The builder writes a periodic checkpoint when the update bytes since the last checkpoint reach max(64 KiB, current state size). That bounds replay to about 2x the page size and keeps checkpoint storage at about 1x the log. No thinning in v1.

5. doc_history_state (M1.3): doc_id PK, built_rev bigint NOT NULL DEFAULT 0, builder_version int, snapshot_checked_rev bigint, updated_at. This is the builder watermark. Its row lock is the per-page work lock.

6. doc_history_entries (M1.3):
   - id bigserial PK, doc_id;
   - kind CHECK IN ('session','restore','copy','import','template','erase','baseline','gap');
   - author_id uuid NULL REFERENCES users. It stores user ids only; names are resolved at read time;
   - started_at, ended_at, start_rev NULL, end_rev NULL, sealed bool;
   - baseline_id NULL REFERENCES doc_history_baselines ON DELETE CASCADE;
   - ins_ids bytea and del_ids bytea: yrs IdSet v1 of exactly what this entry's rows integrated or newly deleted on the canonical replay. Together with author_id this is the item-level author map that later character blame needs;
   - words_added int NULL, words_removed int NULL;
   - meta jsonb NOT NULL DEFAULT '{}': accepted_by [user ids], restore target, copy source, template;
   - updated_at.
   Indexes: (doc_id, end_rev DESC, id DESC); (doc_id, author_id) WHERE NOT sealed; (ended_at) WHERE NOT sealed.

7. doc_history_erasures (M3): id bigserial, doc_id, requested_by uuid, state CHECK IN ('running','done','failed'), heartbeat_at, from_epoch, to_epoch, erased_ids bytea (an IdSet of the erased character ids), needle_len int, places int, versions int, error text NULL, created_at, finished_at.
   It never stores the text or a hash of it, because a hash of a short secret can be brute-forced.

WHAT IS KEPT FOREVER
Every doc_updates row, every baseline, checkpoint, entry and erasure record. Nothing expires and there is no retention job. A later compaction (M5, only if measurements demand it) may merge, with yrs::merge_updates_v1 under knot.history_rewrite, the rows of one author and source between two consecutive entry end_revs. Every entry boundary, IdSet and author stays exact.

OPS
- Metrics:
  - knot_history_table_bytes{table}: pg_total_relation_size every 10 min under an advisory lock;
  - knot_history_builder_lag_revs, knot_history_rows_built_total, knot_history_replay_seconds, knot_history_view_seconds;
  - knot_history_restore_total{outcome}, knot_history_erase_total{outcome}, knot_history_gap_total;
  - knot_writer_flush_retries_total.
- CLI `knot-server history-stats` reports avg(octet_length(update_bytes)), rows per page per day (p50, p95 and max over 30 days) and checkpoint bytes. Run it on a real instance before tuning anything.
- Helm README sizing: rows/day × average bytes × about 1.6 for tuple, index and TOAST overhead, plus checkpoints at about 1x that. Backups and PITR grow at the same rate. Values history.workers and history.replayMaxBytes map to KNOT_HISTORY_* env vars.
- Every new table goes into the TRUNCATE list in e2e/support/reset.ts and into crates/knot-storage/tests/migrations_apply.rs. That test also asserts that a top-level DELETE raises KH001 and that a purge cascade still deletes.

ROLLOUT FLAG
The M1.1 migrations are the flag. They run as the Helm pre-upgrade hook, before any new pod starts and while old pods still run. For existing pages history_since is keeps_since: clock_timestamp() taken by the first migration right after CREATE TRIGGER, while it holds SHARE ROW EXCLUSIVE on doc_updates. No DELETE on doc_updates can run from then until COMMIT, and after COMMIT the trigger refuses one; a row GC deleted was deleted before that moment and created before it was deleted. So every doc_updates row with created_at >= keeps_since still exists, and the moment is exact, however long the second migration takes to follow. Pages created between the two migrations exist when the second runs and get keeps_since too. Almost all have a later created_at and count as new pages, and almost all edits old pods saved in the gap become credited entries, not genesis. created_at is the transaction start (DEFAULT now()) for pages and edits alike, so a create or an edit save already in progress when the first migration took its locks (it then waited for COMMIT) has an earlier created_at: that page counts as predating history and that edit folds into genesis uncredited. Both err on the safe side; nothing is lost. "Complete history starts" needs no heartbeat or manual switch.

## entry_builder

WRITE PATH (M1.2)
Commit order comes from rev, which replaces the settle delay.
- INSERT_UPDATES_SQL becomes an explicit transaction:
  1. `INSERT INTO doc_history_heads(doc_id) VALUES ($1) ON CONFLICT (doc_id) DO UPDATE SET head_rev = doc_history_heads.head_rev + $n, dirty = true, last_write_at = clock_timestamp() RETURNING head_rev - $n AS base, content_epoch`.
  2. The app compares content_epoch with the room's epoch and rolls back with Fatal if they differ.
  3. One INSERT … UNNEST with rev = base + ord, epoch, source[] and meta[], plus the existing contrib CTE.
  4. COMMIT.
- The heads row lock is taken first and held until commit, which has two effects:
  - Rev order is commit order by construction. A lower rev can never become visible after a higher one, so there are no late rows and no settle delay.
  - Lock order is always heads → doc_contributors, which removes the deadlock the rev trigger would otherwise create against the contrib CTE.
- There is one heads update per flush, not one per row, so the heads row does not bloat. created_at comes from clock_timestamp() after the lock, so it is near-monotonic in rev.
- Duplicate's direct insert goes through the same SQL. Old replicas take the trigger path.

ROOM CHANGES (crates/knot-crdt/src/room.rs, writer.rs)
- The writer keeps a failed batch in order and retries with backoff from 250 ms to 5 s.
  - KH001 and KH002 are permanent: the writer sends Fatal and the room evicts itself with evicted = true, writing no snapshot.
  - A buffer over 20k jobs or 32 MiB also evicts the room.
  - Clients resend after the 1012 reconnect.
- The room keeps an in_flight counter.
- The room keeps applied_through_rev, the highest rev such that every rev at or below it is applied.
  - An own confirmation [a..b] with a > applied_through + 1 triggers an immediate catch-up of the gap. This is exact because revs are gapless.
  - Catch-up uses since_rev(applied_through_rev) and replaces since(last_applied_seq). This fixes the existing late-lower-seq skip (room.rs:762-764).
- Idle, every-N and exit snapshots are written only when in_flight == 0 and there is no rev gap. They are stamped with snapshot_rev = applied_through_rev, the current epoch, and snapshot_seq = that row's seq, which keeps old replicas' seq-based hydration working.
- Hydration reads heads.content_epoch, then the snapshot, then rows with rev > snapshot_rev. Legacy snapshots without a snapshot_rev fall back to seq.

BUILDER (new crate crates/knot-history: builder.rs, replay.rs, sessions.rs, words.rs; spawned on every replica from main.rs; never inside a room)
- Claiming work:
  - Every 2 s: `SELECT doc_id FROM doc_history_heads WHERE dirty ORDER BY last_write_at LIMIT 20`.
  - Per page: `SELECT … FROM doc_history_state WHERE doc_id = $1 FOR UPDATE SKIP LOCKED`, inserting the row if it is missing. That row lock is the per-page leader election, with no global leader. A crashed replica's lock dies with its connection.
  - Lock order is doc_history_state → heads, the same as erase. Writers only ever take heads.
- A per-replica semaphore (KNOT_HISTORY_WORKERS, default 2) limits concurrent builds. Each pass is at most 5,000 rows.
- Replays run on a blocking thread with a 16 MiB stack.

GENESIS
- New page (created_at >= history_since): an empty doc at rev 0.
- Existing page: the latest baseline plus every surviving row with seq > its snapshot_seq and created_at < history_since. The result is written as a 'genesis' checkpoint, and entries start after it.

PER ROW, in rev order, on one GC'd Doc
1. In one transact_mut(), call apply_update(Update::decode_v1(bytes)).
2. Record I = insert_set().clone() and D = delete_set().clone(). yrs trims known blocks before integrating (transaction.rs:826-828), so resent or known content contributes nothing.
3. Pending: if the row parked structs or deletes, compare store().pending_update() and pending_ds() before and after. Record the row's claims (Update::insertions(true) and its delete set) in a map from parked claims to (rev, author). When a later apply integrates them, the part of I or D that intersects those claims is credited to the parked row's author.
4. Rows whose I and D are both empty only advance built_rev.

GROUPING
- Rows with source NULL, 'edit', 'task' or 'suggestion' (sessions are per author):
  - If the author's open session's last row is no more than 20 min older, the row joins it. Otherwise that session seals and a new one opens.
  - The session accumulates ins_ids and del_ids, and records started_at, ended_at, start_rev and end_rev.
  - A 'suggestion' row also adds meta.accepted_by and the suggestion_id.
- Rows with source 'restore', 'restore_fallback', 'import', 'template' or 'copy': the same author's open session seals, and the row becomes a sealed single-row entry with its meta.
- 'erase' entries are written only by the erase transaction.
- Rows with author NULL (legacy, before history_since) only feed genesis and are never credited.
- There is no midnight cut, so boundaries depend only on rows and are identical for every viewer.

SEALING AND WORD COUNTS
- A sweep every 60 s seals sessions idle for more than 20 min, using the partial index on ended_at.
- At seal, words_added and words_removed are counted as whitespace tokens in the Added and Removed chunks of exactly the diff the view renders (view_and_diff). The counts therefore always match what Show changes highlights.
- Open sessions are listed as 'in progress' with no counts.

CHECKPOINTS
Written by the max(64 KiB, state size) policy.

GAP DETECTION
- When built_rev reaches the snapshot_rev of an unchecked snapshot, the builder applies its state_bytes to a clone of the replay doc.
- A non-empty insert or delete set means the snapshot holds content no row carries. The builder writes a 'gap' checkpoint (the union state) and a sealed authorless 'gap' entry ("Changes saved without an author"), and increments knot_history_gap_total. Replays apply gap checkpoints when they pass that rev.

ATOMICITY AND REBUILD
- Entries, checkpoints and built_rev commit in one transaction. dirty is cleared in that same transaction only if head_rev = built_rev.
- Bumping BUILDER_VERSION rebuilds every entry from genesis. That works because the log is complete.

READ API
GET /api/docs/{id}/history?cursor=&limit=50 returns {history_since, catching_up, entries:[{id, kind, author:{key, name, color, former}, started_at, ended_at, open, words_added, words_removed, meta}], next_cursor}.
- catching_up is built_rev < head_rev − 500.
- Order: log entries by end_rev DESC, id DESC; then the baseline section by ended_at DESC.
- The cursor is an opaque keyset over (section, end_rev or ended_at, id).
- Access: require_doc_role_mw + require_editor, so Viewers get 403.
- Responses carry Cache-Control: private, no-store.

## attribution

DEFINITION
What an entry inserted and deleted is what its rows changed on the canonical replay in rev (commit) order: TransactionMut::insert_set() and delete_set() per row. It is credited to that row's by_user_id. Raw bytes and yrs client ids are never consulted.

TRUSTWORTHY LOG (M1.2)
1. Persist and fan out the effective update.
   - Engine::apply_update_measuring (crates/knot-crdt/src/engine.rs:141-186) already holds the TransactionMut, commits it and compares pending_fingerprint. It now also returns txn.encode_update_v1() taken after commit. That is the blocks from before_state plus this transaction's delete set (transaction.rs:597-628).
   - Room::on_inbound persists and fans out those bytes.
   - When the apply newly parked pending structs or deletes (pending_fingerprint changed), the raw bytes are persisted instead. encode_update_v1 omits pending content, so this fallback means nothing is lost.
   - Effects:
     - a reconnect SyncStep2 no longer writes other people's items or the whole delete set into the log;
     - rows get smaller;
     - erased items cannot be written back by a resend.
2. Nothing is dropped. Writer retry, room eviction on Fatal or overflow, snapshots only when in_flight == 0, and gap detection for anything that still escapes. Content without a row becomes an authorless 'gap' entry and is never credited to the wrong person.

SERVER-SIDE EDITS
Each is its own row stamped by its route, so the room's shared client id is irrelevant:
- restore → the restorer (source restore, meta target);
- task toggle → the toggler (source task);
- suggestion accept → the suggester. Source is suggestion and meta.accepted_by is the owner: the accept route passes claimed.decided_by into Event::ApplySuggestion, which puts it in PersistJob.meta;
- markdown import or replace and workspace import → the caller (source import);
- template → the caller (source template);
- Duplicate and Restore as new page → the actor (source copy).
Restore uses a scratch doc with a fresh random client id (see restore_in_place). That gives restored items their own client range and keeps later character blame clean.

HARD CASES
- Two people at once: separate rows even inside one 250 ms batch, each with its own author and effect sets. Sessions are per author, so overlapping sessions coexist.
- Reconnect or offline resend: items the server already had integrate nothing, and already-deleted ids are not newly deleted. Only genuinely new content is credited, to the connection that delivered it. Offline edits are credited to their author and timestamped at reconnect, which the spec accepts.
- Duplicate delivery through two replicas: the first row in rev order wins and the second is a no-op.
- Pending: the parked-claims map credits structs that integrate later to the row that delivered them.

RESIDUAL
A process crash with buffered writes during a database outage, followed by another user resending that content before the author does, credits the resender. Both conditions together are needed, so this does not happen in normal use.

## view_and_diff

The server renders ProseMirror JSON. No Y data for a past version ever reaches the client. The live bound doc is never touched, and y-tiptap's renderSnapshot is never used: its catch paths delete Y items (y-tiptap.js:966-976, 1003-1009) and its plain path mutates the doc.

RECONSTRUCTING THE PAGE AFTER ENTRY X (end_rev E, crates/knot-history/src/view.rs, run in spawn_blocking under the history semaphore)
1. Pick the nearest checkpoint at or before start_rev − 1 (genesis, periodic or gap). Load it into H = Doc::with_options(Options{skip_gc: true, ..Default::default()}) (doc.rs:188, 511-540).
2. Replay rows (checkpoint_rev, E] into H, applying any gap checkpoint state when the replay passes its rev. Items deleted during X's window keep their content, because they were inserted after the checkpoint or were visible in it.
3. S = H.transact().snapshot() (transaction.rs:41-47).
4. Baseline entries use their state_bytes directly, with Show changes off.
5. With Show changes off, walk H's 'default' fragment with children() (types/xml.rs:1064). It yields only non-deleted nodes. Emit PM JSON.

SHOW CHANGES: ONLY X'S OWN CHANGES
Let I = the entry's ins_ids and D = its del_ids (IdSet::decode_v1).
- Removed = D.diff(&I): pre-existing items X deleted.
- Added = I.diff(&S.delete_set): X's insertions still alive.
- S_lo = Snapshot::new(S.state_map, S.delete_set.diff(&Removed).merge(&I)): the page at X's end without X's changes. Another author's interleaved inserts are visible in both S and S_lo. Another author's deletions are visible in neither, so they never render.
- S_union = Snapshot::new(S.state_map, S.delete_set.diff(&Removed)), passed through strip_zero_clocks(). That helper exists because Store::write_blocks_to does find_index(clock - 1) (store.rs:179) and must never see a zero clock.
- U = a fresh skip_gc doc with H.transact().encode_state_from_snapshot(&S_union, &mut EncoderV1) applied. That call requires skip_gc (store.rs:152-163).

Walk U's 'default' fragment (children() now yields the union of both states):
- Elements:
  - XmlOut::id() (xml.rs:104) gives BranchID::Nested(id) (branch.rs:270-278, 914-917). attrs.historyChange is 'added' if id ∈ Added and 'removed' if id ∈ Removed.
  - Attributes are read with Xml::attributes / get_attribute (xml.rs:1032-1039) plus Out::to_json (out.rs:203). Elements visible in S are read from H; removed elements are read from a D_lo doc built with encode_state_from_snapshot(S_lo).
  - An element present in both whose attributes differ between H and D_lo gets 'changed'. This covers a task check, a heading level and table widths.
- Text:
  - Collect the XmlTextRefs first, then call t.diff_range(&mut U.transact_mut(), Some(&S), Some(&S_lo), |c| c) for each (types/text.rs:431-450).
  - DiffAssembler (text.rs:569-680) skips items seen in neither snapshot. It emits ChangeKind::Removed when an item is not visible in hi, and ChangeKind::Added when it is visible in hi and not in lo, starting a new chunk per changed item.
  - Removed chunks get the mark historyDelete, Added chunks get historyInsert, and other chunks are plain.
  - Format items apply only when seen in hi, so formatting renders as of S.
- PM JSON conversion (pm_json.rs, generic):
  - The tag is the node type.
  - Attribute values go through to_json.
  - Text chunk attribute keys become marks, using y-tiptap's hashed-name rule /(.*)(--[a-zA-Z0-9+/=]{8})$/ (y-tiptap.js:1490-1494). Adjacent text is merged.
  - Tags not in tools/schema.json become a placeholder paragraph.
  - A golden test compares pm_json with y-tiptap's exported yXmlFragmentToProsemirrorJSON (y-tiptap.js:2464, exported at :2939) over the repo's corpora.
  - The same function gives the canonical JSON used to verify restores.

API
GET /api/docs/{id}/history/entries/{eid}/view?changes=0|1 returns {doc, kind, can_restore, can_erase}. can_erase is true when role == Owner.
- Gate: require_doc_role_mw + require_editor on every request. Share links and exports never call it.
- Caching:
  - in-process LRU (64 MiB) keyed by (entry_id, entry.updated_at, content_epoch, RENDERER_VERSION, changes);
  - ETag from the same key;
  - Cache-Control: private, no-store, so browsers never keep past versions on disk.

CLIENT (web/src/features/docs/history/)
- A separate read-only editor: useEditor({editable: false, content: json, enableContentCheck: true, onContentError → 'This version can't be shown'}) (@tiptap/core index.d.ts:1738-1768).
- Its extensions come from createSchemaExtensions(), a refactor of createExtensions in web/src/features/editor/extensions.ts. It keeps the content extensions only: no Collaboration, CollaborationCaret, or comment and suggestion plugins.
- It adds HistoryDiffExtension:
  - Mark.create 'historyInsert' (green underline) and 'historyDelete' (red strike-through);
  - Extension.create with addGlobalAttributes (index.d.ts:938), a historyChange attribute on all node types rendered as data-history-change and styled with design tokens.
- Embedded boards show their current content. A deleted blob shows as broken.

DRAWER
- The existing right drawer, widened to min(1100px, 92vw), with the dimmed page behind it. It stacks on narrow screens with a back control and is not mounted on archived pages.
- Left column: the infinite timeline, grouped under local-day headings of ended_at. It shows the spec's wording per kind, then 'Earlier versions (author unknown)' and 'Complete history starts <history_since>'.
- Right column bar: Show changes (on by default; disabled for baseline, erase and gap entries), Restore here, Restore as new page, and Erase from history (Owner, while text is selected).

LIMITATION
Attribute-only changes show as a node-level 'changed' marker, not inline.

## restore_in_place

POST /api/docs/{id}/history/entries/{eid}/restore
- Allowed for Editors and Owners (require_editor). The middleware returns 409 on archived pages.
- The work runs off the room actor in crates/knot-history/src/revert.rs. The room only ever applies an update that was verified first.

ALGORITHM, for target entry T (the state at its end_rev, or a baseline state)
1. Target.
   - Replay into a skip_gc doc H up to T, as in view_and_diff.
   - target_json = pm_json(H 'default').
   - H_T_bytes = H.transact().encode_state_as_update_v1(&StateVector::default()). A skip_gc store encodes deleted items with their content.
2. Live state. Room Event::ExportForRevert returns (full live state v1, content_version: u64). content_version increments on every applied change: inbound, peer catch-up and server mutations.
3. What changed since T.
   - P = Doc::with_options(Options{skip_gc: true, client_id: random, ..}). The client id is redrawn if it appears in either state vector.
   - Apply H_T_bytes, then the live state. Items deleted since T keep their content in P.
   - sv_now and ds_now come from P's snapshot.
   - insertions: for each client c with sv_now[c] > sv_T[c], IdSet::insert(ID::new(c, sv_T[c]), sv_now[c] − sv_T[c]).
   - deletions = ds_now.diff(&ds_T).
   - If pm_json(live) == target_json, return 200 'already at this version' and write nothing.
4. Revert with yrs's own undo machinery.
   - UndoManager::<()>::with_options(undo::Options{init_undo_stack: vec![StackItem::new(P.guid(), deletions, insertions)], ..Default::default()}) (undo.rs:146-166, 791-813, 894; Meta has a blanket impl, undo.rs:106-113).
   - expand_scope(&P, &P.get_or_insert_xml_fragment("default")) (undo.rs:168-198).
   - Subscribe P.observe_update_v1, then call undo_blocking() (undo.rs:543).
   - try_process (undo.rs:640-720) deletes the in-scope insertions that are still alive. It redoes the in-scope deletions that are not among the insertions, and ItemPtr::redo redoes deleted parents first.
   - The captured bytes R contain exactly: deletions of what was added since T, and new copies of what was removed since T, under P's fresh client id.
   - Items present both at T and now are untouched. Comment and suggestion RelativePosition anchors and the authorship of unchanged text survive. Comments on text the restore removes behave as on deleted text today.
5. Verify.
   - Load the live state into a scratch GC'd Doc and apply R.
   - Require has_missing_updates() == false (transaction.rs:285) and pm_json(scratch) == target_json.
   - Fallback when verification fails or the undo produced nothing although the states differ: R' clears 'default' and inserts deep copies of the target's children via XmlElementRef/XmlTextRef::as_prelim (xml.rs:357, 641). It is lossless but anchors are lost. R' is verified the same way and persisted with source 'restore_fallback', and the timeline shows it.
   - If that also fails: 500 history.restore_failed, and nothing is written.
6. Apply. Room Event::ApplyRevert{update, expected_version, by_user, meta}.
   - If content_version ≠ expected_version, the room replies Stale. The handler redoes steps 2-5; only the cheap live part is re-done, because H_T_bytes and target_json are cached. After 5 attempts it returns 409 history.restore_busy and the UI asks the user to retry.
   - Otherwise the room applies R in one transaction and captures it with observe_update_v1, as ReplaceWithMarkdown does (room.rs:429-505).
   - It persists before fan-out (the ApplySuggestion pattern), with source 'restore', meta {target_entry_id, target_rev, target_ended_at} and by_user = the restorer. If persisting fails, the room evicts itself with no snapshot.
   - Then it fans out. Other replicas get the change via the bus. Connected editors receive an ordinary y-sync update: no reload and no Markdown. The broadcast update is exactly the persisted one, which avoids the 0.2.1 failure mode.

OTHER Y.DOC ROOTS
- Only 'default' is in scope.
- knot_applied_suggestions (crates/knot-crdt/src/suggestions.rs:101) is deliberately not reverted. It is the ledger of applied suggestions; reverting it would let an accepted suggestion apply twice. The suggestion stays accepted while its text can be restored away.
- Any other root is left alone and logged.
- The title is never touched.

TIMELINE
- The builder records 'Restored by X, to <target ended_at>'.
- Undo is a restore of the entry before it. The restore row's meta also stores target_rev, and the pre-restore state is rev − 1, so an exact 'Undo restore' needs no new data.

REMOVED
The Markdown round-trip routes (/history/{seq}/markdown, /history/{seq}/restore) and the seq-based list are removed in this milestone. ReplaceWithMarkdown stays for imports.

## restore_as_new

POST /api/docs/{id}/history/entries/{eid}/restore-as-new with body {version_label}.

Permission
The same check Duplicate uses (crates/knot-server/src/duplicate.rs check): not a Viewer on the page, and the parent is editable. The UI hides the button otherwise.

Content
- A GC'd replay up to the target, or the baseline's state_bytes, then encode_state_as_update_v1.
- Like Duplicate, it copies the whole state, including knot_applied_suggestions. Those markers do nothing on a page with no suggestions.

Creation
duplicate::run handles a single page with a content override: PageCopy.state is set explicitly (crates/knot-storage/src/duplicate.rs:25-40). It reuses:
- the same parent, with the sort_key directly after the original;
- board and blob reference remapping through the existing remap path. Boards are copied with their current content;
- the restricted-copy owner-grant rules;
- the audit event doc.duplicate with data {source_id, version_entry_id}.

Title
'<current title> (version of <label>)'. The label is the date formatted on the client in local time, validated on the server: trimmed, at most 64 chars, no control characters.

History of the new page
- Its first doc_updates row goes through INSERT_UPDATES_SQL in the duplicate transaction with source 'copy', epoch 0, by_user = the caller and meta {source_doc_id, source_title (title at copy time), version_entry_id, version_at}.
- history_since = created_at.
- The builder makes the entry 'Created from <source title>, version of <date>, by X'.
- Plain Duplicate writes the same row without the version fields: 'Created from <source title> by X'.

Not affected
The original page is not touched. An erase on the original does not reach the copy, as the spec says.

## erase

WHO AND WHEN
- Owners only (DocAccess.role == Owner), on non-archived pages.
- Refused (409 history.erase_unavailable) unless every replica in knot_replicas seen in the last 3 min reports feature_level >= ERASE_FENCE, and knot_replicas has shown only such replicas for longer than a rolling upgrade plus the maximum pod termination grace can take. The heartbeat guard alone cannot see replicas older than M3, which never write one (see milestones, M3). Older replicas lack the read-side fences below.
- Not undoable. The needle lives only in request memory and is never stored or logged.

MATCHING (crates/knot-history/src/erase/matcher.rs)
- Scope: exact, case-sensitive matching on the plain text of one XmlText, across formatting and item boundaries. Text split by an inline node is not matched, and the dialog says so.
- Replay the page in rev order into one GC'd Doc, from genesis or empty, applying gap checkpoints. Each baseline and the genesis state is scanned as an extra version.
- After each row, for every XmlText the row touched:
  - Touched texts are found with observe_deep on 'default' (types/mod.rs:422): XmlTextEvent targets, plus the subtrees in XmlEvent Change::Added.
  - Get its string with GetString.
  - If it contains the needle, take now = txn.snapshot() and call diff_range(&mut txn, Some(&now), Some(&Snapshot::default()), |c| c). Every visible item is then 'Added' and yields its own chunk with its start ID (text.rs:569-680).
  - Map each character to (client, clock + UTF-16 offset). String clocks are UTF-16 (block.rs:699, 1770-1777).
- Every occurrence adds its character ids to R, including transient ones typed and deleted within one session, and ones brought back by restores, which are new items.
- Counts:
  - places = distinct occurrence id tuples;
  - versions = entries and baselines whose end state contained an occurrence.
- Plain strings in other roots (Map or Array ContentAny) that contain the needle are recorded for whole-value redaction.
- Not matched: attribute values (link href, image alt), boards, files.

PREVIEW
POST …/history/erase/preview {text} returns {places, versions, short: len < 6, page_active}.
- page_active is true when last_write_at is within 5 min or a local room has connections.
- It runs synchronously in spawn_blocking within KNOT_HISTORY_ERASE_BUDGET. Past the budget it returns 409 history.too_large.
- Nothing is stored.
- The dialog shows the text, the counts, the short-text warning, the open-elsewhere warning, and one sentence on what an erase does not reach.

EXECUTION
POST …/history/erase {text} → 202 {erasure_id}. The client polls GET …/history/erasures/{id}, which works from any replica because it reads the row.
- The job inserts a 'running' row and refreshes heartbeat_at every 10 s.
- A sweeper marks rows whose heartbeat is older than 2 min as 'failed'.
- Nothing else is held, because all mutation happens in ONE transaction. A crash rolls back and leaves no stuck lock.

Phase A (no lock). The log is append-only, so rows up to R0 = head_rev cannot change.
1. Run the matcher up to R0 to get R.
2. Re-derive every affected row with yrs itself, replaying in rev order on a GC'd Doc:
   - Apply the row, then, in the same TransactionMut, a delete-only update for R ∩ txn.insert_set(). Its bytes are varuint 0 (no structs) followed by IdSet::encode_v1, which is the v1 delete-set layout (id_set.rs:336-345).
   - At commit, GCCollector::collect turns those items into ItemContent::Deleted(len) and keeps id, origins and parent (gc.rs:12-58, block.rs:1556-1567). Later rows still integrate.
   - New bytes = txn.encode_update_v1().
   - The rewrite set is: every row whose update_items::decode ids intersect R; plus every row whose replay transaction drained pending structs or deletes parked by a rewritten row. encode_update_v1 omits pending content (the judges' finding), so that content has to move to the row that integrates it.
   - An emptied row is kept, with its author and time.
3. Rewrite checkpoints and baselines: load each state into Doc::new(), apply the delete-only update for R, commit (GC), and run encode_state_as_update_v1. The state vector is unchanged.
4. Verify Phase A unlocked by replaying the rewritten log:
   - no pending at the end (has_missing_updates() == false);
   - the state vector equals the original one;
   - pm_json at each checkpoint rev equals the rewritten checkpoint;
   - update_items::decode finds no String item with an id in R in any rewritten blob;
   - the matcher re-run finds 0 occurrences.
   Keep the doc at R0 in memory.

Phase B (one transaction):
1. `SET LOCAL knot.history_rewrite = 'on'`.
2. `SELECT … FROM doc_history_state WHERE doc_id = $1 FOR UPDATE`, which excludes the builder. Then `SELECT … FROM doc_history_heads WHERE doc_id = $1 FOR UPDATE`, which blocks every writer's first statement and every snapshot insert.
3. Catch up rows (R0, head]: run the matcher and the re-derivation from the in-memory R0 state. If R gained ids whose rows are at or below R0 (someone completed the needle using older characters meanwhile), roll back and re-run Phase A once, then fail with 409 history.erase_busy.
4. UPDATE the rewritten doc_updates rows, checkpoints and baselines (set redacted_at).
5. Delete the page's doc_snapshots and insert one fresh GC'd snapshot of the rewritten head, with the new epoch and snapshot_rev = head.
6. `UPDATE doc_history_heads SET content_epoch = content_epoch + 1`.
7. Recompute word counts for entries whose ins_ids or del_ids intersect R, using the view diff on the rewritten log. Entry IdSets stay valid because ids are stable.
8. Side tables, scoped to the doc:
   - comments.anchor_text, suggestions.original_text, proposed_text and context_before, and doc_tasks.text: `replace(col, $needle, '[removed]') WHERE strpos(col, $needle) > 0`.
   - Truncated excerpt fields (the notification excerpt, EXCERPT_CHARS = 140 in crates/knot-server/src/suggestion_notifications.rs:30, and context_before) also get a leading needle-suffix or trailing needle-prefix of at least min(3, len) chars replaced.
   - notifications.data (data doc_id) and audit_events.data (target_id = doc): JSON walked in Rust, replacing inside string leaves only.
   - doc_markdown_cache is re-serialised from the rewritten head with knot_markdown::to_markdown::serialise. body_tsv is a generated column, so the search index follows.
   - doc_tasks is re-extracted through the existing refresh path.
9. Insert the 'erase' entry ('<Owner> removed content from history', end_rev = head; it does not say what).
10. Set the erasure row: state 'done', erased_ids = R, needle_len, counts, to_epoch.
11. Write audit doc.history_erase without the text.
12. COMMIT.

Phase C (after commit):
- bus.publish(doc_id, −new_epoch). The PgBus payload is an i64 (bus_pg.rs:253, 276-285), so a negative value means reload.
- Best-effort single-flight VACUUM, at most once per 10 min, of doc_updates, doc_snapshots, doc_history_checkpoints, doc_history_baselines, doc_markdown_cache, comments, suggestions, doc_tasks, notifications and audit_events.

FENCING
Writes:
- A writer that was in flight finishes before the FOR UPDATE is granted, and Phase B then reads its rows.
- Writers that arrive later block on the heads row. After commit they read the new content_epoch from their own upsert's RETURNING, roll back and send Fatal. The room evicts itself with no snapshot.
- A snapshot insert's FOR KEY SHARE waits, then re-reads the updated row (READ COMMITTED row-lock recheck) and raises KH002.
- Old-version replicas' rows carry no epoch and get KH002 from the trigger.

Reads (stale rooms):
- PgBus drops a subscriber whose channel is full, so the bus alone is not trusted.
- Every room stores its hydration epoch and checks it in three places:
  (1) the bus reload message;
  (2) its 5 s catch-up tick, whose query also returns heads.content_epoch;
  (3) Rooms::acquire(doc, db_epoch). collab_upgrade reads content_epoch with its ACL lookup, and a room whose epoch is older is evicted and re-hydrated before anyone joins.
- An evicted room closes its sockets with the new code 4410 and sets evicted = true (no snapshot).

Clients:
- GET /api/docs/{id} returns content_epoch, and KnotProvider connects with ?epoch=N.
- The upgrade closes before any sync:
  - with 4410 when N is lower than the current epoch;
  - with 4422 when the parameter is missing and the epoch is > 0. Old bundles already treat 4422 as 'refused' and stop reconnecting (KnotProvider.ts:121-124).
- On 4410 the web client destroys its Y.Doc, refetches the doc, remounts with a fresh Y.Doc and client id, clears ['history', docId], and shows a toast that unsent edits were discarded.

Resurrection guard:
- The durable record is doc_history_erasures.erased_ids, ids only. Rooms load the union at hydration.
- on_inbound refuses with 4422 any update that carries an id in that set with non-Deleted content. It reuses the update_items::decode the nesting prefilter already runs.
- Resends of known ids are also no-ops, because rows are effective updates.
- Typing the same text later creates new ids and is allowed.

NOT REACHED
Other pages (Duplicate and Restore-as-new copies), attachments, boards, titles, and backups, WAL archives and replicas taken earlier. The dialog says this in one sentence.

## former_member

ONE PLACE: users.history_anonymized_at timestamptz NULL (M4)
History tables store user ids only:
- doc_updates.by_user_id
- doc_history_entries.author_id
- meta.accepted_by
- the erasure's requested_by
Those ids are never nulled or rewritten. A rebuild (a BUILDER_VERSION bump or an erase recount) therefore keeps a former member's sessions intact and still attributed to one person. This fixes Proposal 3's flaw of nulling by_user_id.

READ PATH
Names are resolved by one server helper, history_author_view(user_id) in crates/knot-history/src/people.rs. It is used by:
- the history list (sessions and the restore, accept, erase and copy wording);
- GET /api/docs/{id}/contributors (crates/knot-server/src/routes/api/contributors.rs), which feeds the DocByline credit;
- the byline creator, documents.created_by.

When the flag is set, the helper returns {key: null, name: null, former: true, color: neutral}. The UI renders 'Former member' (translated) with a grey avatar. The API never returns that person's user id, email or avatar on these routes. Comments and suggestions keep the real id and display_name, as the spec requires, so returning the id here would let anyone re-identify the person by joining on it. That was the judges' finding against Proposal 1. For people who are not anonymized, key is the user id, which the UI uses for colour and avatar.

ADMIN COMMAND
`knot-server admin anonymize-history --user <uuid|email> --confirm-irreversible`, a new AdminCmd in crates/knot-server/src/admin.rs.
- Instance admins only (operators with the binary and database access), one user per run, across all workspaces.
- It runs `UPDATE users SET history_anonymized_at = now() WHERE id = $1 AND history_anonymized_at IS NULL`.
- It writes the audit event admin.history_anonymize with the user id only.
- There is no command and no API to clear the column.

WHAT DOES NOT CHANGE
Exact times, content, the comments and suggestions they wrote, suggestions.decided_by, audit_events.actor_id, live presence and the account itself. If the person keeps editing, those edits also show as Former member, because the flag is per user. This is the irreversible reading of the spec.

## baseline

M1.1, in two migrations (the Helm pre-upgrade hook), each its own transaction. The statement order matters (as shipped). Neither migration ever waits for a lock on one table while it holds a lock on another app table (the second holds ACCESS SHARE on its own doc_history_meta, which no app touches): each takes its table locks up front in a retry loop (a DO block), where every attempt is a subtransaction that waits at most 100 ms (lock_timeout) for the first lock, takes any further one with NOWAIT, and on lock_not_available rolls back, releasing what it took; the loop sleeps 0.5 s between attempts and raises after 45 s, naming the lock and the sessions holding it, so the Job retries. Postgres cancels a blocking autovacuum only from a waiter's deadlock check, deadlock_timeout into a wait, which these short waits never reach; so when an attempt fails on a table an autovacuum worker holds, the loop waits for that table's lock alone, holding nothing else, for deadlock_timeout + 500 ms, at most every 5 s per worker and table, so a worker that moves on to the other table, or a check that reordered a wait queue (DS_SOFT_DEADLOCK) instead of cancelling, gets another wait. The check cancels the worker, and the wait rolls back at once. A worker is backend_type 'autovacuum worker' or, without pg_read_all_stats (which hides other roles' backend_type), any lock holder with no role, i.e. a background process. A visibly anti-wraparound worker is skipped, since Postgres never cancels one; without pg_read_all_stats its query can't be seen, so its lone wait times out, repeated at most every 5 s. pg_stat_activity is a per-transaction snapshot, so every read is preceded by pg_stat_clear_snapshot(). The give-up message names up to three holders of a conflicting lock, oldest transaction first among those the Job's role can see. NOWAIT on doc_updates stays: a lock_timeout there, while documents is held, lets an edit save's own deadlock check fire inside the window and abort it, losing the batch. After the loop each migration needs only locks it holds, locks on tables it creates, or locks no app transaction conflicts with, so it cannot be part of a deadlock cycle: a page write that read or row-locked documents and then writes it, or an edit save, just waits for COMMIT.

The first, 20261104120000_history_keep_updates:
1. The lock loop: SHARE ROW EXCLUSIVE on documents (100 ms), then SHARE ROW EXCLUSIVE on doc_updates (NOWAIT), the app's order (Duplicate inserts into documents, then doc_updates; a purge deletes from documents and cascades into doc_updates). Waiting for doc_updates while holding documents deadlocked three ways: a page write holding FOR UPDATE on a page (archive, move, restore, purge) blocks an edit save's FK check (FOR KEY SHARE) on that page; the edit save holds ROW EXCLUSIVE on doc_updates; the migration waits for the edit save; the page write's UPDATE or DELETE waits for the migration. Each attempt pauses page writes (ROW EXCLUSIVE on documents) for at most 100 ms; readers and row locks (FOR SHARE, FOR UPDATE, FK checks) pass. Then lock_timeout is 5 s for the rest.
2. Create the doc_updates append-only trigger. From then on, old replicas' GC deletes raise KH001. It comes before the copy, so no delete lands between the copy and COMMIT. Edit saves and every page write that needs ROW EXCLUSIVE on documents (creates, renames, moves, archives, restores, duplicates, purges, template and restriction changes) wait until COMMIT.
3. Record keeps_since = clock_timestamp() in the one-row table doc_history_meta (see ROLLOUT FLAG for why it is exact).
4. Create doc_history_baselines and copy every doc_snapshots row into it with INSERT … SELECT (ACCESS SHARE on doc_snapshots, which no app transaction conflicts with), then add its FK. At most 5 plus 1/day for 30 days per page survive today's GC. The FK takes SHARE ROW EXCLUSIVE on documents, already held, plus ACCESS SHARE and ROW SHARE on it, and its check reads without row locks.

The second, 20261104120001_history_since:
5. The lock loop: ACCESS EXCLUSIVE on documents (100 ms per attempt; each attempt queues all access to documents, page reads and edit saves' FK checks included, for at most that long). In the same subtransaction, ADD COLUMN history_since timestamptz NOT NULL DEFAULT '<keeps_since>' (a constant, so no table rewrite), so existing pages get keeps_since. Then SET DEFAULT now() for new pages, and DROP TABLE doc_history_meta. As the last statement of the first migration, the ACCESS EXCLUSIVE upgrade deadlocked with a page write that had read or row-locked documents (a create's FOR SHARE on the parent, a purge's FOR UPDATE on the archive root) and then waited to write it. It must not come before the first: old replicas' GC deletes edits until the trigger commits. A long transaction on documents (pg_dump, a slow export) can outlast the loop on every Job attempt; the first migration has committed by then, so the upgrade halts with 0.9.8 pods unable to restart until it is re-run (operator docs: Helm chart README).

Why this loses nothing that still exists:
- GC never deleted rows after a page's latest snapshot. The latest baseline plus all later rows is therefore complete.
- Old replicas that keep thinning doc_snapshots afterwards are harmless, because the baselines are already copied.
- Until M1.4 the old drawer keeps reading doc_snapshots unchanged, so there is no visible regression.

M1.3 builder, first visit to an existing page:
- Genesis = the latest baseline plus surviving rows with seq > its snapshot_seq and created_at < history_since. This includes rows with by_user_id NULL from before 2026-10-03. These rows are used only to build genesis and never become credited entries.
- One sealed 'baseline' entry per baseline row: author NULL, ended_at = its created_at, baseline_id set.
- One more baseline entry, 'state at rollout', for the genesis checkpoint, but only when its pm_json differs from the latest baseline's.
- Pages with no snapshot at rollout were never pruned. They still take history_since = the rollout, and their pre-rollout rows fold into genesis, because rows before 2026-10-03 have no author.

Timeline:
- After all log entries comes the section 'Earlier versions (author unknown)', newest first, then the line 'Complete history starts <history_since>' in the viewer's time zone.
- Pages created after rollout (created_at >= history_since) show neither.
- Each baseline entry can be viewed (its state; Show changes disabled) and restored, in place or as a new page.
- An erase rewrites baselines too and sets redacted_at.

## verified_apis

- yrs 0.27.3 is pinned in Cargo.lock:5538-5539. Paths below are relative to ~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/yrs-0.27.3/src.
- TransactionMut::before_state() / after_state() / insert_set() / delete_set(): the IdSets of what this transaction integrated or newly deleted. transaction.rs:532-565.
- TransactionMut::encode_update_v1() / encode_update(): store.write_blocks_from(before_state) plus this transaction's delete_set, so the effective update omits pending content. transaction.rs:597-628.
- TransactionMut::apply_update(Update) trims blocks the doc already knows (known_state/exclude) before integrating, then merges pending structs and retries pending deletes. transaction.rs:820-860.
- TransactionMut::commit() runs GCCollector::collect unless skip_gc and then squashes the delete set. transaction.rs:1031-1060.
- TransactionMut::gc(Option<&IdSet>) forces GC even on a skip_gc doc. transaction.rs:1310-1312.
- ReadTxn::snapshot() is at transaction.rs:41-47, encode_state_from_snapshot at :51-57, encode_state_as_update_v1 at :139 and root_refs at :167.
- ReadTxn::has_missing_updates() is true when store.pending or store.pending_ds is set. transaction.rs:285-288.
- Store::pending_update() and pending_ds() are public (store.rs:87-105). Store is exported at lib.rs:681, and ReadTxn::store() is a public trait method (transaction.rs:32).
- Store::encode_state_from_snapshot returns Err(Error::Gc) unless skip_gc (store.rs:152-163). write_blocks_to calls blocks.find_index(clock - 1) (store.rs:165-196), hence the zero-clock strip helper.
- GC: GCCollector::collect, collect_all and mark_in_scope are at gc.rs:12-58. Item::gc(collector, parent_gc=false) replaces the content with ItemContent::Deleted(len) and keeps the item, id, origins and parent, while nested children are GC'd (block.rs:1556-1567). ItemContent::Deleted integrates as deleted (block.rs:859-863).
- String item clock length is UTF-16: self.len = content.len(OffsetKind::Utf16) at block.rs:699, and ItemContent::len at block.rs:1770-1777. Options::default offset_kind is Bytes (doc.rs:526-529), which matters for index arithmetic only.
- Text::diff_range(&mut TransactionMut, hi: Option<&Snapshot>, lo: Option<&Snapshot>, f) splits items by snapshot first (types/text.rs:431-450). In DiffAssembler::process (text.rs:569-680), items seen in neither snapshot are skipped, Removed means not visible in hi, Added means visible in hi and not in lo, a new chunk starts per changed item carrying YChange{kind, id}, and Format items apply only when seen in hi.
- Text::diff (no snapshots) is at text.rs:420-428. XmlTextRef implements Text (types/xml.rs:576).
- UndoManager::with_options(Options) is at undo.rs:146-166 and takes init_undo_stack from Options (undo.rs:791-813). expand_scope(&Doc, &T: AsRef<Branch>) is at undo.rs:168-198. undo_blocking() is at undo.rs:543. try_process deletes in-scope live insertions and redoes in-scope deletions that are not in insertions (undo.rs:640-720). StackItem::new(doc_id: Uuid, deletions, insertions) is at undo.rs:894. The Meta blanket impl (undo.rs:106-113) means () works. Uuid = Arc<str> (lib.rs:722). Doc::guid is at doc.rs:216.
- Options{client_id, guid, offset_kind, skip_gc, ..} are public fields (doc.rs:511-540). Doc::with_options is at doc.rs:188 and Doc::with_client_id at doc.rs:183.
- IdSet::new/insert(ID,len)/insert_range/contains/is_empty/merge/merge_with/diff/diff_with/intersect/iter/get and Encode/Decode are at id_set.rs:215-360.
- Snapshot{delete_set, state_map}, Snapshot::new and Encode/Decode are at state_vector.rs:174-210. StateVector get/set_min/set_max/iter/contains_client are at state_vector.rs:21-101. Snapshot is exported at lib.rs:674.
- Branch::id() -> BranchID::Nested(ID) | Root is at branch.rs:270-278. BranchID::get_branch(txn) is at branch.rs:914-940. XmlOut::id() is at types/xml.rs:104.
- XmlFragment::children() iterates BlockIter, which yields non-deleted children only (types/xml.rs:1064, 1214-1232). That is why the diff walks a union doc. XmlFragment::successors is at xml.rs:1179.
- Xml::get_attribute/attributes are at xml.rs:1032-1039, remove_attribute/insert_attribute at xml.rs:994-1002, and XmlElementRef::tag at xml.rs:280. AsPrelim for XmlElementRef/XmlTextRef/XmlFragmentRef is at xml.rs:357, 641 and 878. Out::to_json is at out.rs:203.
- Update::insertions(include_deleted) is at update.rs:248, Update::delete_set at update.rs:267 and Update::state_vector at update.rs:206. alt::merge_updates_v1 (for later compaction) is at alt.rs:15.
- knot: Engine::apply_update_changed and apply_update_measuring (crates/knot-crdt/src/engine.rs:120-186) already compare pending_fingerprint before and after and commit the transaction, so they are the place to also return encode_update_v1(), falling back to raw bytes when pending changed.
- knot: gc.rs:38-48 logs a warning and continues when delete_up_to fails, so a raising KH001 trigger is safe against old replicas. INSERT_UPDATES_SQL with the contrib CTE locks contributor rows in by_user_id order (crates/knot-storage/src/updates_store.rs:60-106).
- knot: KnotProvider closes to status 'refused' without reconnecting on 4422 (web/src/features/editor/KnotProvider.ts:121-124). The server already uses 4422 (crates/knot-server/src/ws_writer.rs:59).
- knot: suggestion notification excerpts are truncated to EXCERPT_CHARS = 140 (crates/knot-server/src/suggestion_notifications.rs:30-44).
- sqlx-core 0.9.0 honours a leading '-- no-transaction' in a migration (sqlx-core-0.9.0/src/migrate/source.rs:234), which allows CREATE INDEX CONCURRENTLY.
- @tiptap/core 3.31.3: addGlobalAttributes is at dist/index.d.ts:938 and enableContentCheck/onContentError at index.d.ts:1738-1768.
- @tiptap/y-tiptap 3.0.9: hashedMarkNameRegex and yattr2markname are at dist/y-tiptap.js:1490-1494 and are mirrored in pm_json. yXmlFragmentToProsemirrorJSON (y-tiptap.js:2464, exported at :2939) is the golden-test reference. renderSnapshot is deliberately not used.

## risks

- **Restore by undo.** ItemPtr::redo can return None on map-entry conflicts, or fail to redo a parent. In that case verification against target_json catches it, and the lossless as_prelim fallback runs, but comment and suggestion anchors are lost and the entry is marked restore_fallback. Property tests are needed: random multi-author histories × random targets, asserting pm_json equality, convergence of two connected peers, and that anchors on untouched text still resolve.
- **Erase re-derivation.** Content left pending is the subtle case. The rewrite set must include rows that drained pending content from a rewritten row. The unlocked Phase A verification guards this: sv equality, pm_json at each checkpoint, no String in R, 0 matches. Any mismatch aborts the erase and leaves storage untouched.
- **Erase cost on very long pages.** Phase A replays the whole history (preview, then confirm). Phase B holds the heads row lock only for the tail catch-up and the writes, but writers of that page stall meanwhile and may evict their room. Mitigations: erase budget and erase duration metric, async job with a heartbeat, 409 history.erase_busy.
- **Erase scope.** Matching works only within one XmlText. Text split by an inline node, attribute values (a link href), boards and files are not erased. The dialog must say this. Truncated excerpts are handled heuristically: edge fragments of 3 or more characters are replaced.
- **Hot-path changes in M1.2.** Each flush becomes an explicit transaction with a heads-row update first. Flushes of the same page from different replicas serialize on that row. Watch writer flush latency; two-replica tests must show no deadlock against the contrib CTE.
- **Old-replica rows during the M1.2 rollout.** They take the trigger path, one heads update per row. That is slower and bloats the heads row briefly, and their rows have source NULL, so they show as plain edits. Pre-M1.2 writers still drop failed batches; any content lost that way appears as authorless gap entries, never misattributed.
- **Legacy numbering.** ensure_legacy_numbered runs an UPDATE once per page at first hydration. Pages that had no snapshot can have many legacy rows (GC never pruned them), which makes that first room boot slower.
- **Storage is unmeasured.** Rows are roughly one per keystroke transaction, kept forever, and backups and PITR grow with them. Mitigations: history-stats CLI, table-size gauges, Helm sizing notes, and the optional M5 compaction that keeps entry boundaries exact.
- **Replay cost and memory for view and restore on huge pages.** Bounded by checkpoints (max(64 KiB, state size)), a per-replica semaphore, KNOT_HISTORY_REPLAY_MAX_BYTES (past it: 409 history.too_large), a 16 MiB stack, and the LRU.
- **Schema drift.** Old versions can contain node types or attributes the current schema lacks. pm_json maps unknown tags to a placeholder, and the viewer uses enableContentCheck with a 'can't be shown' fallback. No Y data reaches the client, so drift cannot damage a page.
- **Undoing a restore is approximate.** Sessions are per author, as the spec asks. If another person's session spans a restore, 'restore the entry before it' is not exactly the pre-restore state. The row's meta keeps target_rev and the restore row's rev, so an exact 'Undo restore' (target = rev − 1) can be added without new data. Confirm with the user whether that belongs in v1.
- **Erase availability during rolling deploys.** Erase is refused until every live replica reports ERASE_FENCE. Releases must not skip M1.2, which is where epochs on writes begin (CHANGELOG upgrade note).
- **Access follows current access.** Unrestricting a page or granting access exposes its whole past, and erase is the only remedy. AclCache invalidation is eventually consistent across replicas, as it is for live content.
- **Anonymization reach.** The person's id stays in comments, suggestions, audit_events.actor_id and backups, by design. History APIs never return it.

## milestones

- **M1.1 Stop the loss** (patch release, ships first, backend and migration only).
- Two migrations, run as the Helm pre-upgrade hook:
  - the raising append-only trigger on doc_updates (KH001; the FK cascade and knot.history_rewrite are allowed);
  - doc_history_baselines, filled from doc_snapshots;
  - documents.history_since, in the second migration: the moment the first made deletes impossible for existing pages, DEFAULT now() for new ones (see baseline);
  - knot_replicas, with the heartbeat task (moved to M3; see the deviation note at the top).
- gc.rs stops deleting doc_updates (UpdatesStore::delete_up_to is removed) and runs under pg_try_advisory_lock. Snapshot thinning is unchanged, so today's drawer still works.
- Ops: knot_history_table_bytes gauge (moved to M1.3; see the deviation note at the top), the `knot-server history-stats` CLI, the e2e reset list, and the migrations test (a direct DELETE raises; purge still cascades).
- Old replicas' deletes fail from the moment the first migration commits. No UI change.
- **M1.2 Complete, ordered, labelled log** (backend only).
- Schema:
  - doc_history_heads, created and seeded in trigger-first order;
  - after ADD COLUMN rev, re-create the append-only trigger, not only its function: `CREATE OR REPLACE FUNCTION doc_updates_append_only()` with rule (b), and `CREATE OR REPLACE TRIGGER doc_updates_append_only BEFORE DELETE OR UPDATE OF update_bytes, doc_id, rev ON doc_updates FOR EACH ROW ...`. M1.1's trigger lists no rev column, so a function-only replacement never fires on UPDATE OF rev;
  - the doc_updates_assign_rev trigger for old-replica rows;
  - rev, source, meta and epoch columns on doc_updates;
  - the UNIQUE (doc_id, rev) index, built CONCURRENTLY in a no-transaction migration;
  - snapshot_rev and epoch on doc_snapshots, with the epoch trigger.
- Writer:
  - INSERT_UPDATES_SQL becomes an explicit transaction that allocates revs on the heads row first;
  - effective-update persistence and fan-out, with the raw fallback when content is pending;
  - ordered retry, Fatal on KH001/KH002, and eviction when the buffer overflows.
- Room:
  - in_flight and applied_through_rev;
  - snapshots only when in_flight == 0 and there is no gap;
  - catch-up by rev (fixes room.rs:762-764);
  - ensure_legacy_numbered.
- Every server-side writer is tagged: restore, import, template, task, suggestion with accepted_by, and copy.
- Tests:
  - injected flush failures lose nothing;
  - two replicas writing concurrently produce gapless, commit-ordered revs with no deadlock;
  - every snapshot is a subset of the replayed log;
  - the effective update applied equals the raw update applied.
- **M1.3 Builder and read API** (crates/knot-history).
- Replay engine and checkpoints (max(64 KiB, state size)).
- Canonical attribution, including the parked-claims map.
- Sessions with the 20-min seal; baseline and genesis entries; gap detection; word counts.
- GET /history with keyset pagination, behind require_editor.
- history_author_view, used by the list API and the contributors API. Former member rendering is wired but inert until M4.
- Metrics, Helm sizing notes, and boot-cache thinning to 3 snapshots per page.
- The old drawer stays.
- **M1.4 Timeline UI, version view and Show changes.**
- pm_json with the union-doc diff (S, S_lo).
- The /entries/{id}/view endpoint with LRU, ETag and no-store.
- The widened two-column drawer: day headings, wording per kind, the 'Earlier versions' section and the 'Complete history starts' line.
- The read-only viewer built from createSchemaExtensions plus HistoryDiffExtension; the stacked mobile layout.
- Golden tests comparing pm_json with y-tiptap's yXmlFragmentToProsemirrorJSON.
- The old snapshot-list UI is retired, but the Markdown restore route stays until M2.
- **M2 Restore here and Restore as new page.**
- ExportForRevert and ApplyRevert with content_version and Stale retry.
- The UndoManager revert on a skip_gc scratch doc with a fresh client id, verified against target_json, with the as_prelim fallback (restore_fallback).
- Persist before fan-out; the restore entry.
- Restore-as-new through duplicate::run with a content override, a version title, and the copy source/meta row.
- Remove the Markdown restore, preview and seq-list routes; update e2e/flows/history.spec.ts.
- Property tests for the revert.
- **M3 Erase from history.** Requires every replica to report ERASE_FENCE.
- knot_replicas and its heartbeat task (moved here from M1.1). Replicas older than M3 never write a heartbeat, so the "every replica seen in the last 3 min" guard cannot see them, and the erase fence must not rely on it alone. For example, refuse erase until knot_replicas has shown only feature_level >= ERASE_FENCE for longer than a rolling upgrade plus the maximum pod termination grace can take, so every pre-M3 pod is gone.
- Set knot.history_rewrite only with `SET LOCAL` or `set_config('knot.history_rewrite', 'on', true)`, never session-wide: pooled connections are reused, and a session-wide flag would let the next pool user rewrite history. Test that the flag does not reach the next user of the same pooled connection (M1.1 pins this for SET LOCAL in migrations_apply.rs).
- The matcher, the preview endpoint and the dialog: counts, the short-text warning, the open-elsewhere warning, and the sentence on what is not reached.
- The async erase job: heartbeat and sweeper; Phase A re-derivation with verification; Phase B as one fenced transaction under knot.history_rewrite.
- Side-table redaction, including excerpt edges and JSON string leaves; markdown cache and search regeneration; the erase entry; audit.
- Read-side fences: the bus −epoch reload, the epoch check on the catch-up tick, Rooms::acquire epoch eviction, and ?epoch on the WebSocket.
- Close codes: 4410 for stale epochs with a client remount; 4422 when the epoch parameter is missing.
- The erased-id ingress guard; best-effort VACUUM.
- e2e: erase with a second tab open and with an offline tab.
- **M4 Former member.**
- users.history_anonymized_at.
- Flip history_author_view to anonymized output: no id, name 'Former member', grey avatar. This covers the timeline, the restore, erase, accept and copy wording, the contributors API and the byline creator.
- `knot-server admin anonymize-history` (irreversible, audited).
- Small and independent; can ship any time after M1.3.
- **M5 (conditional) Compaction.**
- Only if history-stats data after about 30 days demands it.
- Merge one author and source's rows between consecutive entry end_revs with merge_updates_v1, under knot.history_rewrite.
- Every boundary state and IdSet stays exact.

## rationale

Proposal 3 (Risk-first) scored highest across all three judges (8, 8, 7), so it is the base. Its core strengths are kept:
- A commit-ordered, gapless per-page rev. It removes the seq-versus-commit problem by construction, needs no settle delay, and fixes the room's late-seq skip.
- Exact 'Show changes' through delete-set snapshots (S_lo). This needs no client-id assumptions.
- Server-rendered pm_json, so no Y data ever reaches the client. The same pm_json gives canonical equality for verification.
- Restore planned outside the room actor with optimistic concurrency, yrs's own undo engine, verification and a lossless fallback.
- Gap detection.
- A raising append-only trigger.

Every fatal flaw the judges found in Proposal 3 is removed:
- **erase_lock with no lease.** Replaced by Proposal 1's idea of a single fenced transaction. All mutation happens in one transaction under the heads-row FOR UPDATE, so a crash rolls back and leaves nothing stuck. Job progress has a heartbeat and a sweeper.
- **Stale rooms serving erased text after a missed bus message.** Rooms re-check their epoch on the 5 s tick (from Proposal 2) and at Rooms::acquire (from Proposal 1), in addition to the bus.
- **The hand-written v1 byte redactor.** Replaced by Proposal 1's re-derivation through yrs itself: delete at birth, then commit GC, then encode_update_v1. The pending-content hole the judges found in Proposal 1 is closed by widening the rewrite set and by Proposal 3's full verification before commit.
- **Nulling by_user_id for anonymization.** Replaced by Proposal 1's single users column, read through one helper that never returns the id. That also fixes Proposal 1's re-identification leak.
- **Checkpoints every 1000 rows or 1 MiB.** Replaced by Proposal 1's policy of max(64 KiB, state size).
- **The rev trigger's deadlock with the contrib CTE and its per-row bloat.** New code allocates revs with one heads update as the first statement of the flush, which fixes the lock order. The trigger only serves old replicas, and it is created before legacy rows are counted.

Grafted from the other proposals:
- From Proposal 1:
  - effective-update persistence, which keeps the log clean of resends and erased ids;
  - the CONCURRENTLY index in a no-transaction migration;
  - measurement before tuning.
- From Proposal 2:
  - the knot_replicas heartbeat gate before erase;
  - the epoch tick check;
  - the zero-clock strip helper;
  - pending-owner crediting (as the parked-claims map).
- Judge 2's no-store caching and excerpt-edge redaction.

The result is never worse than "the log is the truth". Every derived artifact can be rebuilt from the log, and every destructive operation is verified before it commits.

## rejected_alternatives

- **Proposal 2's lifetime skip_gc history doc H per page, made authoritative after an erase.** A single derived blob would become the only truth, with unbounded memory, TOAST and WAL churn, and no way to recover from a bug.
- **Erase that empties whole rows to 0x0000 (Proposal 2).** It destroys the non-secret content in those rows.
- **Attribution through yrs client-id bindings and awareness claims (Proposal 2).** Any editor can forge it, because client ids are visible to peers.
- **Show changes by lowering the state vector (Proposal 2's (S, Q)).** It assumes an author's inserts form a clock suffix, which the shared room client id breaks.
- **Rendering with y-tiptap renderSnapshot / ProsemirrorBinding (Proposal 2).** Its catch paths delete Y items, and only a node-count heuristic would notice.
- **Proposal 1's client-side snapshot walker.** It is buildable, but server pm_json keeps Y data off the client, is testable in Rust, and doubles as the restore verifier.
- **Ordering by (created_at, seq) with a 120 s settle delay (Proposal 1).** statement_timeout does not bound COMMIT, and checkpoints can hide late rows.
- **A silent RETURN NULL delete trigger (Proposal 1).** A missed delete path would appear to succeed.
- **Restore by a hand-written LCS tree diff with byte-offset text edits (Proposal 1).** It is the most custom code on the most dangerous path, and moved blocks lose their anchors.
- **Proposal 2's restore that loads the lifetime H inside the room actor.** It stalls live editing.
- **Proposal 3's hand-written redact_update_v1 byte re-encoder.** It is tied to the yrs v1 format and risks permanently corrupting history.
- **Proposal 3's two-phase erase_lock that blocks room spawns.** It has no lease and gets stuck forever after a crash.
- **Proposal 3's history_actors indirection with by_user_id nulled.** Rebuilds then lose the person's identity and merge every former member into one actor.
- **An erase confirm that runs inline in the HTTP request while holding documents FOR UPDATE (Proposal 1).** It blocks every FK insert and hits ingress timeouts.
- **Proposal 2's pre-commit byte check, position(needle) = 0, across all storage.** It contradicts the match scope: a secret that also appears in a link href would always roll back.
- **Closing every open session when a special entry happens (Proposal 2).** It changes the spec's per-author session rule. The exact pre-restore rev is kept in meta instead.
- **Keeping every snapshot forever, and compacting raw rows into per-entry states now.** The first lets full-state copies dominate storage. The second is premature before measurement and would lose detail below session level that later blame might need.
- **Keeping the Markdown round-trip restore.** It is lossy, destroys anchors, and has corrupted pages before (0.2.1, #52).

## open_spikes

- **Postgres append-only trigger.** In migrations_apply.rs, prove that the BEFORE DELETE OR UPDATE trigger:
- raises KH001 for a top-level DELETE and for an UPDATE of update_bytes;
- allows the purge cascade (DELETE FROM documents, including parent_id cascades) via NOT EXISTS(parent) / pg_trigger_depth() > 1;
- allows the rev-only legacy update;
- does not affect TRUNCATE in reset.ts.
- **Rev allocation under concurrency.** Two connections flush the same page, each running the heads-first upsert followed by INSERT … UNNEST with the contrib CTE, while a third connection takes the old-replica trigger path. Prove:
- revs are gapless and visible in commit order;
- there are no deadlocks, including the same user on two replicas;
- writer flush latency under load.
- **Migration ordering for doc_history_heads.** CREATE TRIGGER before the count(*) seed blocks concurrent old-replica inserts until commit, so legacy_rows is exact.
- **Row-lock recheck.** A snapshot insert trigger doing SELECT … FOR KEY SHARE on the heads row must wait behind the erase's FOR UPDATE and, after commit, see the bumped content_epoch (READ COMMITTED row-lock recheck). The writer's heads upsert likewise must return the new epoch after waiting.
- **Effective update equals the raw update.** For random multi-client histories, including reconnect SyncStep2 resends, applying txn.encode_update_v1() taken after commit must give the same state as applying the raw bytes. The pending fallback (raw bytes when pending_fingerprint changes) must never lose structs or deletes. Also confirm the yjs client accepts the effective bytes on fan-out.
- **encode_state_from_snapshot on a mixed skip_gc doc.** It is used on a skip_gc doc seeded from a GC'd checkpoint, so the doc contains ItemContent::Deleted and GC blocks. Check:
- it returns Ok;
- the S_union document walks correctly;
- diff_range(hi = S, lo = S_lo) yields exactly the author's Added and Removed chunks with interleaved second-author edits (the spec's 09:00-09:40 / 09:10-09:20 case);
- the zero-clock strip helper avoids the clock - 1 panic.
- **pm_json parity.** Compare pm_json against y-tiptap yXmlFragmentToProsemirrorJSON over the repo corpora: tables, task items, mentions, board and image embeds, hashed mark names, line breaks in list items (#52 shapes).
- **UndoManager revert.** On a skip_gc scratch doc built from H_T_bytes plus the live GC'd state, with StackItem::new(guid, ds_now − ds_T, sv_now − sv_T) and expand_scope('default'):
- pm_json equals the target across random histories;
- attribute reverts (heading level, task checked, table widths) redo;
- comment RelativePositions on untouched text still resolve on the live doc after R is applied;
- origins in R resolve against the GC'd live doc;
- measure how often redo returns None and forces the fallback.
- **Erase re-derivation.** Replaying with a delete-only update ([0x00] ++ IdSet::encode_v1) for R ∩ insert_set in the same TransactionMut, then committing, must:
- keep ids stable and leave R items as ItemContent::Deleted when the parent is alive;
- leave later rows integrating with no pending;
- give a state vector identical to the original;
- handle legacy raw rows that parked pending content when the rewrite set includes the draining rows.
Also confirm the dialog's counts on emoji and CJK text, where clock offsets are UTF-16.
- **Character-to-id mapping for erase.** diff_range(hi = now, lo = Snapshot::default()) on a GC'd replay doc must split chunks per item with correct start ids, including items merged by commit squashing, so every character maps to (client, clock + UTF-16 offset).
- **Throughput.** Measure yrs replay rows/s for typical ~150 B rows and for large pastes. These numbers set KNOT_HISTORY_REPLAY_MAX_BYTES, KNOT_HISTORY_ERASE_BUDGET and confirm the checkpoint policy.
- **Real-instance measurement before M1.3 tuning.** Measure avg(octet_length(update_bytes)), rows per page per day (p50, p95, max), and state sizes. These decide the checkpoint thresholds and whether M5 compaction is ever needed.
- **Excerpt fields.** Confirm the exact truncation lengths of every field holding page text: suggestions.context_before, comments.anchor_text, doc_tasks.text, and notification data keys. This sets the scope of the edge-fragment redaction.