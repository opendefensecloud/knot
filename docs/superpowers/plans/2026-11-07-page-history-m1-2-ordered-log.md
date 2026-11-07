# Page history M1.2: complete, ordered, labelled log (implementation plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From release 0.9.10 on, every change to a page lands in `doc_updates` exactly once, numbered by a per-page gapless `rev` in commit order, tagged with what made it (`source`, `meta`) and the content epoch its writer saw, and nothing a room accepted is dropped from the log (the one exception, an update yrs rejects part-way, is kept out of the log and out of every snapshot; see Rulings).

**Architecture:** A per-page head row (`doc_history_heads`) is locked by every flush, right after the page row, and hands out `rev`s, so rev order is commit order. New code writes revs itself; a `BEFORE INSERT` trigger numbers rows from 0.9.9 pods during the rollout. Stamped snapshots are immutable, so a 0.9.9 snapshot write can't overwrite one. The writer keeps a failed batch and retries it in order, and evicts its room on a permanent error, an overflowing buffer or a server-side edit that missed its deadline, instead of dropping edits or freezing the room; after an eviction or a graceful shutdown it still writes the edits it holds, and a new room for that page waits for that drain. The room persists and fans out the *effective* update (what the apply changed), tracks `applied_through_rev`, catches up by rev, and writes a snapshot only when it holds nothing beyond the page's pre-upgrade base and the log up to the snapshot's rev. Every server-side writer tags its rows. Backend only: no UI, no API change.

**Tech Stack:** Rust (yrs 0.27.3, sqlx 0.9, tokio), Postgres 18 (PL/pgSQL triggers, `CREATE INDEX CONCURRENTLY`), Playwright reset list (TypeScript).

**Spec:** [docs/superpowers/specs/2026-11-04-page-history-design.md](../specs/2026-11-04-page-history-design.md), technical architecture [docs/superpowers/specs/2026-11-04-page-history-architecture.md](../specs/2026-11-04-page-history-architecture.md) (sections `storage_model`, `entry_builder` → WRITE PATH and ROOM CHANGES, `attribution` → TRUSTWORTHY LOG and SERVER-SIDE EDITS, `risks`, `milestones` → M1.2, `open_spikes`). Format and lessons: [the M1.1 plan](2026-11-04-page-history-m1-1-stop-the-loss.md) and its migrations `migrations/20261104120000_history_keep_updates.sql`, `migrations/20261104120001_history_since.sql`.

## Global Constraints

- Release 0.9.10. Upgrades must not skip it: epochs on writes begin here (architecture, risks). Never edit an applied migration (`20261104*` included): sqlx checks checksums.
- Migrations, in this order, each recorded on its own (all run in the Helm `pre-upgrade` Job while 0.9.9 pods keep serving and writing):
  1. `migrations/20261107120000_history_log_columns.sql` (transaction): creates the lock helper, then `ACCESS EXCLUSIVE` on `doc_updates`, `doc_snapshots`; adds columns (metadata only).
  2. `migrations/20261107120001_history_heads.sql` (transaction): `SHARE ROW EXCLUSIVE` on `doc_updates` only; heads table **without** its FK, `doc_updates_assign_rev` trigger, seed, re-created append-only trigger. It does not touch `doc_snapshots`: 0.9.9 rooms write snapshots inline in their actor loop, so locking it for the seed would freeze those rooms, not just delay saves.
  3. `migrations/20261107120002_history_heads_fk.sql` (transaction): `SHARE ROW EXCLUSIVE` on `doc_history_heads`, `documents`, `doc_snapshots` (one helper call, that order: the helper waits up to 100 ms only for `tables[1]` and asks for the rest with `NOWAIT`, and `doc_history_heads` is the busy one, since every in-flight 0.9.9 save holds ROW EXCLUSIVE on it through `doc_updates_assign_rev`; waiting there queues new saves behind the request so the in-flight ones finish, as migration 2's wait on `doc_updates` does, while `documents` is contended only by page writes; the helper never waits while holding anything, so this table order does not touch deadlock safety, and the row-lock order below is unchanged; review round 5); deletes orphan heads rows, adds the FK, creates `doc_snapshots_epoch` function and trigger. Nothing is counted here, so the pause is one pass over the heads rows. During that pass page writes, edit saves and snapshot writes all wait: every 0.9.9 edit save goes through `doc_updates_assign_rev`, whose heads upsert needs ROW EXCLUSIVE on `doc_history_heads`, so each waiting save holds a pool connection, ROW EXCLUSIVE on `doc_updates` and a `documents` KEY SHARE, and on 0.9.9 pods an open page waiting on a task toggle, accept, import or snapshot is held up too.
  4. `migrations/20261107120003_history_rev_index.sql`: first line exactly `-- no-transaction`, then optional `--` comment lines that contain no `;`, then exactly one statement: `CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS doc_updates_doc_rev_uq ON doc_updates (doc_id, rev) WHERE rev IS NOT NULL;` (sqlx sends the file as one simple query; a second statement would put CIC in an implicit transaction block and fail). Its explanation lives in migration 5's header.
  5. `migrations/20261107120004_history_rev_index_check.sql` (transaction): if `doc_updates_doc_rev_uq` is missing or `NOT indisvalid` (a CIC aborted by the deadlock detector or cancelled leaves it invalid, and step 4's `IF NOT EXISTS` then skips it on the re-run; a Job killed mid-CIC usually does not stop the build: the backend does not notice its client is gone, `client_connection_check_interval` defaults to 0, so the build keeps running in Postgres, still holds sqlx's session advisory lock, and the index becomes valid if it finishes; the retried Job's migrator meanwhile waits on that advisory lock with a live snapshot, which can deadlock with the orphaned build's wait-for-older-snapshots phase, and whichever side the detector aborts, the next run converges), take `ACCESS EXCLUSIVE` on `doc_updates` with the helper, `DROP INDEX IF EXISTS doc_updates_doc_rev_uq`, and `CREATE UNIQUE INDEX doc_updates_doc_rev_uq ON doc_updates (doc_id, rev) WHERE rev IS NOT NULL`. Every holder of a lock on the index also holds one on the table, so the helper's retry loop covers the index too and nothing waits for a lock outside it. Otherwise it takes no lock. In the repair case page loads and saves both pause for one scan of `doc_updates` (the partial index holds only rows written since migration 2).
- Lock order, as the app takes locks: `documents` (row, KEY SHARE) → `doc_history_state` (row; table created in M1.3) → `doc_history_heads` (row) → `doc_updates` → `doc_contributors` → `doc_snapshots`. The rule: **the page row's KEY SHARE (`SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE`) is the first lock of any transaction that locks a page-scoped history row (`doc_history_state`, `doc_history_heads`, or the page's `doc_updates` rows).** Every M1.2 path (append, trigger-path insert, snapshot write, legacy numbering) follows it, so a purge (FOR UPDATE on `documents`, then the cascade into every table with an `ON DELETE CASCADE` FK to it: `doc_history_heads`, `doc_updates`, and from M1.3 `doc_history_state`) makes writers wait instead of forming a cycle. The rule binds later milestones too, by name: M1.3's builder runs it before its claim of `doc_history_state` (`SELECT … FOR UPDATE SKIP LOCKED`) and before inserting a missing `doc_history_state` row, so before the heads row it clears `dirty` on; M3's erase runs it before Phase B step 2's first SELECT (`SELECT … FROM doc_history_state WHERE doc_id = $1 FOR UPDATE`), so before its `doc_history_heads … FOR UPDATE` and before its step 5 snapshot insert, which takes the page row's KEY SHARE again through `doc_snapshots_epoch` and the FK. Taking the KEY SHARE just before the heads row, after `doc_history_state`, deadlocks with a purge (it holds the page row and waits on the `doc_history_state` row; the transaction holds that row and waits on the page row), and nothing retries an erase. The architecture's "doc_history_state → heads" order (entry_builder BUILDER) still holds after the page row. A migration never waits for one table while holding another.
- Lock helper (a port of the M1.1 loop, `20261104120000_history_keep_updates.sql:74-164`, which stays untouched): `CREATE OR REPLACE FUNCTION knot_lock_tables(who text, tables regclass[], lock_mode text, give_up_after interval DEFAULT '45 seconds') RETURNS void LANGUAGE plpgsql`. `lock_mode` must be one of `'SHARE'`, `'SHARE ROW EXCLUSIVE'`, `'ACCESS EXCLUSIVE'` (else `RAISE EXCEPTION`). Each attempt is a subtransaction: `lock_timeout` 100 ms for `tables[1]`, `NOWAIT` for the rest (`EXECUTE format('LOCK TABLE %s IN %s MODE', ...)`); on `lock_not_available` it rolls back, releasing all. Then: lone wait on an autovacuum worker holding the missing table, as M1.1 (`deadlock_timeout` + 500 ms, at most every 5 s per pid and table, anti-wraparound skipped, `pg_stat_clear_snapshot()` before every `pg_stat_activity` read, private SQLSTATE `KHLW1`), except that the lone wait always requests `SHARE UPDATE EXCLUSIVE`, whatever `lock_mode` is: it conflicts with autovacuum's own lock, so the waiter's deadlock check still cancels the worker, and it blocks neither readers nor writers (in the requested `ACCESS EXCLUSIVE` it would stop every page load for about 1.5 s per lone wait); sleep 0.5 s; after `give_up_after` raise `'<who>: could not get <mode> on <table> within <n> s (<a> attempts, <l> lone waits); held by <holders>. The migrate Job will retry.'`, holders = up to three sessions holding a *conflicting* granted lock on that table, oldest transaction first, formatted as in M1.1. Conflicting modes: `SHARE` ⇐ RowExclusive, ShareUpdateExclusive, ShareRowExclusive, Exclusive, AccessExclusive; `SHARE ROW EXCLUSIVE` ⇐ those plus Share; `ACCESS EXCLUSIVE` ⇐ every mode. On success: `set_config('lock_timeout', '5s', true)` and `RAISE NOTICE '<who>: locked <tables> after <a> attempt(s), <l> lone wait(s)'`. Migrations call it as `SET LOCAL lock_timeout = '5s'; SELECT knot_lock_tables(...);`.
- `doc_updates` columns: `rev bigint NULL`, `source text NULL`, `meta jsonb NULL`, `epoch int NULL`; `CONSTRAINT doc_updates_source_check CHECK (source IN ('edit','task','suggestion','restore','restore_fallback','import','template','copy')) NOT VALID`. `doc_snapshots`: `snapshot_rev bigint NULL`, `epoch int NULL`.
- `doc_history_heads(doc_id uuid PRIMARY KEY, head_rev bigint NOT NULL DEFAULT 0, legacy_rows bigint NOT NULL DEFAULT 0, content_epoch int NOT NULL DEFAULT 0, dirty bool NOT NULL DEFAULT true, last_write_at timestamptz NULL)`; `CREATE INDEX doc_history_heads_dirty_idx ON doc_history_heads (last_write_at) WHERE dirty`; FK (migration 3) `doc_history_heads_doc_fk FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE`. Seed, one row for every page, including pages whose rows the pre-M1.1 GC pruned (M1.3's builder finds its work only through `doc_history_heads WHERE dirty`, and writes a page's baseline entries on its first visit): `INSERT INTO doc_history_heads (doc_id, head_rev, legacy_rows, last_write_at) SELECT d.id, count(u.seq), count(u.seq), now() FROM documents d LEFT JOIN doc_updates u ON u.doc_id = d.id GROUP BY d.id` (`last_write_at = now()`, the migration's transaction start, so seeded rows are never NULL and sort before every page written after the upgrade in M1.3's `WHERE dirty ORDER BY last_write_at`; not `max(u.created_at)`, which would force a heap read of every row while saves wait; the count can use an index-only scan of `doc_updates_by_doc_idx (doc_id, seq)`, but nothing forces that plan, and pages the visibility map does not mark all-visible are read from the heap anyway, so the upgrade notes size the pause by `pg_relation_size('doc_updates')`, not by the index), after `CREATE TRIGGER doc_updates_assign_rev` (whose `SHARE ROW EXCLUSIVE` is already held, so no row slips between count and trigger). Reading `documents` takes only ACCESS SHARE (no FK exists yet, so no row lock either); a page created after the seed's snapshot gets its heads row with its first row, and a page purged meanwhile leaves an orphan that migration 3 deletes.
- `doc_updates_assign_rev()` (`BEFORE INSERT ON doc_updates FOR EACH ROW`):
  - `NEW.rev IS NULL` (a 0.9.9 pod, a raw insert): first `PERFORM 1 FROM documents WHERE id = NEW.doc_id FOR KEY SHARE; IF NOT FOUND THEN RAISE EXCEPTION 'page % does not exist', NEW.doc_id USING ERRCODE = '23503'; END IF;` (documents before heads; the same SQLSTATE the FK raised before), then `INSERT INTO doc_history_heads AS h (doc_id, head_rev, dirty, last_write_at) VALUES (NEW.doc_id, 1, true, clock_timestamp()) ON CONFLICT (doc_id) DO UPDATE SET head_rev = h.head_rev + 1, dirty = true, last_write_at = clock_timestamp() RETURNING h.head_rev, h.content_epoch`; raise KH002 if the epoch is > 0; `NEW.rev := head_rev`; `IF NEW.created_at = now() THEN NEW.created_at := clock_timestamp(); END IF` (only the column default is replaced; explicit timestamps from tests and tools stay).
  - `NEW.rev IS NOT NULL` (new code): no `documents` lock here (statement (0) of `append_in_tx` already holds the page row's KEY SHARE, and the FK check still raises 23503 for a missing page; Rulings); `SELECT content_epoch FROM doc_history_heads WHERE doc_id = NEW.doc_id` (plain read of the row this transaction already locked); raise KH002 when no row or `NEW.epoch IS DISTINCT FROM content_epoch`.
- KH002: `RAISE EXCEPTION 'page history epoch changed; reload the page' USING ERRCODE = 'KH002'`. KH001 stays M1.1's text.
- `DOC_KEY_SHARE_SQL` = `SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE`, the first statement of every new-code flush (no row → `UpdatesStoreError::DocGone`, permanent, nothing written).
- `HEADS_UPSERT_SQL` (the second statement of every new-code flush; binds `$1` doc, `$2` row count): `INSERT INTO doc_history_heads AS h (doc_id, head_rev, dirty, last_write_at) VALUES ($1, $2, true, clock_timestamp()) ON CONFLICT (doc_id) DO UPDATE SET head_rev = h.head_rev + $2, dirty = true, last_write_at = clock_timestamp() RETURNING h.head_rev - $2 AS base, h.content_epoch`. It takes FOR NO KEY UPDATE on the heads row, which conflicts with an erase's FOR UPDATE but not with a snapshot trigger's FOR KEY SHARE.
- `doc_updates_append_only()` re-created (`CREATE OR REPLACE FUNCTION`) and its trigger re-created as `CREATE OR REPLACE TRIGGER doc_updates_append_only BEFORE DELETE OR UPDATE ON doc_updates FOR EACH ROW` (every column, not a column list). Body: M1.1's purge arm, then rule (b) `IF TG_OP = 'UPDATE' AND OLD.rev IS NULL AND NEW.rev IS NOT NULL AND NEW.seq = OLD.seq AND NEW.doc_id = OLD.doc_id AND NEW.update_bytes = OLD.update_bytes AND NEW.by_user_id IS NOT DISTINCT FROM OLD.by_user_id AND NEW.created_at = OLD.created_at AND NEW.source IS NOT DISTINCT FROM OLD.source AND NEW.meta IS NOT DISTINCT FROM OLD.meta AND NEW.epoch IS NOT DISTINCT FROM OLD.epoch THEN RETURN NEW; END IF;` (explicit columns, not `to_jsonb`, which would hex-encode every `update_bytes` twice per numbered row; a test pins the column list so a new column forces this rule to be revisited), then M1.1's `knot.history_rewrite` arm, then KH001.
- `doc_snapshots_epoch()` (`BEFORE INSERT OR UPDATE ON doc_snapshots FOR EACH ROW`, created in migration 3), in order: (o) `rewriting boolean NOT NULL := coalesce(current_setting('knot.history_rewrite', true), '') = 'on'` (`current_setting(.., true)` is NULL on a session that never set the flag, which is every fresh pool connection; without the `coalesce`, `NOT rewriting` is NULL and both the immutability guard and the epoch check would be skipped; the migration header says so; M1.1's flag; an erase writes its fresh snapshot at the new epoch before it bumps `content_epoch`, architecture erase Phase B steps 5-6); (i) `IF TG_OP = 'UPDATE' AND OLD.snapshot_rev IS NOT NULL AND NOT rewriting THEN RETURN NULL; END IF;` (a stamped snapshot is immutable: a 0.9.9 upsert at the same `snapshot_seq` would otherwise replace its bytes and keep its `snapshot_rev`; two 0.9.10 writers at one seq are at one rev and hold the same state, so skipping theirs loses nothing); (ii) `PERFORM 1 FROM documents WHERE id = NEW.doc_id FOR KEY SHARE` (not found → 23503 as above); (iii) `SELECT content_epoch FROM doc_history_heads WHERE doc_id = NEW.doc_id FOR KEY SHARE` (no row = 0); unless `rewriting`, raise KH002 when `NEW.epoch IS NULL AND epoch > 0`, or `NEW.epoch IS NOT NULL AND NEW.epoch <> epoch`. Step (ii) runs whenever the trigger does not return at (i), whether or not the rewrite flag is set; an upsert has already run it in its BEFORE INSERT firing.
- `meta` shapes written in M1.2 (ids, timestamps and page titles only; never page text, never person names): suggestion `{"suggestion_id", "accepted_by"}`; copy `{"source_doc_id", "source_title"}`; template `{"template_doc_id", "template_title"}`; legacy Markdown restore `{"target_snapshot_seq", "target_ended_at"}` (RFC 3339). `edit`, `task`, `import`: `meta` NULL.
- Writer constants (`crates/knot-crdt/src/writer.rs`): `BATCH_MAX = 200`, `BATCH_INTERVAL = 250 ms` (unchanged), a running `batch_cap` (starts at `BATCH_MAX`; halved, down to 1, after an attempt fails with an error for which `UpdatesStoreError::shrinks_batch()` is true; doubled back toward `BATCH_MAX` after each success), `RETRY_MIN = 250 ms`, `RETRY_MAX = 5 s` (doubling), `BUFFER_MAX_JOBS = 20_000`, `BUFFER_MAX_BYTES = 32 * 1024 * 1024`; `WriterLimits { server_job_deadline: 5 s, attempt_timeout: 10 s }` (`Default`; tests pass smaller values), `PUBLISH_TIMEOUT = 2 s`. A server-side job (one with a `persisted` reply) has no deadline until an append attempt fails (transiently) after the job arrived, whether or not that attempt held the job; from the end of that failed attempt it has `server_job_deadline` to be persisted, checked only between attempts. A slow attempt that succeeds, or a backlog of WS jobs ahead of it that keeps succeeding, therefore never fails it. One append attempt that takes longer than `attempt_timeout` is abandoned as transient. Postgres bounds every attempt itself, so an abandoned or stuck attempt never keeps a pool connection or its locks: `PgUpdatesStore::append`'s first statement after BEGIN (not `append_in_tx`'s, so Duplicate's longer transaction keeps the server defaults) is `SELECT pg_backend_pid(), now(), set_config('lock_timeout', $1, true), set_config('statement_timeout', $1, true)` with `$1 = APPEND_STATEMENT_TIMEOUT` (`crates/knot-storage/src/updates_store.rs`, `8 s`, below `attempt_timeout`; `PgUpdatesStore::with_statement_timeout(Duration)` for tests), so a lock wait or slow statement ends in Postgres with 55P03 or 57014, a transient error on a connection that rolls back and returns to the pool cleanly. As a backstop for a statement that outlives the writer's `attempt_timeout` (a dead TCP connection, a COMMIT stuck behind a stalled synchronous standby, several statements each just under the limit), `PgUpdatesStore::append` holds a drop guard with the pool and that pid: if the append future is dropped before it finishes, the guard spawns `tokio::time::timeout(2 s, SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid = $1 AND xact_start = $2)` on the pool, `$2` being the attempt's `now()` (the transaction start), so a connection that has already gone back to the pool and runs someone else's transaction is never cancelled. An abandoned attempt therefore holds its connection, its `documents` KEY SHARE and its heads row lock for at most `APPEND_STATEMENT_TIMEOUT` after it was abandoned, and retries do not pile up one connection per attempt behind it. During an outage, a room awaiting a server-side persist is blocked for at most one failed attempt + `server_job_deadline` + one more attempt, about 25 s; ApplySuggestion's pre-apply catch-up read can add a 10 s pool acquire, so an accept may take about 35 s inside the room. That bound protects the room actor, not the HTTP request: the API router's 30 s `TimeoutLayer` (`knot-server/src/lib.rs:255`) and the accept route's `ACCEPT_TIMEOUT` of 20 s (`suggestions.rs:76`, whose claim settles in its own task) answer the client first.
- Drain constants (`crates/knot-crdt/src/registry.rs`): `DRAIN_JOIN_WAIT = 15 s` (how long `acquire` waits for a previous writer of the same page to finish draining), `SHUTDOWN_DRAIN = 10 s` (how long `main` waits for every writer after the HTTP drain). The time before SIGKILL is preStop (`preStopSleepSeconds: 5`) + the HTTP drain (bounded only by the 30 s `TimeoutLayer`, `lib.rs:255`; during an outage a `/tasks` PATCH or an accept already waits 20-25 s on its room) + `SHUTDOWN_DRAIN`, so the worst case is 5 + 30 + 10 = 45 s, and with slack for process start-to-signal latency, a flush still in flight when `drain_all` times out, the `pending` log line and the telemetry flush, the chart's default `terminationGracePeriodSeconds` (`deploy/helm/knot/values.yaml:175`) goes from 30 to 50 (Task 8); the README says the budget includes a few seconds of slack and that a shorter grace period can lose buffered edits on a shutdown during an outage. Writers keep retrying during the HTTP drain, so outside an outage 30 s is still enough.
- Permanent store errors (`UpdatesStoreError::is_permanent`; writer sends a Permanent `Fatal`, room evicts): SQLSTATE `KH001`, `KH002`, `23503`, `23505`, every SQLSTATE of class `22` (data exception), `42` (syntax error or access rule violation) and `XX` (internal error), `UpdatesStoreError::{EpochChanged, DocGone, LegacyCountMismatch}`, `sqlx::Error::{PoolClosed, Decode, ColumnDecode, RowNotFound}` (each fails the same way on every retry). Everything else is transient (retried), `40P01`, `55P03` (the append's `lock_timeout`) and `57014` (its `statement_timeout`, or the backstop's cancel) included. `UpdatesStoreError::shrinks_batch` is true for `57014` and class `54` (program limit exceeded): the failure may depend on the batch's size, so the writer halves `batch_cap` before the retry; at `batch_cap = 1` such an error stays transient. `55P03` does not shrink the batch: a lock wait does not depend on its size.
- Writer Fatal comes in two kinds. **Permanent** (a permanent store error): `Err` to every buffered job, nothing more is written, the writer ends. **Evict** (the buffer overflowed, or a server-side job passed its deadline): every buffered server-side job that no in-flight attempt holds gets `Err` at once and is removed (whichever job caused the Evict: the job that overflows the buffer can itself be a task toggle, import or accept, and the room is blocked awaiting its reply, so it would never read the Fatal); a server-side job held by an in-flight attempt gets that attempt's outcome, `Ok` on success, and on failure `Err` and removal instead of a retry. Removing one reorders nothing: the room awaits a server-side reply before it sends another job, so a server-side job is always the buffer's last (`debug_assert!` that nothing is behind it, and an error log naming how many jobs were dropped in release builds). Then the writer closes `rx` and takes what is still queued in it; a server-side job among those also gets `Err` at once and is dropped (the room can have queued it behind the WS job that overflowed the buffer, and is now blocked awaiting its reply; it is the queue's last for the same reason, so dropping it reorders nothing). The writer then drains every remaining job with ordered retries and no deadline until the buffer is empty (or a permanent error turns it into Permanent), then ends. Both send `WriterEvent::Fatal(reason)` first, and both resolve the writer's `WriterDone` only when the writer task ends.
- A `Err` reply to a server-side job means "not persisted". (The one exception is an attempt abandoned by `attempt_timeout` whose COMMIT had already reached Postgres; see Rulings.)
- A room writes a snapshot only when `!evicted && !unlogged && in_flight == 0 && max_own_rev <= applied_through_rev`, stamped `snapshot_rev = applied_through_rev`, `epoch = content_epoch`, `snapshot_seq = through_seq` (the seq of the row at `applied_through_rev`, or the hydrating snapshot's seq).
- Snapshot invariant, as M1.3 and M3 may rely on it: a stamped snapshot at rev R equals `base ∪ replay(R)`, where `base` is the page's pre-upgrade content that no row carries (the legacy snapshot the room booted from, whose rows the pre-M1.1 GC pruned; empty for a page created after the upgrade, and for a page whose rows all survived). It is not `replay(R)` alone, and it is not a subset of the architecture's genesis as written ("the latest baseline plus every surviving row with seq > its snapshot_seq and created_at < history_since") plus later rows either: a page that hit the 0.9.x late-seq skip (`room.rs:755`) has a legacy row b with seq < the baseline's `snapshot_seq` that the baseline lacks, the room boots it with b (Rulings: a legacy snapshot triggers `RowsAfter::All`), and its first stamped snapshot certifies b. The architecture's "every snapshot is a subset of the replayed log" holds only if M1.3's genesis applies every surviving row with `created_at < history_since`, including rows with seq ≤ the baseline's `snapshot_seq` (replay is idempotent, so rows the baseline holds change nothing); otherwise M1.3's gap check writes a spurious authorless gap entry for b's content on every such page. Task 11 records this as a deviation, and "After M1.2" hands it to M1.3.
- New metrics: `knot_writer_flush_seconds` (histogram), `knot_writer_flush_retries_total`, `knot_room_rev_gap_total`, `knot_room_row_apply_errors_total` (label `phase` = `boot` | `catch_up`), `knot_room_updates_persisted_raw_total`, `knot_history_legacy_mismatch_total`.
- Metric assertions: `crates/knot-crdt/src/test_metrics.rs` (`#[cfg(test)] mod test_metrics;` in `lib.rs`, created in Task 6) holds `CountingRecorder`, a `metrics::Recorder` modelled on `Counters` in `crates/knot-server/src/routes/api/markdown.rs:615-700`, with `fn counter(&self, name: &str, labels: &[(&str, &str)]) -> u64`. A test installs it with `let _g = metrics::set_default_local_recorder(&rec);`, which is thread-local, so only in current-thread tests (`#[tokio::test]` and `start_paused`, where the spawned room and writer run on the test's thread). No `metrics-util` dependency.
- New tables go into `e2e/support/reset.ts` `TABLES` and `crates/knot-storage/tests/migrations_apply.rs` `expected` (alphabetical).
- No new dependencies except, in the existing `crates/knot-crdt/Cargo.toml`, `serde_json.workspace = true` under `[dependencies]` and a second `tokio` entry, `tokio = { workspace = true, features = ["test-util"] }`, under `[dev-dependencies]` (tokio is already in `[dependencies]`; the dev entry only adds `test-util`, needed by `#[tokio::test(start_paused = true)]`; the workspace tokio has no `test-util`). The `serde_json` line adds it to knot-crdt's entry in `Cargo.lock`, so Task 6 commits `Cargo.lock` too: the Dockerfile builds with `cargo zigbuild --release --locked` (`Dockerfile:98`), and the Helm upgrade test's candidate image and the release build would otherwise fail with "the lock file needs to be updated but --locked was passed". Random tests use a local seeded xorshift, not a crate.
- Verification commands: `make lint`, `make test.rust` (cargo nextest against `knot-dev-postgres` on :5432), `make test.web`. After adding a migration file run `cargo clean -p knot-storage -p knot-test-support` once if tests report missing columns (sqlx `migrate!` embeds at build time). Commits are SSH-signed through 1Password; never bypass signing.

## Review Focus

1. **A page last saved before the upgrade.** Its latest snapshot is a legacy one (no `snapshot_rev`), some legacy rows have `seq > snapshot_seq`, one legacy row has `seq < snapshot_seq` but is missing from the snapshot (the 0.9.x late-seq skip, `room.rs:755`), and none is numbered yet. Expected: the room opens it with all content, numbers its legacy rows `1..legacy_rows` in seq order once, and the next edit gets `legacy_rows + 1`. Its first stamped snapshot then holds the lower-seq row too, which M1.3's genesis must also apply (snapshot invariant, Global Constraints). Pinned in Task 7 (`legacy_page_boots_complete_and_continues_numbering`, `legacy_snapshot_missing_a_lower_seq_row_boots_with_it`).
2. **A 0.9.9 pod writing the same page during the rolling upgrade.** Its rows get revs from the trigger, interleave gaplessly with new rows, and the new pod's room picks them up by rev; its snapshot upserts never overwrite a stamped snapshot. Pinned in Task 10 (`old_and_new_replicas_share_one_gapless_log`) and Task 3 (`an_old_snapshot_upsert_leaves_a_stamped_snapshot_alone`).
3. **The database is down for a while as people type.** Expected: nothing is lost, no snapshot is written meanwhile, and the room keeps serving WS edits until the buffer limit; a server-side edit (task toggle, import, accept) made during the outage fails within about `attempt_timeout` + `server_job_deadline` + `attempt_timeout` and evicts the room instead of freezing it for the outage, while the WS edits buffered ahead of it are still written, once, when the database is back (also after the page's last editor left, and on a graceful shutdown within `SHUTDOWN_DRAIN`). A slow database that still answers evicts nothing. Pinned in Task 6 (deadline and drain unit tests), Task 7 (`flush_failures_lose_nothing`), Task 8 (drain barrier and shutdown) and Task 9 (`server_edit_during_an_outage_frees_the_room_and_keeps_the_ws_edit`).
4. **Reconnect after offline editing.** The SyncStep2 resend carries the whole delete set and content the server has; only what is new is persisted, and replaying the log rebuilds the live doc. Pinned in Task 1 (property test) and Task 7 (`reconnect_resend_persists_only_new_content`). Under the spike fallback (Rulings) only the replay half holds: the resend is persisted whole.
5. **A server-side edit whose persist fails** (task toggle, import, template, restore). It must reach no client and so can never come back through someone's reconnect credited to them. Pinned in Task 9 (`server_edit_persist_failure_reaches_no_client`).
6. **A page edited before the upgrade whose old rows were pruned.** Its legacy snapshot holds content no row carries. Expected: it gets a heads row in the seed, the room boots it with that content, and every stamped snapshot equals that legacy snapshot ∪ `replay(R)`. Pinned in Task 3 (`old_inserts_racing_the_heads_migration_are_counted_exactly`) and Task 7 (`legacy_snapshot_content_without_rows_is_the_base_of_stamped_snapshots`).

## Rulings

- Ruling: five migrations instead of the architecture's one transaction plus the CIC file — ADD COLUMN needs ACCESS EXCLUSIVE on `doc_updates`, which would block every page load for the seed's duration; split, the seed runs under SHARE ROW EXCLUSIVE on `doc_updates` alone (reads continue, only saves wait; 0.9.9 rooms block only where they await a save inline: task toggles, accepts, imports, restores), and the FK and the snapshot epoch trigger, which need `documents` and `doc_snapshots`, run separately so page writes and snapshot writes pause for one pass over the heads rows, not the seed — cost if wrong: more Job steps (more lock-loop time in the worst case; the README's `--timeout` grows to 8 minutes).
- Ruling: `doc_snapshots_epoch` moves from migration 2 to migration 3 — migration 2 would otherwise hold `doc_snapshots` for the whole seed, and 0.9.9 rooms await snapshot writes inside their actor loop (`room.rs:326-340`, `:683-707`), freezing open pages; nothing reads `content_epoch > 0` before M3, so a snapshot written between migrations 2 and 3 is harmless — cost if wrong: none.
- Ruling: a stamped snapshot row is immutable (`doc_snapshots_epoch` returns NULL on UPDATE of a row with `snapshot_rev`) — 0.9.9's upsert (`snapshot_store.rs:86-91`) sets only bytes, vector and `created_at`, so on a `snapshot_seq` collision it would keep `snapshot_rev = R` over a state that is not `replay(R)` (unflushed or dropped edits, a skipped late-seq row), and new boots would read only `rev > R` forever after; two 0.9.10 writers at one seq are at one rev with equal state — cost if wrong: a stamped row cannot be refreshed in place, which nothing needs.
- Ruling: the page row's KEY SHARE is the first lock of any transaction that locks a page-scoped history row (`doc_history_state`, `doc_history_heads`, or the page's `doc_updates` rows), and `purge` is wrapped in `placement::retry_on_deadlock` — heads-first would deadlock with a purge (its DELETE holds the page row, its new cascade into `doc_history_heads` waits on the flush, whose FK check waits on the purge), and nothing retries a purge (`doc_store.rs:1007`); with the trigger path, snapshot writes and legacy numbering also taking the page row first, no cycle is known, so the retry is a backstop for an order this plan did not foresee (0.9.9's contributor CTE, archive locks). M1.3's builder and M3's erase Phase B lock `doc_history_state` before the heads row (architecture entry_builder BUILDER: "doc_history_state → heads, the same as erase"), and from M1.3 a purge cascades into `doc_history_state` too, so they inherit the rule with the page row first of all (Global Constraints, "After M1.2"): the builder's `doc_history_state` claim and its insert of a missing row, and erase Phase B step 2's first SELECT (`… FROM doc_history_state … FOR UPDATE`), each come after `SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE`; an erase that took `doc_history_state` or the heads row before the page row would meet a purge in the opposite order, and nothing retries an erase (review round 5) — cost if wrong: a flush racing a purge waits for it and then fails permanently (`DocGone`), which evicts a room whose page is gone anyway.
- Ruling: the `NEW.rev IS NOT NULL` arm of `doc_updates_assign_rev` takes no `documents` lock (review round 4) — the architecture's storage_model has that arm only check the epoch against the heads row the transaction already locked; `append_in_tx` statement (0) already holds the page row's KEY SHARE, and the FK check still raises 23503 for a missing page, so a second lock per row would only add a PL/pgSQL row lock to every row of a 200-row flush, on the live-typing path the design names as an anti-goal; only the trigger path (0.9.9 pods, raw inserts) and snapshot writes take the lock in the trigger, because nothing before them took it — cost if wrong: a raw `INSERT` with an explicit rev that skipped `DOC_KEY_SHARE_SQL` takes the page row only in the FK check, after the heads read; no production code does that.
- Ruling: a server-side persist is bounded (`WriterLimits`), not awaited for the whole outage — the room awaits it inside its actor loop, and a blocked room sends no further jobs, so the buffer limit never fires and the room would freeze for the outage; the bound protects the room actor (the HTTP request has its own 30 s `TimeoutLayer`, `lib.rs:255`, and accept its 20 s `ACCEPT_TIMEOUT`); the writer also keeps receiving jobs while an attempt is in flight, so `persist_tx` never fills behind a slow append — cost if wrong: a server-side edit during an outage longer than about 5 s fails and evicts the room; editors reconnect and resend.
- Ruling: the server-job deadline starts at the first failed attempt after the job arrived, not at its arrival (review round 2) — from arrival, a slow attempt that succeeds (a 6 s pool wait under load, a heads row behind a slow transaction, a later release's migration pausing saves) or a backlog of 200+ WS jobs ahead of it would evict the room although no store call failed, and every task toggle or accept on an open page under load would force its editors to reconnect, adding load. A failed attempt that did not hold the job still starts it: during an outage with a backlog ahead, the job never reaches an attempt, and the room must not freeze. Overdue server jobs are not moved to the front: a server job's update can build on the WS jobs ahead of it, and rev order must stay apply order — cost if wrong: on a database that is slow but answering, a room awaiting a server-side persist behind a long backlog waits up to one `attempt_timeout` per 200 jobs ahead; the HTTP request times out first.
- Ruling: an Evict Fatal (deadline or overflow) still writes the jobs buffered ahead of the cause, with ordered retries and no deadline; only a Permanent Fatal fails every buffered job (review round 2, critical) — the jobs ahead of a server job don't depend on it, and nothing else holds them once their author has closed the tab (rooms are never idle-evicted, `registry.rs:174`, and `/tasks` reaches pages nobody has open through `acquire`, `tasks.rs:167`); dropping them would lose the text, or credit it to whichever editor's reconnect resends it first, the design anti-goal. A Permanent error means the store refuses this page's writes (epoch changed, page gone), so retrying cannot succeed — cost if wrong: an evicted page's writer lives on through the outage holding at most `BUFFER_MAX_BYTES`, as its room did before.
- Ruling: a new room waits for the previous writer of its page to finish draining (`acquire`, up to `DRAIN_JOIN_WAIT`, then `AcquireError::Draining`, which the client retries; the wait happens before `acquire` takes the per-page `inflight` mutex, on a clone of the `WriterDone`, so N clients reconnecting at once all wait at most `DRAIN_JOIN_WAIT` instead of N × `DRAIN_JOIN_WAIT` in turn; outside the mutex `acquire` only reads, and removing a dead handle, moving its `WriterDone` to `draining` and the bus unsubscribe stay under the mutex, as today, so an `UNLISTEN` can never land after the next boot's `LISTEN` (review round 4); under the mutex `drop_if_dead` runs before the `draining` check, so a room that died while this `acquire` waited for the mutex is drained too), and `main` drains every writer for up to `SHUTDOWN_DRAIN` after the HTTP drain on SIGTERM (review round 2) — otherwise the reconnect's resend reaches a new room before the old writer's rows land, the effective update credits the resender with the drained text, and on SIGTERM the runtime drops every writer task with its buffer on every rolling deploy. The barrier is per replica: a client that reconnects to another replica while this one drains can still be credited with the drained text (recorded in Task 11's deviation notes) — cost if wrong: a page whose writer drains through an outage cannot be opened on that replica until the database is back, which it could not be anyway (boot reads the store); a SIGTERM during an outage longer than `SHUTDOWN_DRAIN` still drops what is buffered, the architecture's crash residual.
- Ruling: the chart's default `terminationGracePeriodSeconds` goes from 30 to 50 s, and the shutdown drain still starts after the HTTP drain (review rounds 3 and 4) — the worst case is preStop 5 s + HTTP drain up to the 30 s `TimeoutLayer` + `SHUTDOWN_DRAIN` 10 s = 45 s, and 5 s of slack covers signal latency, a flush still finishing when `drain_all` times out, the `pending` log line and the telemetry flush; starting `drain_all` at the shutdown signal instead would evict rooms that in-flight `/tasks` PATCHes and accepts are still awaiting, failing them during an ordinary rolling deploy — cost if wrong: pods take up to 20 s longer to be killed when shutdown hangs; operators who set a shorter grace period can lose buffered edits on a shutdown during an outage (README).
- Ruling: an attempt abandoned by `attempt_timeout`, or one that lost its connection during COMMIT, is retried even though it may have committed — the alternative is dropping edits; a duplicated row is the same yrs update twice, which replay applies idempotently (it changes nothing the second time), and its rev is still gapless; the room learns of such rows only as a rev gap ahead of its own `Applied` revs, catches up over them and snapshots only once `applied_through_rev ≥ max_own_rev`, which Task 7 pins against real Postgres (`Flaky` lost-ack mode) and in a room unit test — cost if wrong: M1.3 sees a row that integrates nothing and must show no entry for it (Task 11 records this for M1.3).
- Ruling: Postgres ends a slow append attempt itself (`SET LOCAL lock_timeout` and `statement_timeout` = `APPEND_STATEMENT_TIMEOUT` inside the append transaction), and an abandoned attempt's backend is cancelled by a drop guard (`pg_cancel_backend`) (review round 3, critical) — dropping the append future sends no CancelRequest: the statement keeps running, `Transaction::drop` only queues a ROLLBACK, and `PoolConnection::drop` pings with no timeout while still holding the pool permit, so each abandoned attempt kept a pool slot, the page's `documents` KEY SHARE and the heads row lock until its statement finished, and each retry queued behind the previous one; 3-4 open pages during a 60 s pause would exhaust a replica's 16-connection pool and fail every page load. `attempt_timeout` stays as the writer's backstop for a hanging double or a dead connection — cost if wrong: a single statement that legitimately needs more than 8 s (a 200-row batch of very large updates on a slow disk) fails with 57014, the writer halves its batch and retries (next Ruling), and with a server-side job behind it evicts the room after `server_job_deadline`; the dead-TCP case still holds that one client-side slot until TCP keepalive, though the cancel releases the backend's locks.
- Ruling: an attempt that fails the same way every time must not keep a writer retrying forever (review round 4, important) — after an Evict the writer drains with no deadline, so such a batch would keep the page unopenable on that replica (`AcquireError::Draining` after each `DRAIN_JOIN_WAIT`) until the pod restarts, and on SIGTERM drop the whole buffer. Two measures: deterministic errors (SQLSTATE classes `22`, `42`, `XX`; sqlx `Decode`, `ColumnDecode`, `RowNotFound`) are Permanent; `57014` and class `54` halve `batch_cap` (down to 1) before the retry, since `BATCH_MAX` counts jobs and a 200-job batch of 4 MiB WS messages can take longer than `APPEND_STATEMENT_TIMEOUT` every time, and the cap doubles back after each success. `55P03` does not shrink (a lock wait does not depend on the batch), and at `batch_cap = 1` a timeout stays transient: a single row that always takes more than 8 s means the database itself is failing — cost if wrong: a single-row attempt that times out on every retry still keeps its writer retrying, visible in `knot_writer_flush_retries_total`; a deterministic error evicts the room and fails what is buffered, and editors resend on reconnect.
- Ruling: the heads FK is added after the seed in its own migration, after deleting orphan heads rows — a FK check per seeded row would take FOR KEY SHARE on `documents` rows while holding `doc_updates`, which deadlocks with a purge holding FOR UPDATE on the page (the M1.1 three-way) — cost if wrong: for the seconds between migrations 2 and 3, a purge leaves an orphan heads row, which migration 3 deletes.
- Ruling: factor the M1.1 lock loop into `knot_lock_tables` and leave the M1.1 files as shipped — three more copies of a 90-line loop would drift; applied migrations can't change — cost if wrong: the port has a bug the M1.1 copy didn't; Task 2's tests pin the deadlock escape and the give-up message.
- Ruling: the helper's lone wait requests `SHARE UPDATE EXCLUSIVE` whatever the target mode (review round 2) — M1.1 waited in its target mode (SHARE ROW EXCLUSIVE), which blocked only writers; in migrations 1 and 5's `ACCESS EXCLUSIVE` each lone wait would stop every page load for `deadlock_timeout` + 500 ms (about 1.5 s), repeatable every 5 s per worker. `SHARE UPDATE EXCLUSIVE` conflicts with autovacuum's own lock, so the deadlock check still cancels the worker, and it conflicts with no reader or writer — cost if wrong: a worker that the check does not cancel this way makes the loop give up after 45 s, as in M1.1, and the Job retries.
- Ruling: keep the architecture's `CREATE UNIQUE INDEX CONCURRENTLY` and add migration 5, which rebuilds an invalid or missing index under the lock helper (ACCESS EXCLUSIVE, then DROP + CREATE) — re-running the upgrade after an aborted CIC otherwise leaves an invalid index forever (`IF NOT EXISTS` skips it). A killed Job leaves either a build still running in Postgres (the backend does not notice its client left; it keeps the session advisory lock, and the retried Job waits for it or, if the detector picks one side, converges on the next run) or an invalid index; the README's halt table tells operators to check `pg_stat_progress_create_index` before re-running and to wait for the build or `pg_terminate_backend` it. The CIC is not that rare to abort: sqlx holds a session advisory lock for the whole migrate run, every knot-server start migrates (`pool.rs:56`), and a 0.9.9 pod that restarts during the Job waits on that advisory lock with a live snapshot, which CIC's wait-for-older-snapshots phase waits on in turn; the deadlock detector may pick the CIC. The upgrade notes say so and ask operators not to restart pods during the index build — cost if wrong: in that case page loads and saves pause for one scan of `doc_updates`.
- Ruling: no test reproduces the advisory-lock/CIC deadlock (review round 1 suggested one) — which side the detector aborts is not deterministic, and both outcomes end in a valid index: if the waiter dies, the CIC finishes; if the CIC dies, the Job re-runs and migration 5 repairs, which `index_check_rebuilds_an_invalid_index` pins — cost if wrong: an untested path, documented for operators.
- Ruling: `doc_updates_source_check` is added `NOT VALID` and never validated — validating scans the table under ACCESS EXCLUSIVE; every existing row is NULL, and new rows are checked either way — cost if wrong: `\d` shows the constraint as not valid.
- Ruling: the append-only trigger fires on UPDATE of any column, and rule (b) compares every column except `rev` explicitly — M1.1's column list let `by_user_id` and `created_at` be rewritten, which would falsify history; nothing updates `doc_updates` today (`grep`); explicit comparison avoids `to_jsonb`'s hex encoding of every row's bytes during numbering, and `doc_updates_columns_are_the_ones_rule_b_compares` fails when a column is added — cost if wrong: an unknown updater fails with KH001 (loudly).
- Ruling: both insert arms of the heads upsert give a page's first rows revs from 1 and a non-NULL `last_write_at` — the architecture's statements insert `(doc_id)` alone, so the row takes the column defaults: the trigger path (`… DO UPDATE SET head_rev = head_rev + 1`) would give the first row rev 0, the flush path (`… head_rev + $n … RETURNING head_rev - $n AS base`, WRITE PATH step 1) would return base = −n, and both would leave `last_write_at` NULL, which ascending order puts last in M1.3's `WHERE dirty ORDER BY last_write_at`. The trigger path inserts `head_rev = 1`, the flush path (`HEADS_UPSERT_SQL`) `head_rev = n`, so base = 0, and both set `dirty = true, last_write_at = clock_timestamp()` on insert, so M1.3's claim ordering never meets a NULL (review round 5); `append_assigns_consecutive_revs_and_stores_labels` and `old_style_inserts_are_numbered_after_the_legacy_rows` pin it — cost if wrong: none.
- Ruling: every flush still sets `last_write_at = clock_timestamp()`, although it is the key of `doc_history_heads_dirty_idx`, so no heads-row update is HOT (review round 5 suggested setting it only on the clean-to-dirty transition) — M3's erase preview defines `page_active` as "`last_write_at` within 5 min" (architecture, PREVIEW), which a first-dirtied timestamp would break for a page edited continuously while the builder lags; and HOT would not stop the growth the finding describes: under a long transaction (pg_dump, anti-wraparound vacuum) no version newer than the xmin horizon can be pruned, and the `ON CONFLICT (doc_id)` probe walks each live-to-someone version whether it sits in a HOT chain (which also leaves its page once the page fills) or behind its own index entry; without a long transaction, autovacuum, opportunistic pruning, killed index entries from the builder's 2 s scans and bottom-up deletion on the unchanged primary key keep the row small. The architecture's "the heads row does not bloat" meant one heads update per flush instead of one per row; Task 11's deviation notes correct it to "one non-HOT heads version per flush, reclaimed by vacuum" — cost if wrong: during a long transaction, flush latency on an active page grows with its number of flushes since the transaction began, visible in `knot_writer_flush_seconds`; the clean-to-dirty variant can still be adopted later together with a different `page_active` signal (the page's newest row through the `(doc_id, rev)` index).
- Ruling: the trigger replaces `created_at` only when it equals `now()` (the column default) — tests and tools insert explicit timestamps (`crates/knot-crdt/tests/gc.rs:37`, `crates/knot-server/tests/history_stats_integration.rs:22`) — cost if wrong: a row explicitly stamped with exactly the transaction start gets clock time.
- Ruling: `meta` may hold page titles (`source_title`, `template_title`) — the architecture lists them in the copy and template shapes, and its "never page text or names" means page content and person names; titles must survive a rename or purge of the source — cost if wrong: M3's erase leaves titles in `meta`, as the design already says erase does not touch titles.
- Ruling: today's Markdown restore (`routes/api/history.rs`) writes source `restore` with meta `{target_snapshot_seq, target_ended_at}` — M1.3 then shows it as "Restored by X, to <date>" until M2 replaces the route — cost if wrong: M1.3 reads two meta shapes for `restore`.
- Ruling: every server-side event (ApplyUpdate, ReplaceWithMarkdown, PatchTaskChecked, ApplySuggestion) persists before it fans out, and evicts the room when the persist fails or the apply errors after mutating the doc — today three of them fan out first, so a failed persist reaches clients, who push it back on reconnect credited to themselves (a design anti-goal); `ReplaceWithMarkdown` also cleared the live doc before a decode could fail; `ApplyUpdate` persists its effective update (the ApplyUpdate Ruling below) — cost if wrong: server-side edits reach other editors up to one flush (≤ 250 ms) later.
- Ruling: an update yrs rejects part-way is still neither persisted nor fanned out (as today, `tests/nesting.rs:1191`), and the room stops writing snapshots for the rest of its life (`unlogged`) — persisting the integrated part would let a crafted parked block fail every later boot; snapshotting would put content with no row into the boot cache — cost if wrong: such a room's next boot replays more rows, and while it lives it serves the integrated, unlogged content to joiners (JoinState), whose later edits are persisted with origins pointing at items no row carries; on any log replay those rows stay pending forever. Visibly: once the unlogged room ends, every edit other editors saved inside the unlogged content (persisted and fanned out while the room lived) is gone from the live page at the next boot, where today it survives through the room's snapshot. `a_rejected_update_stops_snapshots` pins this outcome, Task 11 records it, and M1.3's replay must tolerate rows that never integrate; this is the one case where the live page holds content the log lacks.
- Ruling: a row read from the log that fails to apply, at room boot (`BootMode::Room`) or in catch-up, is logged (warn, doc id and rev), counted in `knot_room_row_apply_errors_total{phase}`, marks the room `unlogged`, and is passed: the watermark advances past it and later rows still apply (review round 2) — yrs integrates part of such a row (a crafted parked block makes every later apply that brings its dependency fail, `tests/nesting.rs:1154-1183`, and a replica that never parked the block persists that innocent edit). Stopping at the row would stall catch-up forever (no snapshot, `SuggestionApplied` unanswered, no gap metric), and failing boot (today's `?`, `room.rs:248-258`) makes the page unopenable for good; advancing without `unlogged` would let a stamped snapshot certify a partly integrated row. The row is in the log, so a replay meets it too. A read-only load (`BootMode::ReadOnly`: the Markdown refresh, its sweep, `knot-server export-check`) instead fails with `BootError::Apply` on the first row that fails, as `stored_snapshot` does today (`markdown.rs:136-139`: "a refresh never caches a page with part of it missing"; otherwise a `TaskIndex::Write` refresh would drop that page's tasks from /tasks, and `markdown_compat::a_page_that_does_not_load_fails_the_check` and `refresh_sweep::sweep_skips_past_a_failing_doc` would break) (review round 4); neither reason for passing the row (an openable page, a catch-up that does not stall) applies to a read-only load — cost if wrong: a page with such a row past its last stamped snapshot never gets another one, so every boot replays from that snapshot; the metric shows it, and M1.3 must tolerate the row (Task 11 records it); its Markdown export and task index stay unrefreshed while no room for it runs on the replica, as today.
- Ruling: content that only a 0.9.9 pod's legacy snapshot holds during the rollout (a 0.9.9 writer dropped a failed batch and its room then snapshotted the live doc) is lost once a stamped snapshot exists: boot prefers the stamped snapshot, a stamped write at the same seq promotes the legacy row, and a 0.9.9 upsert onto a stamped row is skipped (review round 2) — applying a newer legacy snapshot at boot would make that page `unlogged` for every later room (no stamped snapshot could ever be newer), so its boots replay forever; the case needs a database error on a 0.9.9 pod during the rollout window, and the edit's author normally still has it and resends it on reconnect — cost if wrong: such content is lost unless a connected client resends it; the CHANGELOG rollout caveat and Task 11's deviation notes say so, and the architecture's gap entry does not cover it.
- Ruling: every page gets a heads row in the seed, from `documents LEFT JOIN doc_updates` (review round 2) — pages whose rows the pre-M1.1 GC pruned would otherwise get none until their next edit, and M1.3's builder, which finds work only through `doc_history_heads WHERE dirty`, would never visit them to write their baseline entries; adding the rows later needs a migration that FK-checks `documents` while holding heads rows, the purge-deadlock class — cost if wrong: one more read of `documents` (ACCESS SHARE) in migration 2. Every page starts dirty with `last_write_at` = the migration's `now()` (not NULL, which ascending order puts last, so on a busy instance with more than 20 freshly dirty pages per tick the seeded pages would never be reached), so M1.3's builder, ordering by `last_write_at`, reaches every seeded page before any page written after the upgrade; it may take many ticks, not one pass.
- Ruling: `doc_snapshots_epoch` honours M1.1's `knot.history_rewrite` flag (it skips the epoch check and the stamped-row immutability, not the page-row lock) (review round 3) — the architecture's erase inserts its fresh snapshot at the new epoch (Phase B step 5) before it bumps `content_epoch` (step 6), which the trigger would otherwise refuse with KH002; `a_flagged_rewrite_may_write_a_snapshot_at_any_epoch` pins it — cost if wrong: a session that sets the flag can write a snapshot at any epoch, which only an erase does.
- Ruling: `ApplyUpdate` persists and fans out `apply_update_measuring(..).persist` (the effective update, or the raw bytes when the apply parked or drained pending content), not the update captured by `observe_update_v1` (review round 3) — yrs 0.27.3 emits `update_v1` only when the delete set is non-empty or the state vector moved (`store.rs:587`), so an `ApplyUpdate` whose blocks park as pending would leave content in the live doc that no row carries; an `ApplyUpdate` that changes nothing persists and fans out nothing and replies `Ok(last_applied_seq)` (callers ignore the seq: `markdown.rs:573-579`, `docs.rs:1278`); a rejected apply replies `Err` and evicts the room like any server-side apply error, and so does an apply with `max_depth > MAX_NESTING` (nothing persisted or fanned out; the live doc holds refused content, as after `refuse_applied`; today ApplyUpdate checks no depth, but every production caller degrades deep Markdown before it builds the update, `markdown_import_integration.rs:860-882`, so only a bug reaches this arm) (review round 4). The other server-side events build their update inside the room's own transaction, where nothing parks, and keep `observe_update_v1` — cost if wrong: none (production callers send self-contained updates).
- Ruling: the persist helper treats a closed `persist_tx` as Fatal (evicted, shutdown cancelled, the edit neither fanned out nor persisted), and the `WriterEvent` arm sits before `rx.recv()` in the room's `biased` select (review round 2) — after an Evict Fatal the writer closes its channel, and today's "persist channel closed; dropping update" (`room.rs:927`) would keep fanning out edits it can no longer persist; behind `rx.recv()` a steady stream of inbound events would delay the Fatal — cost if wrong: none.
- Ruling: the writer's `bus.publish` is bounded by `PUBLISH_TIMEOUT` and its failure is only logged (review round 2) — `PgBus::publish` (`bus_pg.rs:276`) has no timeout, and a half-open connection would stop the writer draining `rx`, fill `persist_tx` and block the room; the publish is best-effort, since the 5 s catch-up tick covers a lost one — cost if wrong: peers see an edit up to 5 s late.
- Ruling: the room keeps `last_applied_seq` (max seq applied) beside `applied_through_rev`; `Event::ExportState` and the Markdown cache keep seq semantics in M1.2 — the cache is not history and changing its key is out of scope — cost if wrong: during the rollout a seq/rev inversion (the trigger-path seq Ruling below: a 0.9.9 row at seq 100 and rev 51 behind a new row at seq 101 and rev 50) lets a render, its task listing and its search entry be filed under seq 101 while rev 51 is not yet applied, and they stay stale until the page is next edited; the rollout caveat in the CHANGELOG and README says so. The reviewer's alternative (report the highest seq among rows with rev ≤ `applied_through_rev`) does not help: rev 50, seq 101 is inside that prefix (review round 4).
- Ruling: the writer→room channel becomes unbounded (`WriterEvent`), `Applied` loses its unused `bytes`, and the bus is published once per flush with the highest rev — a bounded `try_send` dropped confirmations, which would freeze `in_flight` and block snapshots forever, and a bounded `send` deadlocks with a room awaiting a persist reply — cost if wrong: memory bounded by `BUFFER_MAX_JOBS` small messages.
- Ruling: a room's exit snapshot is written only when the snapshot rule holds; it does not wait for in-flight rows — the next boot replays them — cost if wrong: a slightly slower next boot.
- Ruling: room boot fails on any store read error (today it boots from whatever it could read, `room.rs:248-259`) — a partial page served as complete is worse than a failed join that the client retries; a row that reads but fails to apply does not fail boot (see the apply-error Ruling) — cost if wrong: joins fail during a DB blip instead of loading partial content.
- Ruling: `LegacyCountMismatch` keeps failing the page's boot (logged at error level with the doc id, counted in `knot_history_legacy_mismatch_total`), with no automatic fallback (review round 1 suggested numbering past `head_rev`) — rows can only be inserted through the numbering trigger or new code and never deleted outside a purge, so a mismatch means someone edited the tables by hand; numbering legacy rows after newer ones would put them out of commit order and silently certify the damage. The Helm README says what the log line means and to report it with the doc id — cost if wrong: one page stays unopenable until an operator repairs it.
- Ruling: catch-up applies only rows contiguous from `applied_through_rev + 1` (a row that fails to apply is passed, not a gap: see the apply-error Ruling; a row with `rev <= applied_through_rev`, which a store or double may return, is skipped, not a gap) and stops at a rev gap (`knot_room_rev_gap_total`, retried on the next tick), and a catch-up that stopped at a gap reports "not caught up" (`SuggestionApplied` then stays unanswered, as for a read error) — rev order is commit order, so a gap means a bug; applying past it would make a later snapshot claim a rev it overshoots — cost if wrong: a stuck gap stalls catch-up and the applied check for that page, visibly in the metric.
- Ruling: catch-up fans out only rows that changed the room's doc (`apply_update_changed`) — own rows re-read after a gap were already fanned out — cost if wrong: none.
- Ruling: boot picks the snapshot with the highest `snapshot_rev`, else the highest `snapshot_seq`; after a stamped snapshot it reads `rev > snapshot_rev`; after a legacy snapshot (or none) it re-applies every row of the page (`RowsAfter::All`; applying is idempotent) — a legacy snapshot is not exact even for legacy rows: a 0.9.x room skipped a peer row whose seq was below its own confirmed seq (`room.rs:755`) and then snapshotted without it, and M1.1 kept that row; reading only `seq > snapshot_seq` would keep the loss, and the room's first stamped snapshot would certify it (this is the architecture's "catch-up by rev fixes room.rs:762-764") — cost if wrong: one pass over the page's whole log at its first 0.9.10 room boot, until its first stamped snapshot. Read-only loads (`BootMode::ReadOnly`: `stored_snapshot` for the Markdown refresh and its sweep, Duplicate's `current_snapshot` when no room is live, `knot-server export-check`) also read `All` after a legacy snapshot and never stamp one, so a page nobody opens after the upgrade replays its whole surviving log on every such load, for good, where 0.9.9 read only `since(snapshot_seq)` (review round 5). Kept, not narrowed to `rev IS NOT NULL OR seq > snapshot_seq`: a read-only load then serves the same content a room would (a late-seq row included), not a page that changes once a room opens it; the log is mostly rows written since 0.9.9 kept them, and none of these loads is on the editing path. Task 11 records this cost. Pre-M1.1 GC pruned rows below each snapshot, so this is mostly rows written under 0.9.9; the legacy snapshot's content that no surviving row carries stays in the room's doc and so in every later stamped snapshot (the snapshot invariant in Global Constraints).
- Ruling: `ensure_legacy_numbered` runs only in room boot (`BootMode::Room`); read-only loads (`stored_snapshot`, `knot-server export-check`) never write — export-check promises no writes; M1.3's builder adds its own call — cost if wrong: none.
- Ruling: `ensure_legacy_numbered` returns at once when `legacy_rows = 0` or a row with `rev = legacy_rows` exists (one probe of the `(doc_id, rev)` index), so only the first boot of an unnumbered page scans its rows — without the short-circuit every boot counts `rev IS NULL` over the page's whole history, which M1.1 keeps forever; numbering assigns `1..legacy_rows` in one statement and every other row gets a rev above `legacy_rows`, so `rev = legacy_rows` exists exactly when numbering is done — cost if wrong: none (no column added).
- Ruling: the architecture's "snapshot_seq = that row's seq keeps old replicas' seq-based hydration working" does not hold for trigger-path rows during the rollout, and M1.2 accepts that — a 0.9.9 insert gets its `seq` (column default) before its BEFORE trigger waits on the heads row, so an old-pod row can get seq 100 and rev 51 while a new flush holding the lock gets seq 101 and rev 50; a 0.9.9 room booting from a stamped snapshot (seq 101, rev 50) reads `since(101)` and misses rev 51 until that room restarts. This is the old late-seq bug's class, confined to 0.9.9 pods while the rollout runs, and not limited to one edit: during migration 3 every 0.9.9 save on every page blocks in `doc_updates_assign_rev`'s heads upsert holding an already-drawn seq, and they commit in arbitrary order when migration 3 does, so many pages get seq/rev inversions at once, and a 0.9.9 room reading `since(last_applied_seq)` skips every row below a seq it already applied (`room.rs:754-756`); the same happens whenever a trigger-path insert waits behind a 0.9.10 flush on the heads row (review round 5); the upgrade notes say so — cost if wrong: editors on a 0.9.9 pod may not see edits saved by other pods while the upgrade runs (especially those made during the third migration's pause) until they reconnect, and that pod's later legacy snapshots lack them; nothing is lost (a 0.9.10 room boots from a legacy snapshot with `RowsAfter::All`); and a 0.9.10 room's seq-keyed Markdown render, task listing and search entry for such a page can stay stale until its next edit (the seq-cache Ruling above).
- Ruling: `UpdatesStore::insert_batch` and `SnapshotStore::insert` stay as provided trait methods (epoch 0, source `edit`; no rev or epoch) for the ~30 test call sites; production code calls `append` and `insert_stamped` — cost if wrong: a production caller of the old names writes epoch 0, which KH002 refuses once M3 bumps an epoch.
- Ruling: the effective update is persisted only when the apply neither parked nor drained pending content; otherwise the raw bytes (`raw_fallback`) — `encode_update_v1` omits pending content — cost if wrong: nothing lost; rows are larger.
- Ruling: the spike's "yjs client accepts the effective bytes" half gets no new yjs-side test (review round 4 offered a node check or a two-tab resend e2e) — the effective update is `encode_update_v1` of a yrs transaction, the same v1 encoder whose output every yjs client already integrates today in `JoinState`/SyncStep2 (`encode_state_as_update`) and in every server-side edit's fan-out (`observe_update_v1`); a trimmed resend is an ordinary v1 diff. The CI `ci` workflow's `playwright` job (`.github/workflows/ci.yml:104`) is a merge gate ("After M1.2"): `two-users-converge` integrates effective updates of live typing in a second yjs peer, and `ws-reconnect` (real toxiproxy, only in CI) persists an offline resend that a second tab must load — cost if wrong: a yjs decoder that rejects a yrs-trimmed diff would show only as a peer that stops converging; the `knot_room_updates_persisted_raw_total` path (raw bytes) is unaffected.
- Ruling (spike fallback): if Task 1's property test finds a history where the effective update does not apply like the raw one, `ApplyMeasured::persist` becomes the raw bytes always (today's behaviour) and Task 1 records the failing seed in the architecture deviation notes; Task 7's `reconnect_resend_persists_only_new_content` then asserts only that replaying the log equals the live doc (not trimming), and Review Focus 4's trimming expectation is dropped — M1.3 attribution reads the canonical replay's insert and delete sets, never raw bytes, so it stays correct — cost if wrong: larger rows and resent content in the log; M3 must then rely on its ingress guard alone.

---

### Task 1: Spike — the effective update applies like the raw update

**Files:**
- Modify: `crates/knot-crdt/src/engine.rs:22-42` (`ApplyMeasured`), `:141-186` (`apply_update_measuring`)
- Create: `crates/knot-crdt/tests/effective_update.rs`

**Interfaces:**
- Produces: `ApplyMeasured { changed, max_depth, rejected, persist: Vec<u8>, raw_fallback: bool }`. `persist` is empty when `!changed` or when `rejected.is_some()`; the raw update bytes with `raw_fallback = true` when the pending fingerprint changed; otherwise `txn.encode_update_v1()` taken after `txn.commit()`.

- [ ] **Step 1: Write the failing property test** in `crates/knot-crdt/tests/effective_update.rs`

Three yrs client docs (`Doc::with_client_id(1..=3)`) edit one `XmlFragment("default")` (insert paragraphs, insert and delete text inside them, format ranges) under a seeded xorshift. Each step one client delivers to the server, chosen at random among: its incremental update since its last delivery; a reconnect resend (`encode_state_as_update_v1(&server_sv)`, which carries its whole delete set); or a held-back older update delivered late (creates pending). Three server-side docs: `raw` applies every delivered byte string with `YrsEngine::apply_update`; `live` applies it with `apply_update_measuring(.., MAX_NESTING)`; `log` applies every non-empty `persist` in the same order.

The generator records each client transaction's own update (`observe_update_v1`) as well as the incremental ones, so a held-back update can be a single transaction's. Each seed also runs one fixed delete-before-insert step at a random point (review round 5): a client inserts text into a paragraph it created in transaction U1, deletes part of that text in transaction U2, the generator holds U1 back and delivers U2 alone first (its update carries only a delete set over items the server lacks, so the server parks a pending delete set and no structs), then U1.

```rust
#[test]
fn effective_update_applies_like_the_raw_update() {
    let (mut parked_structs, mut parked_deletes_only, mut trimmed) = (0, 0, 0);
    for seed in 1..=300u64 {
        // ... run 60 steps; for each measured apply on `live`, compare its store before and after:
        //   pending_update() newly set or changed            → parked_structs += 1
        //   only pending_ds() newly set or changed            → parked_deletes_only += 1
        //   either of the two                                 → assert!(m.raw_fallback, "seed {seed}")
        //   if !m.persist.is_empty() && m.persist.len() < delivered.len() { trimmed += 1 }
        assert_same_state(&raw, &live, seed);
        assert_same_state(&live, &log, seed);
    }
    assert!(parked_structs > 0, "the generator never parked structs");
    assert!(parked_deletes_only > 0, "the generator never parked a delete set alone");
    assert!(trimmed > 0, "no resend was ever trimmed");
}
```

`assert_same_state(a, b, seed)` compares, naming the seed on failure: `state_vector()`, `snapshot().delete_set` (encoded v1), the fragment's `get_string`, and the pending content itself, not only whether it is set: `store().pending_update()` as its update's `encode_v1()` and `pending_ds()` as its `encode_v1()` (each `None` or equal bytes). Each seed ends by delivering every update still held back, then asserts `assert_same_state` once more and that none of the three docs holds pending content (review round 4: two docs could otherwise both hold different pending content and pass). Also add `effective_update_of_a_resend_is_empty_or_new_only`: a client with no new edits resends its full state; `changed == false` and `persist` is empty.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo nextest run -p knot-crdt --test effective_update`
Expected: FAIL to compile (`persist`, `raw_fallback` not fields).

- [ ] **Step 3: Implement the two fields in `apply_update_measuring`** per Interfaces. Leave `apply_update_changed` unchanged.

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo nextest run -p knot-crdt --test effective_update --test nesting`
Expected: PASS. If the property test fails on a seed, stop and apply the spike-fallback ruling: `persist` = raw bytes always, keep the test with the failing seed `#[ignore]`d and its output pasted into the commit message, and note it for Task 11's deviation notes.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-crdt/src/engine.rs crates/knot-crdt/tests/effective_update.rs
git commit -m "feat(history): measure the effective update of every inbound apply"
```

---

### Task 2: Lock helper and the log columns (migration 1)

**Files:**
- Create: `migrations/20261107120000_history_log_columns.sql`
- Modify: `crates/knot-storage/tests/migrations_apply.rs` (new tests at the end; reuse `seed_ws_user` :1016, `seed_doc_with_updates` :1032)

**Interfaces:**
- Produces: SQL function `knot_lock_tables(who text, tables regclass[], lock_mode text, give_up_after interval DEFAULT '45 seconds')` (Global Constraints); columns `doc_updates.{rev, source, meta, epoch}`, `doc_snapshots.{snapshot_rev, epoch}`; constraint `doc_updates_source_check`.

- [ ] **Step 1: Write the failing tests** in `crates/knot-storage/tests/migrations_apply.rs`

```rust
#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_backs_off_instead_of_deadlocking() {
    // fresh_db; doc d. T1: BEGIN; SELECT 1 FROM documents WHERE id = d FOR UPDATE.
    // T2: BEGIN; INSERT INTO doc_updates (doc_id, update_bytes) VALUES (d, '\x01')
    //     (spawned; it blocks on T1's row lock while holding ROW EXCLUSIVE on doc_updates).
    // M: BEGIN; SELECT knot_lock_tables('t', ARRAY['documents','doc_updates']::regclass[],
    //     'SHARE ROW EXCLUSIVE', '20 seconds') (spawned).
    // wait_for_lock_waits(pool, 1); sleep 300 ms (M has retried);
    // T1: UPDATE documents SET title = 'x' WHERE id = d; COMMIT.
    // Then T2 commits and M returns.
    assert!(t2.is_ok() && m.is_ok());          // no 40P01 anywhere
    assert!(m_elapsed < Duration::from_secs(20));
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_gives_up_naming_the_holder() {
    // H: BEGIN; LOCK TABLE doc_updates IN ROW EXCLUSIVE MODE; (kept open)
    let e = /* SELECT knot_lock_tables('t', ARRAY['doc_updates']::regclass[], 'SHARE', '1 second') */;
    let msg = e.to_string();
    assert!(msg.contains("t: could not get SHARE on doc_updates within 1 s"), "{msg}");
    assert!(msg.contains(&format!("pid {h_pid}")), "{msg}");
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_refuses_an_unknown_mode() { /* 'ROW SHARE; DROP TABLE x' → error */ }

#[tokio::test(flavor = "multi_thread")]
async fn lock_helper_lone_wait_never_requests_the_target_mode() {
    // An autovacuum can't be staged on demand, so pin the source: pg_get_functiondef('knot_lock_tables'::regproc)
    // contains 'SHARE UPDATE EXCLUSIVE' in the lone-wait block, and the lone wait's LOCK statement does not
    // interpolate lock_mode (a review of the SQL, then this guard against a revert).
}

#[tokio::test(flavor = "multi_thread")]
async fn log_columns_exist_and_old_statements_still_work() {
    // fresh_db: the 0.9.9 INSERT (paste updates_store.rs:79-103 as OLD_INSERT_UPDATES_SQL),
    // `SELECT seq, doc_id, update_bytes, by_user_id, created_at FROM doc_updates ...`
    // and the 0.9.9 snapshot upsert (snapshot_store.rs:86-91) all succeed.
    // INSERT ... source = 'bogus' fails with 23514; source = 'edit' succeeds.
}
```

`wait_for_lock_waits` is `knot_test_support::locks::wait_for_lock_waits`. Keep `OLD_INSERT_UPDATES_SQL` and `OLD_SNAPSHOT_UPSERT_SQL` as consts at the top of the test file; Tasks 3 and 4 use them in the same file, and Tasks 5 and 10 copy them into their own test files (consts of one integration test are not visible to another).

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: FAIL (`knot_lock_tables` does not exist; columns missing).

- [ ] **Step 3: Write `migrations/20261107120000_history_log_columns.sql`**

Header comment in the style of the M1.1 files: why (rev, source, meta, epoch for page history; architecture link), why this migration only adds columns (ACCESS EXCLUSIVE blocks every read of the two tables, so it holds them only for catalog changes), and the lock helper's contract. The header also says why the lone wait asks for `SHARE UPDATE EXCLUSIVE` (Rulings). Then the function, `SET LOCAL lock_timeout = '5s';`, `SELECT knot_lock_tables('history_log_columns', ARRAY['doc_updates','doc_snapshots']::regclass[], 'ACCESS EXCLUSIVE');`, the two `ALTER TABLE`s.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: PASS, including every M1.1 test in the file.

- [ ] **Step 5: Commit**

```bash
git add migrations/20261107120000_history_log_columns.sql crates/knot-storage/tests/migrations_apply.rs
git commit -m "feat(history): rev, source, meta and epoch columns, and a shared lock helper"
```

---

### Task 3: Heads table, rev trigger and epoch triggers (migrations 2 and 3)

**Files:**
- Create: `migrations/20261107120001_history_heads.sql`, `migrations/20261107120002_history_heads_fk.sql`
- Modify: `crates/knot-storage/tests/migrations_apply.rs` (`expected` at :30-58 gains `"doc_history_heads"` after `"doc_history_baselines"`; new tests)
- Modify: `e2e/support/reset.ts:35-60` (`"doc_history_heads"` after `"doc_history_baselines"`)

**Interfaces:**
- Consumes: `knot_lock_tables` (Task 2).
- Produces: table `doc_history_heads`; triggers `doc_updates_assign_rev`, `doc_updates_append_only` (re-created), `doc_snapshots_epoch`; SQLSTATE `KH002`.

- [ ] **Step 1: Write the failing tests** in `crates/knot-storage/tests/migrations_apply.rs`

Use a `migrate_up_to(pool, last_version)` helper built like `history_since_is_the_moment_deletes_stopped`'s `up_to` (:1393-1402) and `fn kh002(e) -> bool` like `kh001` (:1012). Add `STAMPED_SNAPSHOT_INSERT_SQL` (below) and `HEADS_UPSERT_SQL` (the Global Constraints text, pasted as a test-local const: Task 5's crate constant is private and not visible to an integration test) beside the Task 2 consts.

```rust
#[tokio::test(flavor = "multi_thread")]
async fn old_style_inserts_are_numbered_after_the_legacy_rows() {
    // up to 20261107120000; doc D with 3 rows (legacy); full migrate.
    // heads(D) = (head_rev 3, legacy_rows 3, content_epoch 0, dirty true, last_write_at not NULL)
    // UPDATE doc_history_heads SET dirty = false, last_write_at = '2000-01-01' WHERE doc_id = D
    // (an existing row a builder has cleared: the ON CONFLICT arm must set both again);
    // two OLD_INSERT_UPDATES_SQL inserts → revs 4, 5; legacy rows keep rev NULL;
    // created_at of rev 5 >= rev 4; heads.head_rev = 5, dirty = true, last_write_at > '2000-01-01'.
    // A raw insert with explicit created_at '2026-01-01' keeps it.
    // A new doc N with no heads row: one OLD_INSERT_UPDATES_SQL insert → rev 1, heads(N) = (head_rev 1,
    // dirty true, last_write_at not NULL): the trigger path's INSERT arm (first-row Ruling).
}

#[tokio::test(flavor = "multi_thread")]
async fn old_inserts_racing_the_heads_migration_are_counted_exactly() {
    // Spike "migration ordering": up to 20261107120000; docs A, B with 5 rows each; doc C with a legacy
    // snapshot and no doc_updates rows (as the pre-M1.1 GC left a quiet page).
    // Spawn a loop running OLD_INSERT_UPDATES_SQL (1-3 rows, A and B alternately) on its own
    // connection until told to stop, and a second loop running OLD_SNAPSHOT_UPSERT_SQL on doc A
    // (recording the longest single call); meanwhile run migrate_up_to(20261107120001), read the
    // snapshot loop's longest call so far (< 1 s: migration 2 does not lock doc_snapshots), then
    // run the full migrator; stop the loops.
    // For each doc: count(rev IS NULL) == legacy_rows, and the non-NULL revs are exactly
    // legacy_rows+1 ..= head_rev, each once. Neither loop saw an error.
    // heads(C) exists = (head_rev 0, legacy_rows 0, content_epoch 0, dirty true): every page has a heads row.
    // Every seeded row's last_write_at is not NULL (the seed's now()).
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn migrations_one_to_five_run_under_live_0_9_9_traffic() {
    // Review round 5: the M1.1 deadlock class (page writes, purges, Duplicate, readers) against the real files.
    // up to 20261104120001 (0.9.9's schema); pages A, B, E, F with 5 rows each, page C with a legacy snapshot.
    // Read deadlocks0 = SELECT deadlocks FROM pg_stat_database WHERE datname = current_database().
    // Start these loops, each on its own pool (max 2), each recording every error it sees:
    //  (i)   four inserters, one per page A, B, E, F, running OLD_INSERT_UPDATES_SQL (1-3 rows) back to back,
    //        no sleep (every in-flight save holds ROW EXCLUSIVE on doc_updates and, from migration 2, on
    //        doc_history_heads);
    //  (ii)  a page-write loop on A: BEGIN; SELECT id FROM documents WHERE id = A FOR UPDATE;
    //        UPDATE documents SET title = 'w' || n WHERE id = A; COMMIT; sleep 20 ms;
    //  (iii) an archive+purge loop: create a throwaway page T, OLD_INSERT_UPDATES_SQL for T,
    //        PgDocStore::archive(ws, T, actor), PgDocStore::purge(ws, T, None); sleep 50 ms;
    //  (iv)  a 0.9.9-shaped Duplicate: BEGIN; INSERT INTO documents (seed_doc_with_updates' columns)
    //        RETURNING id; OLD_INSERT_UPDATES_SQL for the new id; SELECT id FROM documents WHERE id = A
    //        FOR UPDATE (placement locks the source's row); UPDATE documents SET sort_key = 'n' || n WHERE
    //        id = <new id>; COMMIT; sleep 50 ms (as duplicate.rs:107-170: ROW EXCLUSIVE on documents, then
    //        the trigger's heads upsert: the one live transaction that wants migration 3's first two tables);
    //  (v)   OLD_SNAPSHOT_UPSERT_SQL on A, sleep 50 ms;
    //  (vi)  a reader: SELECT count(*) FROM doc_updates WHERE doc_id = B; SELECT * FROM doc_snapshots
    //        WHERE doc_id = C; recording its longest call.
    // Let them run 1 s, then run the full migrator (migrations 20261107120000..04, all five) and time it;
    // let them run 1 s more; stop them; close every loop pool (backend exit flushes its statistics).
    // The migrator returns Ok within 30 s (migration 3's heads-first helper call gets its locks against
    // tight inserters, well before its 45 s give-up).
    // No loop saw any error (in particular no 40P01 and no KH001/KH002/23503).
    // Poll for up to 5 s (pg_stat_clear_snapshot() before each read): pg_stat_database.deadlocks ==
    //   deadlocks0 (this also catches a 40P01 that purge's retry_on_deadlock retried away).
    // The reader's longest call < 1 s (only migration 1's ACCESS EXCLUSIVE attempts, each ≤ 100 ms of
    //   waiting plus catalog-only ALTERs, block reads).
    // For each of A, B, E, F: count(rev IS NULL) == legacy_rows and the non-NULL revs are exactly
    //   legacy_rows+1 ..= head_rev, each once.
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_are_refused_once_the_epoch_moves() {
    // UPDATE doc_history_heads SET content_epoch = 1 WHERE doc_id = D;
    // OLD_INSERT_UPDATES_SQL → KH002; INSERT with rev = head_rev+1, epoch = 0 → KH002;
    // with epoch = 1 → ok. OLD_SNAPSHOT_UPSERT_SQL (no epoch column, so epoch NULL) → KH002;
    // STAMPED_SNAPSHOT_INSERT_SQL = "INSERT INTO doc_snapshots (doc_id, snapshot_seq, state_bytes,
    // state_vector, snapshot_rev, epoch) VALUES ($1, $2, $3, $4, $5, $6)" with epoch 0 → KH002, epoch 1 → ok.
    // While content_epoch = 0 (another doc): OLD_SNAPSHOT_UPSERT_SQL → ok.
}

#[tokio::test(flavor = "multi_thread")]
async fn an_old_snapshot_upsert_leaves_a_stamped_snapshot_alone() {
    // STAMPED_SNAPSHOT_INSERT_SQL at (D, seq S, bytes X, rev R, epoch 0);
    // OLD_SNAPSHOT_UPSERT_SQL at (D, seq S, bytes Y) succeeds (0.9.9 sees no error)
    // and the row still holds X, rev R, epoch 0. A stamped upsert (Task 5's statement shape:
    // ON CONFLICT ... SET state_bytes, state_vector, created_at, snapshot_rev, epoch) at S
    // with bytes Z also leaves X. OLD_SNAPSHOT_UPSERT_SQL on a legacy row (rev NULL) at seq T
    // still replaces its bytes, and a stamped upsert at T promotes it (bytes, rev, epoch set).
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_and_trigger_path_writes_to_a_purged_page_fail_with_23503() {
    // D purged (DELETE FROM documents); OLD_INSERT_UPDATES_SQL → 23503; OLD_SNAPSHOT_UPSERT_SQL → 23503;
    // no heads row for D was created.
}

#[tokio::test(flavor = "multi_thread")]
async fn a_purge_racing_a_trigger_path_insert_never_deadlocks() {
    // T: BEGIN; OLD_INSERT_UPDATES_SQL for D (holds documents KEY SHARE, then the heads row).
    // P (spawned): DELETE FROM documents WHERE id = D → waits (wait_for_lock_waits(1)).
    // T commits; P commits. Reverse order: P holds the delete; T (spawned) waits; P commits;
    // T fails with 23503. Neither side ever sees 40P01; both finish within 5 s.
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_insert_waits_for_an_epoch_bump_then_fails() {
    // Spike "row-lock recheck". E: BEGIN; SELECT ... FROM doc_history_heads WHERE doc_id = D FOR UPDATE;
    //   UPDATE doc_history_heads SET content_epoch = 1 WHERE doc_id = D;
    // S (spawned): snapshot insert with epoch 0 → blocks (wait_for_lock_waits(1)).
    // W (spawned): the test-local HEADS_UPSERT_SQL with $2 = 1 → blocks.
    // E commits. S fails with KH002; W returns content_epoch = 1.
}

#[tokio::test(flavor = "multi_thread")]
async fn append_only_allows_only_the_legacy_rev_numbering() {
    // Legacy rows need the pre-heads schema (on a fully migrated fresh_db the trigger numbers every insert):
    // migrate_up_to(20261107120000); insert two rows for D; full migrate.
    // on a legacy row (rev NULL): UPDATE SET rev = 1 → ok; UPDATE SET rev = 2 again → KH001;
    // on another legacy row: UPDATE SET rev = 9, by_user_id = <other user> → KH001;
    // UPDATE SET by_user_id = ... → KH001; UPDATE SET created_at = ... → KH001;
    // UPDATE SET source = 'edit' → KH001. M1.1's purge-cascade and flagged-rewrite tests stay green.
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flagged_rewrite_may_write_a_snapshot_at_any_epoch() {
    // Review round 3. heads(D).content_epoch = 0; a stamped snapshot at (D, seq S, rev R, epoch 0) exists.
    // Without the flag, on a brand-new connection (sqlx::PgConnection::connect, never SET knot.history_rewrite, so
    // current_setting(.., true) is NULL; review round 4): STAMPED_SNAPSHOT_INSERT_SQL with epoch 1 → KH002, and
    // OLD_SNAPSHOT_UPSERT_SQL at S with other bytes leaves the stamped row's bytes alone.
    // BEGIN; SET LOCAL knot.history_rewrite = 'on'; STAMPED_SNAPSHOT_INSERT_SQL with epoch 1 at a new seq → ok;
    // an upsert at S with other bytes and epoch 1 replaces the stamped row (immutability skipped too); COMMIT.
    // With the flag on and D purged: the insert still fails with 23503 (step (ii) always runs).
}

#[tokio::test(flavor = "multi_thread")]
async fn doc_updates_columns_are_the_ones_rule_b_compares() {
    // information_schema.columns for doc_updates, sorted ==
    // ["by_user_id","created_at","doc_id","epoch","meta","rev","seq","source","update_bytes"].
    // Failure message: "a new doc_updates column must be added to rule (b) in doc_updates_append_only".
}

#[tokio::test(flavor = "multi_thread")]
async fn heads_fk_migration_drops_orphans_and_cascades() {
    // up to 20261107120001; doc D with rows; DELETE FROM documents WHERE id = D
    // (cascades doc_updates; heads row stays: no FK yet); full migrate;
    // heads(D) is gone; the FK exists; a new doc's heads row is deleted with the doc.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: FAIL (`doc_history_heads` missing).

- [ ] **Step 3: Write `migrations/20261107120001_history_heads.sql`**

Header: why heads (rev allocation in commit order; the row lock is the order), why documents before heads in the trigger's `NEW.rev IS NULL` arm (a purge must make writers wait, not deadlock) and why the `NEW.rev IS NOT NULL` arm takes no page lock (new code took it in statement (0)), why trigger-before-seed (no row slips between count and trigger), why the seed reads `documents` (every page, including pages whose rows were pruned, gets a heads row for M1.3's builder; ACCESS SHARE only), why no FK and no snapshot trigger here (Rulings), what waits until COMMIT (edit saves, and page writes that insert updates: duplicates; 0.9.9 rooms block where they await a save inline). Order: `SET LOCAL lock_timeout`; `SELECT knot_lock_tables('history_heads', ARRAY['doc_updates']::regclass[], 'SHARE ROW EXCLUSIVE')`; `CREATE TABLE doc_history_heads` + dirty index; `doc_updates_assign_rev` function and trigger; seed; append-only function and trigger.

- [ ] **Step 4: Write `migrations/20261107120002_history_heads_fk.sql`**

`SET LOCAL lock_timeout`; `SELECT knot_lock_tables('history_heads_fk', ARRAY['doc_history_heads','documents','doc_snapshots']::regclass[], 'SHARE ROW EXCLUSIVE')`; `DELETE FROM doc_history_heads h WHERE NOT EXISTS (SELECT 1 FROM documents d WHERE d.id = h.doc_id)`; `ALTER TABLE doc_history_heads ADD CONSTRAINT doc_history_heads_doc_fk FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE`; `doc_snapshots_epoch` function and trigger (Global Constraints). Header: why `doc_history_heads` comes first in the helper call (its 100 ms wait goes to the table every in-flight 0.9.9 save holds, so new saves queue behind the request and the in-flight ones finish; `documents` and `doc_snapshots` then get `NOWAIT`; the helper never waits while holding a lock, so the table order is no deadlock risk, and the app's row-lock order is unaffected); page writes, edit saves and snapshot writes pause for the orphan delete and the FK validation (one pass over the heads rows; on 0.9.9 pods an open page waiting on a task toggle, accept, import or snapshot is held up too, Global Constraints); why stamped snapshots are immutable; why the `knot.history_rewrite` flag skips the epoch check; why `rewriting` is read through `coalesce(.., '')` (an unset custom setting reads as NULL, and `NOT NULL` is not true).

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: PASS.

- [ ] **Step 6: Add `"doc_history_heads"` to `TABLES` in `e2e/support/reset.ts`.**

- [ ] **Step 7: Commit**

```bash
git add migrations/20261107120001_history_heads.sql migrations/20261107120002_history_heads_fk.sql crates/knot-storage/tests/migrations_apply.rs e2e/support/reset.ts
git commit -m "feat(history): per-page head row that numbers every update in commit order"
```

---

### Task 4: The `(doc_id, rev)` unique index (migrations 4 and 5)

**Files:**
- Create: `migrations/20261107120003_history_rev_index.sql`, `migrations/20261107120004_history_rev_index_check.sql`
- Modify: `crates/knot-storage/tests/migrations_apply.rs`

**Interfaces:**
- Produces: index `doc_updates_doc_rev_uq`; Task 5's `rows_after(.., RowsAfter::Rev(_))` relies on it.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn rev_index_is_built_valid_and_unique() {
    // fresh_db: pg_index.indisvalid for doc_updates_doc_rev_uq is true;
    // inserting a second row with an existing (doc_id, rev) and the right epoch → 23505.
}

#[test]
fn no_transaction_migration_is_one_statement() {
    let sql = include_str!("../../../migrations/20261107120003_history_rev_index.sql");
    assert!(sql.starts_with("-- no-transaction"));
    assert_eq!(sql.matches(';').count(), 1, "sqlx sends the file as one query");
}

#[tokio::test(flavor = "multi_thread")]
async fn index_check_rebuilds_an_invalid_index() {
    // fresh_db (superuser): UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'doc_updates_doc_rev_uq'::regclass;
    // run include_str!("…/20261107120004_history_rev_index_check.sql") in a transaction; commit;
    // indisvalid is true again. Then DROP INDEX doc_updates_doc_rev_uq; run it again; the index exists and is valid.
}

#[tokio::test(flavor = "multi_thread")]
async fn index_check_backs_off_instead_of_deadlocking() {
    // Three sessions, as Task 2's lock_helper_backs_off_instead_of_deadlocking (a two-session shape cannot fail:
    // a reader that already holds ACCESS SHARE jumps M's queued request and is granted at once).
    // Index marked invalid as above. T1: BEGIN; SELECT 1 FROM documents WHERE id = D FOR UPDATE.
    // T2 (spawned): BEGIN; raw INSERT into doc_updates for D (rev NULL, the trigger path) → holds ROW EXCLUSIVE
    //   on doc_updates and blocks on T1's row lock in the trigger's KEY SHARE (wait_for_lock_waits(1)).
    // M (spawned): the check file in a transaction → its 100 ms attempts keep failing on T2's lock; sleep 300 ms.
    // T1: SELECT count(*) FROM doc_updates WHERE doc_id = D (needs ACCESS SHARE, which a waiting ACCESS EXCLUSIVE
    //   would queue behind) → completes within 1 s; COMMIT. T2 then commits; M commits.
    // No 40P01 anywhere; every session finished within 10 s; index valid.
}

#[tokio::test(flavor = "multi_thread")]
async fn index_check_takes_no_lock_when_the_index_is_valid() {
    // H: BEGIN; LOCK TABLE doc_updates IN ROW EXCLUSIVE MODE (kept open);
    // the check file runs and commits within 1 s.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: FAIL (index missing, files missing).

- [ ] **Step 3: Write both migration files** per Global Constraints. `20261107120003` is `-- no-transaction`, then at most a few `--` comment lines with no `;` in them (e.g. `-- See 20261107120004 for why and what happens if this is interrupted`), then the single statement; the semicolon-count test enforces this. The check file's header carries the explanation: why CONCURRENTLY (page loads and saves continue), what CIC waits for (transactions older than itself, so not during `pg_dump`, and not while a pod is starting: a starting knot-server waits on sqlx's migration advisory lock with a live snapshot, and the deadlock detector may abort the CIC), when the check does anything (only after an interrupted or aborted index build), and that it then pauses page loads and saves for one scan of `doc_updates`. The repair runs as `DO $$ ... $$` with `PERFORM knot_lock_tables('history_rev_index_check', ARRAY['doc_updates']::regclass[], 'ACCESS EXCLUSIVE')` inside the `IF`.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-storage --test migrations_apply`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add migrations/20261107120003_history_rev_index.sql migrations/20261107120004_history_rev_index_check.sql crates/knot-storage/tests/migrations_apply.rs
git commit -m "feat(history): unique (doc_id, rev) index, built concurrently"
```

---

### Task 5: Storage API — page-then-heads append, rev reads, legacy numbering, stamped snapshots, purge retry

**Files:**
- Modify: `crates/knot-storage/src/updates_store.rs` (whole file, 172 lines: trait :28-49, `INSERT_UPDATES_SQL` :79-103, impl :116-172)
- Modify: `crates/knot-storage/src/snapshot_store.rs` (`DocSnapshot` :11-18, trait :34-63, `insert` :78-100, `latest` :102-117)
- Modify: `crates/knot-storage/src/duplicate.rs` (`PageCopy` :25-39, row insert :132-140), `crates/knot-storage/src/lib.rs:48-50, 58` (re-exports; `#[doc(hidden)] pub mod testing;`)
- Create: `crates/knot-storage/src/testing.rs` (`db_error`, Interfaces)
- Modify: `crates/knot-storage/src/doc_store.rs:1007-1032` (`purge` becomes `placement::retry_on_deadlock(|| self.purge_once(workspace_id, doc_id, actor))`; `purge_due` in `knot-server/src/archive.rs:257` calls `purge`, so it is covered)
- Modify: `crates/knot-storage/src/doc_store.rs:62-107` (`DocStoreError` gains `#[error("internal: {0}")] Internal(String)`; it has no internal variant today, and the server's matches on it end in `Err(e) =>` fallbacks, e.g. `routes/api/docs.rs:878`; `grep -rn 'DocStoreError::' crates/knot-server/src` to confirm none is exhaustive)
- Modify: `crates/knot-server/src/duplicate.rs` (the private `struct Page` :101-112 gains `source_id: Uuid, source_title: String`, set from the outline tuple :286-301, which gains the source title: `source.title` for the top page, not the "(copy)" title, and `row.title` for subpages; the `PageCopy` built at :358-370 copies both)
- Modify (test doubles, `insert_batch` → `append`, `insert` → `insert_stamped`, new required methods): `crates/knot-crdt/src/room.rs:1020, 1046, 1074, 1099, 2371, 2400, 2504`, `crates/knot-crdt/tests/nesting.rs:331, 361, 1076`, `crates/knot-server/tests/suggestions_integration.rs:1617`. Re-check with `grep -rn 'UpdatesStore for\|SnapshotStore for' crates` first. `FailingUpdates` (room.rs:2371) must return a permanent error so the writer goes Fatal at once instead of retrying; use `EpochChanged` (the clearest; `PoolClosed`, today's error, is permanent too, Global Constraints). Its reads stay empty here; Task 6 makes them return the seed. `FlakyReads` (room.rs:2499) fails `rows_after` and `head` (not just `since`) while `fail_since` is set, so `applied_check_unanswered_when_catch_up_fails` (room.rs:2540) keeps exercising the failed read once catch-up reads by rev; rename the flag `fail_reads`.
- Test: `crates/knot-storage/tests/updates.rs`, `crates/knot-storage/tests/snapshots.rs`, `crates/knot-storage/tests/duplicate.rs`, new `crates/knot-storage/tests/rev_allocation.rs`

**Interfaces:**
- Produces (all re-exported from `knot_storage`):
  - `pub enum UpdateSource { Edit, Task, Suggestion, Restore, RestoreFallback, Import, Template, Copy }` (`Debug, Clone, Copy, PartialEq, Eq`), `fn as_str(self) -> &'static str` (the SQL spellings), `impl FromStr` (unknown → `Err`).
  - `pub struct NewUpdate { pub by_user_id: Option<Uuid>, pub bytes: Vec<u8>, pub source: UpdateSource, pub meta: Option<serde_json::Value> }`
  - `pub struct Persisted { pub seq: i64, pub rev: i64 }`
  - `DocUpdate` gains `rev: Option<i64>, source: Option<UpdateSource>, meta: Option<serde_json::Value>, epoch: Option<i32>`.
  - `pub struct HistoryHead { pub head_rev: i64, pub legacy_rows: i64, pub content_epoch: i32 }` (`Default`: all zero, for a page with no row).
  - `pub enum RowsAfter { Rev(i64), All }`
  - `UpdatesStoreError` gains `EpochChanged { room: i32, stored: i32 }`, `DocGone`, `LegacyCountMismatch { legacy_rows: i64, unnumbered: i64 }`, `pub fn is_permanent(&self) -> bool` and `pub fn shrinks_batch(&self) -> bool` (Global Constraints).
  - `#[doc(hidden)] pub mod testing { pub fn db_error(sqlstate: &'static str) -> sqlx::Error; pub use crate::updates_store::CANCEL_ABANDONED_SQL; }` in `crates/knot-storage/src/testing.rs` (`CANCEL_ABANDONED_SQL` is the drop guard's statement, `pub(crate) const` in `updates_store.rs`, re-exported so `rev_allocation.rs` tests the exact text the guard runs): a `sqlx::Error::Database` wrapping a private struct that implements `sqlx::error::DatabaseError` with that `code()`, so doubles in other crates (Task 6's writer tests) can return `UpdatesStoreError::Sqlx(db_error("57014"))`.
  - `impl From<UpdatesStoreError> for DocStoreError` (in `doc_store.rs` or `duplicate.rs`): `Sqlx(e) => DocStoreError::Sqlx(e)` (the inner error kept as is, so `retry_on_deadlock`'s `DocStoreError::Sqlx(sqlx::Error::Database(40P01))` match still fires for a deadlock inside the copy's row insert); `EpochChanged`, `DocGone`, `LegacyCountMismatch` → the new `DocStoreError::Internal(e.to_string())` (Files).
  - `UpdatesStore`: required `append(&self, doc_id: Uuid, epoch: i32, rows: &[NewUpdate]) -> Result<Vec<Persisted>, UpdatesStoreError>` (input order = rev order), `rows_after(&self, doc_id, from: RowsAfter) -> Result<Vec<DocUpdate>, _>`, `head(&self, doc_id) -> Result<HistoryHead, _>`, `ensure_legacy_numbered(&self, doc_id) -> Result<u64, _>` (rows numbered); provided `insert_batch(&self, doc_id, &[(Option<Uuid>, Vec<u8>)]) -> Result<Vec<i64>, _>` (= `append(.., 0, Edit, meta None)`, seqs); `since`, `max_seq` unchanged.
  - `pub async fn append_in_tx(conn: &mut sqlx::PgConnection, doc_id: Uuid, epoch: i32, rows: &[NewUpdate]) -> Result<Vec<Persisted>, UpdatesStoreError>` (used by `append` inside `crate::begin`, and by Duplicate).
  - `pub struct SnapshotStamp { pub snapshot_seq: i64, pub snapshot_rev: Option<i64>, pub epoch: Option<i32> }`; `DocSnapshot` gains `snapshot_rev: Option<i64>, epoch: Option<i32>`; `SnapshotStore`: required `insert_stamped(&self, doc_id, stamp: SnapshotStamp, state_bytes: &[u8], state_vector: &[u8])`, provided `insert(doc_id, seq, bytes, sv)` (rev and epoch `None`); `SnapshotStoreError::EpochChanged` (from KH002), `SnapshotStoreError::DocGone` (from 23503).
  - `PageCopy` gains `pub source_id: Uuid, pub source_title: String`.
  - `pub const APPEND_STATEMENT_TIMEOUT: Duration = Duration::from_secs(8)`; `PgUpdatesStore::with_statement_timeout(self, Duration) -> Self` (builder, tests).

`append_in_tx`, in order: (0) `DOC_KEY_SHARE_SQL` (no row → `DocGone`); (1) `HEADS_UPSERT_SQL` (Global Constraints) with `$2 = rows.len()`; (2) if `content_epoch != epoch` return `EpochChanged` (the caller's transaction rolls back); (3) `INSERT_UPDATES_SQL` rewritten to insert `rev = base + ord`, `source` and `meta` from `text[]` arrays (`meta` cast `::jsonb`), `epoch`, `created_at = clock_timestamp()`, `RETURNING seq, rev, by_user_id`, keeping the `contrib` CTE unchanged, and selecting `seq, rev ORDER BY rev`. Update the doc comment (:51-78): the page row (KEY SHARE) and then the heads row lock come first, so rev order is commit order, a purge makes the flush wait instead of deadlocking, and the lock order is always documents → heads → `doc_contributors`.

`rows_after`: `Rev(r)`: `WHERE doc_id = $1 AND rev > $2 ORDER BY rev`. `All`: `WHERE doc_id = $1 ORDER BY rev NULLS FIRST, seq` (legacy rows predate every numbered one, so this is commit order even before numbering).

`PgUpdatesStore::append`: `crate::begin` (cancellation-safe, `tests/cancel_safety.rs:261, 291`), then `SELECT pg_backend_pid(), now(), set_config('lock_timeout', $1, true), set_config('statement_timeout', $1, true)` (Global Constraints; the pid and transaction start go to the drop guard), `append_in_tx`, COMMIT, all under a drop guard `CancelOnDrop { pool, backend: Option<(i32, DateTime<Utc>)>, armed: bool }` that is disarmed once COMMIT returns (Ok or Err); its `Drop`, when still armed with a backend and `tokio::runtime::Handle::try_current()` is `Ok` (no runtime, e.g. during shutdown: skip the cancel, never panic inside `Drop`; Postgres's own `statement_timeout` still bounds the attempt), spawns on that handle `tokio::time::timeout(Duration::from_secs(2), sqlx::query(CANCEL_ABANDONED_SQL).bind(pid).bind(xact_start).execute(&pool))` with `CANCEL_ABANDONED_SQL = "SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid = $1 AND xact_start = $2"` (`$2` bound as the `DateTime<Utc>` read from `now()`, microsecond-exact, never re-derived) and ignores the result (matching `xact_start` means it cancels only our own still-open transaction, never a later one on the same pooled connection). `PgUpdatesStore::with_statement_timeout(Duration)` overrides `APPEND_STATEMENT_TIMEOUT` (tests).

`ensure_legacy_numbered`, in one transaction opened with `crate::begin` (room boot runs inside `acquire`, which an HTTP handler can drop on its 30 s `TimeoutLayer`; a cancelled `pool.begin()` could strand a connection holding row locks on the page's whole legacy log, `tests/cancel_safety.rs:261`): read `legacy_rows` from heads without a lock (no row or 0 → `Ok(0)`); `SELECT EXISTS (SELECT 1 FROM doc_updates WHERE doc_id = $1 AND rev = legacy_rows)` → true → `Ok(0)` (numbering done; Rulings). Only when numbering looks needed, take the locks in the global order: `DOC_KEY_SHARE_SQL` (no row → `DocGone`, which boot maps to `BootError::Store`; a purge then waits on the page row instead of meeting the numbering on `doc_updates` rows), then `SELECT legacy_rows FROM doc_history_heads WHERE doc_id = $1 FOR NO KEY UPDATE` (serializes two replicas numbering the same page, and appends and trigger-path inserts wait the one statement it takes), then the `rev = legacy_rows` probe again (→ `Ok(0)` when the other replica finished first); else `count(*) FILTER (WHERE rev IS NULL)`; 0 → `Ok(0)`; unnumbered ≠ `legacy_rows` → `tracing::error!(doc_id, legacy_rows, unnumbered, "legacy rows do not match the count taken at upgrade")`, `knot_history_legacy_mismatch_total` +1, `LegacyCountMismatch`; else `UPDATE doc_updates d SET rev = n.rn FROM (SELECT seq, row_number() OVER (ORDER BY seq) AS rn FROM doc_updates WHERE doc_id = $1 AND rev IS NULL) n WHERE d.seq = n.seq AND d.rev IS NULL`. Under the heads row lock no other numbering touches the rows, so the UPDATE's row-lock order cannot deadlock with another numbering.

`insert_stamped` upsert: `ON CONFLICT (doc_id, snapshot_seq) DO UPDATE SET state_bytes, state_vector, created_at = now(), snapshot_rev = EXCLUDED.snapshot_rev, epoch = EXCLUDED.epoch` (the trigger skips it when the existing row is stamped, and promotes a legacy row otherwise); `latest` orders `snapshot_rev DESC NULLS LAST, snapshot_seq DESC`. Map SQLSTATE 23503 on a snapshot write to `SnapshotStoreError::DocGone` (room: evict quietly, the page is gone).

- [ ] **Step 1: Write the failing tests**

In `crates/knot-storage/tests/updates.rs`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn append_assigns_consecutive_revs_and_stores_labels() {
    // append(doc, 0, [edit by u, task by u, suggestion by v with meta {"suggestion_id": s, "accepted_by": u}])
    // → revs [1, 2, 3], seqs ascending; rows_after(Rev(0)) returns them with source/meta/epoch 0/by_user_id;
    // head() == HistoryHead { head_rev: 3, legacy_rows: 0, content_epoch: 0 }; heads.dirty;
    // heads.last_write_at IS NOT NULL (the flush path's INSERT arm sets it; first-row Ruling).
    // UPDATE doc_history_heads SET dirty = false, last_write_at = '2000-01-01' WHERE doc_id = doc
    // (as M1.3's builder will; the first append created the row dirty through the INSERT arm, so only this
    // pins the ON CONFLICT arm);
    // second append of 2 rows → revs [4, 5]; heads.dirty = true; heads.last_write_at > '2000-01-01'.
}

#[tokio::test(flavor = "multi_thread")]
async fn append_with_a_stale_epoch_writes_nothing() {
    // append(doc, 0, [one row]) first (so the heads row exists; on a fresh doc the UPDATE would match nothing
    // and the append would succeed for the wrong reason);
    // UPDATE doc_history_heads SET content_epoch = 1 (assert 1 row affected); append(doc, 0, ..) → Err(EpochChanged { room: 0, stored: 1 });
    // is_permanent(); no further row inserted; head_rev unchanged (1).
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_after_all_returns_legacy_rows_by_seq_then_revs() {
    // Legacy rows (rev NULL) can only predate the heads migration: migrate up to 20261107120000
    // (a filtered Migrator, as migrations_apply.rs does), insert 4 rows, run the full migrator,
    // then append 2 new rows (revs 5, 6).
    // All → legacy rows 1..=4 (rev NULL, by seq), then revs 5, 6.
    // ensure_legacy_numbered → Ok(4); All → revs 1..=6 with the legacy rows numbered by seq;
    // second call Ok(0). A page with legacy_rows = 0 (new doc, one append) → Ok(0).
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_numbering_count_mismatch_is_reported() {
    // legacy page as above (4 rows); UPDATE doc_history_heads SET legacy_rows = 3;
    // ensure_legacy_numbered → Err(LegacyCountMismatch { legacy_rows: 3, unnumbered: 4 }), is_permanent; no row numbered.
}

#[tokio::test(flavor = "multi_thread")]
async fn error_classification() {
    // KH002 from a raw insert with a wrong epoch → is_permanent;
    // KH001 from an UPDATE doc_updates SET by_user_id = .. → is_permanent;
    // 23505 from a raw insert repeating an existing (doc_id, rev) with the right epoch → is_permanent;
    // append to a random doc id → DocGone → permanent;
    // UpdatesStoreError::Sqlx(sqlx::Error::PoolTimedOut) → transient; PoolClosed → permanent;
    // a 40P01 database error → transient; 55P03 and 57014 (see rev_allocation.rs) → transient.
    // Through knot_storage::testing::db_error: "22P02", "42703", "XX000" → permanent; "53300" → transient;
    // sqlx::Error::Decode(..), RowNotFound → permanent.
    // shrinks_batch: "57014" and "54000" → true; "55P03", "40P01", PoolTimedOut, "22P02" → false.
}
```

New `crates/knot-storage/tests/rev_allocation.rs` (spike "rev allocation under concurrency"):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn concurrent_appends_get_gapless_commit_ordered_revs() {
    // Two PgUpdatesStore on two separate pools (replicas A, B) append 300 batches of 1-5 rows each to doc D,
    // alternating authors u1/u2 (same user on both replicas); task C runs OLD_INSERT_UPDATES_SQL (copy the const)
    // 300 times with 1-3 rows; task R polls `SELECT rev FROM doc_updates WHERE doc_id = $1 ORDER BY rev`
    // and asserts every result is exactly 1..=n.
    // After join: no task returned an error (a deadlock is 40P01); revs == 1..=N, N = count(*) = head_rev;
    // created_at is non-decreasing in rev order. eprintln! p50/p99 flush latency per path.
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_racing_an_append_never_deadlocks() {
    // Setup: D is archived and is its own archive root (PgDocStore::archive(ws, D, actor)); `purge` refuses a live page
    // (NotArchived) and a descendant (NotArchiveRoot) in `lock_archive_root` (doc_store.rs:1196-1215).
    // Order 1 — W first: W: BEGIN; append_in_tx steps (0) and (1) by hand (page KEY SHARE, heads row).
    //   P (spawned): PgDocStore::purge(ws, D, None) → waits in lock_archive_root's SELECT … FOR UPDATE on
    //   W's KEY SHARE (wait_for_lock_waits(1)).
    //   W: step (3); COMMIT. P then succeeds (the cascade removes W's rows and the heads row).
    // Order 2 — P first: P: BEGIN; DELETE FROM documents WHERE id = D (uncommitted).
    //   W (spawned): append(D, ..) → waits on step (0); P commits; W returns Err(DocGone), is_permanent.
    // In both orders: no 40P01 on either side, both finish within 5 s, purge never returns an error.
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_racing_a_snapshot_write_never_deadlocks() {
    // D archived as above.
    // Order 1 — snapshot first (insert_stamped is one autocommit statement and cannot be held open, so stage it by
    //   hand): S: BEGIN; STAMPED_SNAPSHOT_INSERT_SQL (copied from Task 3; the trigger takes KEY SHARE on the page,
    //   then on the heads row). P (spawned): PgDocStore::purge(ws, D, None) → waits (wait_for_lock_waits(1)).
    //   S: COMMIT. P then succeeds.
    // Order 2 — P first: P: BEGIN; DELETE FROM documents WHERE id = D (uncommitted).
    //   insert_stamped(D, ..) (spawned) → waits; P commits; insert_stamped returns SnapshotStoreError::DocGone.
    // No 40P01 on either side; both finish within 5 s.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_legacy_numbering_never_deadlocks() {
    // Review round 4. Ten legacy pages (migrate up to 20261107120000, 500 rows each, full migrate), one round per
    // page. Two PgUpdatesStore on two pools call ensure_legacy_numbered(D) at the same moment (a Barrier).
    // Both return Ok; the two results sum to legacy_rows (one numbered everything, the other found it done); no
    // 40P01; revs == 1..=500 by seq.
    // Then five more legacy pages, each archived, with a third task purging it (PgDocStore::purge) at the same
    // moment: no side sees 40P01; numbering returns Ok or Err(DocGone); purge returns Ok.
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_append_gives_up_inside_postgres() {
    // Review round 3, critical. Store on its own pool (max 4 connections),
    // with_statement_timeout(1 s). L: BEGIN; SELECT 1 FROM doc_history_heads WHERE doc_id = D FOR UPDATE (kept open
    // for 12 s). A loop calls append(D, 0, [one row]) under tokio::time::timeout(10 s) (the writer's attempt_timeout)
    // for 10 s: every call returns Err within ~1.5 s, with SQLSTATE 55P03 or 57014, and is transient (never the
    // timeout); a sampler polls every 100 ms: pool.size() - pool.num_idle() <= 1 and pg_stat_activity shows at most
    // one backend of this pool (application_name set on its connect options) waiting on a lock.
    // Release L; one more append succeeds; exactly one row per successful call, revs gapless.
}

#[tokio::test(flavor = "multi_thread")]
async fn an_abandoned_append_is_cancelled_in_postgres() {
    // Review round 3, critical. As above but with_statement_timeout(30 s) and the caller's timeout 1 s, so every
    // attempt is abandoned by the caller, not ended by Postgres: 8 abandoned calls in a row. Within 3 s of each
    // abandonment pg_stat_activity shows no backend of this pool still waiting on the heads row (the drop guard's
    // pg_cancel_backend), and pool.size() - pool.num_idle() never exceeds 2 (the cancel's own connection).
    // Release L; one append succeeds; count(*) for D == 1 (no abandoned attempt committed).
}

#[tokio::test(flavor = "multi_thread")]
async fn the_drop_guard_cancel_never_hits_a_reused_connection() {
    // Review round 5: the guard must not send 57014 to unrelated work on the same pooled connection.
    // Pool P1 with max_connections 1. On it: BEGIN; SELECT pg_backend_pid(), now(); COMMIT → keep (pid, t_old).
    // Spawn on P1 (so on the same backend): BEGIN; SELECT pg_backend_pid(), now() → (pid2, t_new);
    //   SELECT pg_sleep(2); COMMIT. Assert pid2 == pid.
    // From a second pool P2, once pg_stat_activity shows pid in pg_sleep: run
    //   knot_storage::testing::CANCEL_ABANDONED_SQL bound to (pid, t_old) → no row, or false.
    // The sleeping transaction completes Ok (no 57014).
    // Positive control, same setup again: the statement bound to (pid2, t_new) → true, and the sleep fails
    //   with 57014 (the match is not vacuous: the bound timestamp equals xact_start exactly).
}
```

In `crates/knot-storage/tests/snapshots.rs`: `insert_stamped_round_trips_rev_and_epoch`, `latest_prefers_the_highest_snapshot_rev` (legacy seq 900 vs stamped seq 500 rev 7 → the stamped one), `insert_stamped_promotes_a_legacy_row_but_never_rewrites_a_stamped_one` (legacy row at seq S → stamped upsert at S sets bytes, rev, epoch; a second stamped upsert at S with other bytes → the first bytes stay), `stale_epoch_snapshot_is_epoch_changed`. In `crates/knot-storage/tests/duplicate.rs`: `copied_pages_get_a_copy_sourced_first_row` (each page: rev 1, source `copy`, epoch 0, meta `{"source_doc_id", "source_title"}`, heads row `head_rev = 1`).

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-storage`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** per Interfaces. Map SQLSTATE KH002 on snapshot inserts to `SnapshotStoreError::EpochChanged`. In `apply_once` (duplicate.rs:132-140) call `append_in_tx(&mut tx, page.new_id, 0, &[NewUpdate { by_user_id: Some(plan.actor), bytes: page.state.clone(), source: Copy, meta: Some(json!({"source_doc_id": page.source_id, "source_title": page.source_title})) }]).await?` (the `?` goes through the `From<UpdatesStoreError> for DocStoreError` above); `DuplicateOutcome.pages` keeps `(Uuid, seq)`. Split `purge` into `purge_once` (today's body) wrapped by `retry_on_deadlock`. In server `duplicate.rs`, `source_title` is the source page's own title (`source.title` for the top page, not the "(copy)" title; `row.title` for subpages). Update the test doubles (they assign `rev = seq` and return empty reads where they did before).

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-storage && cargo nextest run -p knot-crdt --lib && cargo nextest run -p knot-server --test duplicate_integration --test archive_integration && cargo nextest run -p knot-crdt -p knot-server --no-run`
Expected: PASS (the Duplicate insert path, the purge retry and the adapted room doubles are exercised here, not two tasks later); everything compiles.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-storage crates/knot-crdt/src/room.rs crates/knot-crdt/tests/nesting.rs crates/knot-server/src/duplicate.rs crates/knot-server/tests/suggestions_integration.rs
git commit -m "feat(history): heads-first append with revs, labels and epochs"
```

---

### Task 6: Writer — ordered retry, Fatal, overflow, server-job deadline, drain

**Files:**
- Modify: `crates/knot-crdt/src/writer.rs` (whole file, 121 lines), `crates/knot-crdt/src/lib.rs:33` (re-export `WriterEvent`, `WriterLimits`, `WriterDone`; `#[cfg(test)] mod test_metrics;`)
- Modify: `crates/knot-crdt/Cargo.toml` (`serde_json.workspace = true` under `[dependencies]`; tokio a second time, under `[dev-dependencies]`, as `tokio = { workspace = true, features = ["test-util"] }`, which only adds `test-util` to the `[dependencies]` entry), `Cargo.lock` (knot-crdt's entry gains `serde_json`; Global Constraints: the image build uses `--locked`)
- Create: `crates/knot-crdt/src/test_metrics.rs` (`CountingRecorder`, Global Constraints)
- Test: unit tests at the end of `crates/knot-crdt/src/writer.rs`

**Interfaces:**
- Consumes: `UpdatesStore::append`, `UpdatesStoreError::is_permanent`, `NewUpdate`, `UpdateSource`, `Persisted` (Task 5).
- Produces:
  - `pub struct PersistJob { pub bytes: Vec<u8>, pub by_user_id: Option<Uuid>, pub source: UpdateSource, pub meta: Option<serde_json::Value>, pub persisted: Option<oneshot::Sender<Result<i64, String>>> }` (the reply still carries the seq). A job with `persisted: Some(_)` is a server-side job. The writer records, per server-side job, `deadline: Option<Instant>`, `None` on arrival.
  - `pub struct Applied { pub seq: i64, pub rev: i64 }`
  - `pub enum WriterEvent { Applied(Applied), Fatal(String) }` (tuple variants; match as `WriterEvent::Applied(Applied { seq, rev })`).
  - `#[derive(Clone, Copy, Debug)] pub struct WriterLimits { pub server_job_deadline: Duration, pub attempt_timeout: Duration }` with `Default` = 5 s, 10 s.
  - `#[derive(Clone)] pub struct WriterDone(tokio::sync::watch::Receiver<bool>)` with `pub fn is_done(&self) -> bool` and `pub async fn wait(&mut self)` (returns once the writer task has ended, also when the sender was dropped by a panic), and `#[cfg(test)] pub(crate) fn pair() -> (tokio::sync::watch::Sender<bool>, WriterDone)` (a `WriterDone` the test resolves by sending `true` or dropping the sender; Task 7's `Room::spawn_detached` uses it).
  - `pub fn spawn(doc_id: Uuid, content_epoch: i32, store: Arc<dyn UpdatesStore>, bus: Arc<dyn Bus>, rx: mpsc::Receiver<PersistJob>, events: mpsc::UnboundedSender<WriterEvent>, limits: WriterLimits) -> WriterDone`

Behaviour: jobs are buffered in arrival order; a flush sends at most `batch_cap` jobs from the front (Global Constraints: `BATCH_MAX`, halved after a `shrinks_batch()` error, doubled back after each success) (never reordered: a server job's update can build on the WS jobs ahead of it). The writer keeps receiving jobs into the buffer while an attempt is in flight and while it waits to retry, so the room's `persist_tx` never fills behind a slow append. Two selects, never one over all three (review round 5): while an attempt is in flight, `tokio::select!` covers only the attempt (under `attempt_timeout`) and `rx.recv()`, and the only Evict cause it can meet is an overflow from a received job; a server-job deadline that passes meanwhile is not acted on until the attempt ends. While it waits to retry, `tokio::select!` covers the retry sleep, `rx.recv()` and the earliest armed server-job deadline. Each attempt is wrapped in `tokio::time::timeout(limits.attempt_timeout)`; expiry counts as a transient error (against Postgres the store's own `APPEND_STATEMENT_TIMEOUT` normally ends a stuck attempt first, and its drop guard cancels an abandoned one; Task 5). On success: one `WriterEvent::Applied` per job in order, then that job's `persisted` reply, then one `tokio::time::timeout(PUBLISH_TIMEOUT, bus.publish(doc_id, highest_rev))` whose error or expiry is only `tracing::debug!`-logged (best-effort; the room's 5 s catch-up tick covers it). On a transient error: keep the jobs at the front, arm the deadline of every buffered server-side job whose deadline is still `None` (`now + server_job_deadline`; jobs that arrive later stay unarmed until the next failure), increment `knot_writer_flush_retries_total`, wait `RETRY_MIN` doubling to `RETRY_MAX` (reset after a success). An in-flight attempt is never cut short by a deadline, only by `attempt_timeout`; deadlines are checked when an attempt ends (a held server job whose deadline passed during a failed attempt gets `Err`, not a retry; `a_server_job_held_by_the_attempt_that_fails_past_its_deadline_is_not_retried`), before each attempt, and when the earliest one fires during a retry wait.

Fatal (Global Constraints, two kinds). **Permanent** on a permanent store error: `WriterEvent::Fatal`, `Err` to every buffered job, end. **Evict** when the buffer exceeds `BUFFER_MAX_JOBS` or `BUFFER_MAX_BYTES`, or a server-side job's armed deadline has passed: `WriterEvent::Fatal(reason)` (reason contains "buffer" or "deadline"); every buffered server-side job that no in-flight attempt holds gets `Err` at once and is removed, whichever job caused the Evict (the job that overflows the buffer may itself be server-side, and the room is blocked awaiting its reply); a server-side job held by an in-flight attempt gets `Ok` if that attempt succeeds and otherwise `Err` and removal, never a retry (Global Constraints; `debug_assert!` that nothing is behind a server-side job; release builds log an error naming how many jobs were dropped); `rx.close()`, receive what is still queued (a server-side job among it gets `Err` at once and is dropped, Global Constraints), then flush the rest in order with retries and no deadlines until the buffer is empty (a permanent error during this drain turns it into Permanent for what is left), and end. When `rx` closes because the room ended (normally or after a Fatal it caused), flush what is buffered the same way (retries, no deadlines) before ending. The `WriterDone` resolves when the task ends. Record `knot_writer_flush_seconds` per attempt.

- [ ] **Step 1: Write the failing unit tests** (`#[tokio::test(start_paused = true)]`, a scripted store double whose `append` returns a queued sequence of results, each optionally after a virtual delay, and records every call; a bus double that records publishes and can hang; database errors built with `knot_storage::testing::db_error`; metric assertions through `test_metrics::CountingRecorder`)

```rust
async fn a_failed_batch_is_retried_in_order_with_later_jobs_behind_it() {
    // script: transient, transient, Ok, Ok. Send jobs 1..=3, wait one interval, send 4..=5.
    // recorded successful appends contain jobs 1..=5 exactly once, in order;
    // events: Applied revs 1..=5 in order; replies Ok(seq) for each; retry waits were 250 ms then 500 ms;
    // CountingRecorder: knot_writer_flush_retries_total == 2.
}
async fn a_permanent_error_is_fatal_and_fails_every_buffered_job() {
    // script: Err(EpochChanged); jobs with reply channels → every reply Err; one WriterEvent::Fatal;
    // writer task ends (WriterDone resolves); no further append call.
}
async fn too_many_buffered_jobs_evicts_and_still_writes_them() {
    // store transient until told otherwise; send BUFFER_MAX_JOBS + 1 tiny WS jobs → one Fatal containing "buffer";
    // further sends on the channel fail (rx closed). Make the store succeed → all BUFFER_MAX_JOBS + 1 jobs are
    // appended once, in order, in batches of ≤ BATCH_MAX; then WriterDone resolves.
}
async fn too_many_buffered_bytes_evicts() { /* 33 WS jobs of 1 MiB → Fatal containing "buffer" */ }
async fn a_server_job_that_overflows_the_buffer_is_answered_at_once() {
    // Review round 3. Store failing transiently until a flag flips. Buffer BUFFER_MAX_JOBS WS jobs (the limit, not
    // past it: no Fatal yet), wait until an attempt has failed and no attempt is in flight (the double reports it),
    // then send one job with a reply (BUFFER_MAX_JOBS + 1: it overflows).
    // → exactly one Fatal containing "buffer"; the reply is Err within 10 ms of virtual time (no retry, no deadline
    // wait); rx is closed. Flip the flag → the WS jobs are appended once, in order; the server job never; WriterDone
    // resolves.
}
async fn a_server_job_queued_behind_the_overflowing_job_is_answered_at_once() {
    // Review round 4, important. rx capacity 4; store failing transiently until a flag flips. Buffer BUFFER_MAX_JOBS
    // WS jobs and wait until an attempt has failed and no attempt is in flight. Then, without yielding, try_send one
    // more WS job and then a server job with a reply (both queued in rx before the writer receives either: the room
    // sent its WS job and then handled a server event).
    // → exactly one Fatal containing "buffer"; the server reply is Err within 10 ms of virtual time; flip the flag →
    // all BUFFER_MAX_JOBS + 1 WS jobs are appended once, in order; the server job never; WriterDone resolves.
}
async fn a_batch_that_always_times_out_is_split_until_it_fits() {
    // Review round 4, important. Scripted store: any append of more than 30 rows fails with
    // UpdatesStoreError::Sqlx(knot_storage::testing::db_error("57014")); smaller ones succeed. 200 WS jobs buffered
    // during a first transient failure (PoolTimedOut). → attempts of 200, 100 and 50 rows fail, a 25-row attempt
    // succeeds, and from then on 50-row attempts (the cap doubled back) fail and 25-row attempts succeed; every job is
    // appended exactly once, in order; no Fatal; WriterDone resolves after the channel is dropped.
}
async fn a_deterministic_error_is_permanent() {
    // Review round 4. append → Sqlx(db_error("22P02")) → one Fatal, every buffered reply Err, no retry (one append call).
}
async fn a_server_job_held_by_the_attempt_that_fails_past_its_deadline_is_not_retried() {
    // Review round 3. limits { server_job_deadline: 1 s, attempt_timeout: 10 s }. One WS job, then a server job;
    // the first attempt fails transiently (arms the deadline); the second attempt holds both and blocks on a Notify;
    // advance 2 s → no Fatal yet (an in-flight attempt is never cut short); release it with a transient error →
    // one Fatal containing "deadline", the reply Err, and the next attempt holds only the WS job.
}
async fn a_backlog_flushes_in_batches_of_at_most_batch_max() {
    // transient once (PoolTimedOut, which does not shrink the batch), 450 jobs buffered meanwhile →
    // successful appends of 200, 200, 50 rows.
}
async fn closing_the_channel_still_flushes_the_buffer() {
    // drop sender after 3 jobs, store transient twice then Ok → 3 rows appended; WriterDone resolves only after that.
}
async fn a_server_job_past_its_deadline_evicts_and_the_ws_jobs_ahead_are_still_written() {
    // Review round 2, critical. limits default; store transient until a flag flips, then Ok.
    // Send 2 WS jobs, then one job with a reply. First attempt fails at once; advance 5 s →
    // one Fatal containing "deadline"; the server job's reply is Err; rx is closed.
    // Flip the flag → exactly one successful append holding the 2 WS jobs (in order), never the server job;
    // Applied events for those 2; WriterDone resolves.
}
async fn a_slow_successful_attempt_never_fails_a_server_job() {
    // Review round 2. script: Ok after 6 s, Ok after 6 s. Send one WS job; during the first attempt (wait until
    // the double reports the call started) send a server job. Advance 15 s → no Fatal; the server reply is Ok(seq);
    // both jobs appended once, in order.
}
async fn a_server_job_behind_a_succeeding_backlog_waits_without_a_deadline() {
    // Review round 2. Every append Ok after 3 s. 450 WS jobs, then one server job at the back.
    // → appends of 200, 200, 51 rows (the server job in the last), no Fatal, the server reply Ok.
}
async fn a_failed_attempt_without_the_server_job_still_arms_its_deadline() {
    // 450 WS jobs then a server job; store always transient → Fatal "deadline" about server_job_deadline
    // after the first failed attempt that ended after the server job arrived, although no attempt held it.
}
async fn ws_jobs_alone_never_hit_a_deadline() {
    // store transient for 60 s of virtual time, then Ok; 10 WS jobs (no reply) → no Fatal; all 10 appended in order.
}
async fn a_hanging_attempt_is_abandoned_and_retried() {
    // first append never returns (pending future); second Ok. Advance attempt_timeout (10 s) →
    // one retry counted; the jobs are appended once by the second call; Applied events in order.
}
async fn a_hanging_publish_does_not_stop_the_writer() {
    // bus.publish never returns. Send job 1 → appended; after PUBLISH_TIMEOUT send job 2 → appended too;
    // the room's channel (capacity 2) never blocked a send for more than PUBLISH_TIMEOUT + BATCH_INTERVAL.
}
async fn jobs_are_received_while_an_attempt_is_in_flight() {
    // rx capacity 2; append blocks on a Notify. Send job 1, advance BATCH_INTERVAL and wait until the double
    // reports the first append started (so job 1 is alone in flight); then send jobs 2..=10 (each send completes:
    // assert with a 1 s timeout per send); release → the first append held job 1, the second jobs 2..=10.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-crdt --lib writer`
Expected: FAIL to compile.

- [ ] **Step 3: Implement** per Interfaces. Rewrite the module doc: nothing the room applied is dropped; a failed batch is retried; a permanent error evicts the room and fails what is buffered; an overflow or a missed server-job deadline evicts the room but still writes the jobs ahead of the cause; clients resend after the 1012 reconnect.

- [ ] **Step 4: Adapt the room just enough** (the room wiring follows in Task 7): `persist_tx` jobs get `source: Edit, meta: None`; the room spawns the writer with `content_epoch` `0` (Task 7 passes the boot's `content_epoch`) and `WriterLimits::default()` and keeps the returned `WriterDone` in `RoomHandle` (unused until Task 8); `WriterEvent::Applied(Applied { seq, .. })` goes to today's seq handling. Today's room has no generic eviction (`evict_unpersisted` needs a suggestion id, `refuse_applied` a conn), so add `fn evict(&mut self, why: std::fmt::Arguments)` (warn log with the doc id, `knot_room_evicted_total`, `evicted = true`, `shutdown.cancel()`), have `evict_unpersisted` call it, and map `WriterEvent::Fatal(reason)` to `self.evict(format_args!("writer: {reason}"))`. Once the writer ends on a permanent error its Fatal evicts the room, so on_inbound's `let _ = persist_tx.send(..)` (room.rs:927) never keeps fanning out for long; Task 7 replaces it with the persist helper.

Room tests whose premise this changes, rewritten here (not in Task 7) so this commit's full-crate run is green: `persist_failure_evicts_room` (room.rs:2447) seeds through an `ApplyUpdate` whose failing persist now makes the writer send a Fatal that evicts the room before the suggestion under test (`assert!(!h.shutdown.is_cancelled())` at :2462 fails, or the later `.unwrap()` panics, depending on timing). Give `FailingUpdates` reads that return the seed paragraph (`seed_paragraph(SEED_TEXT)`) as one `DocUpdate { seq: 1, rev: Some(1), .. }`, from `since(_, 0)` (today's boot) and from `rows_after` as every Task 7 double does (`All` and `Rev(0)` return the seed, `Rev(r)` filters `rev > r`, so the room's 5 s catch-up and `ApplySuggestion`'s pre-apply catch-up after boot read nothing), drop the `ApplyUpdate` seeding and the `is_cancelled` assert, and keep the assertions about the suggestion (Persist error, eviction, no snapshot, no fan-out). Then `grep -n 'is_cancelled()\|FailingUpdates' crates/knot-crdt/src/room.rs` for any other test that expects a room to survive a failed persist (as of 3b3df76: :2289 is a refusal, :2563 a failed read; neither persists through a failing store) and rewrite it the same way.

- [ ] **Step 5: Run them to verify they pass**

Run: `cargo nextest run -p knot-crdt --lib writer && cargo nextest run -p knot-crdt && cargo metadata --locked --format-version 1 > /dev/null`
Expected: PASS: the writer tests, and every room and integration test of the crate against the adaptation above (a regression here must not surface in Task 7 mixed with intended changes); `cargo metadata --locked` succeeds, so the `Cargo.lock` change is in the working tree to commit.

- [ ] **Step 6: Commit**

```bash
git add crates/knot-crdt Cargo.lock
git commit -m "feat(history): writer retries failed batches in order and evicts on permanent errors"
```

---

### Task 7: Room — boot by rev, applied_through_rev, catch-up, snapshot rule, effective fan-out

**Files:**
- Create: `crates/knot-crdt/src/hydrate.rs`
- Modify: `crates/knot-crdt/src/room.rs` (struct :177-208, `spawn` :234-309, idle snapshot :326-340, applied arm :683-708, exit snapshot :725-739, `replay_since_watermark` :747-790, `on_inbound` :811-929, eviction helpers :958-991), `crates/knot-crdt/src/snapshot.rs:16-35`, `crates/knot-crdt/src/registry.rs:23-28, 153`, `crates/knot-crdt/src/lib.rs`
- Modify: `crates/knot-server/src/routes/api/markdown.rs:141-170` (`stored_snapshot` calls `hydrate::boot(.., BootMode::ReadOnly)`, returns `(doc, last_seq)`; `BootError::Store(e)` → `SnapshotError::Storage(e)`, `BootError::Apply(e)` → `SnapshotError::Apply(e)`; its doc comment's contract, "read errors fail the snapshot instead of leaving content out", stays true, apply errors included)
- Unchanged server tests that pin that contract and must still pass: `crates/knot-server/tests/markdown_compat.rs:1628` `a_page_that_does_not_load_fails_the_check` (a `0xff` row → `LOAD_FAILED`), `crates/knot-server/tests/refresh_sweep.rs:623` `sweep_skips_past_a_failing_doc` (no cache row for pages whose rows don't decode)
- Existing room tests: `persist_failure_evicts_room` was rewritten in Task 6 to seed through the boot (its `FailingUpdates` reads return the seed for any `rows_after` argument, so it keeps passing on the new boot). `applied_check_unanswered_when_catch_up_fails` (room.rs:2540) keeps its shape; its double fails `rows_after` (Task 5). `grep -n 'is_cancelled()' crates/knot-crdt/src/room.rs` for any other test asserting a room survives a failed persist.
- Test: unit tests in `crates/knot-crdt/src/room.rs`; new `crates/knot-crdt/tests/ordered_log.rs`; adjust byte-equality assertions only where effective re-encoding changes bytes, comparing with a `same_update(a, b)` helper (equal `Update::insertions(true)` and `Update::delete_set()`; defined in `room.rs`'s test module and copied into `tests/nesting.rs` and `tests/ordered_log.rs`). Known sites: `crates/knot-crdt/tests/nesting.rs`; in `room.rs`, `noop_inbound_update_is_neither_fanned_out_nor_persisted` (:1673; `*first == wrap_sync_update(&real)` and `persisted == vec![real, sentinel]`, where `real` carries a deletion commit/GC may re-encode), `inbound_update_with_missing_dependency_is_still_persisted` (:1783; `vec![dependent]`, which stays raw under `raw_fallback` but compare with `same_update` anyway). Waits and matches count too, not only `assert_eq!`: the same noop test waits with `while !persisted.lock().unwrap().contains(&sentinel)` (:1763), and `tests/nesting.rs` has `store.holds(&after)` (:1063, used at :1215, :1513) and `fanned_out(&mut peer_rx, &after)` (:1136, frame equality at :1144); a changed byte string there never matches and fails on the 10 s deadline instead of at a compared assertion. Move every byte-equality wait or match on persisted or fanned-out bytes of a changed inbound update to `same_update` (a `contains` becomes `iter().any(|p| same_update(p, &x))`; a frame compare unwraps the sync frame first); `grep -n 'contains(&\|holds(\|fanned_out(\|== wrap_sync_update\|vec!\[' crates/knot-crdt/src/room.rs crates/knot-crdt/tests/nesting.rs` to find them. `revoke_signals_each_conn_and_ignores_its_updates` (:1934; `vec![sentinel]` for an `ApplyUpdate`) changes in Task 9, where `ApplyUpdate` persists its effective update.
- Test doubles at boot: a double whose `latest` returns `None` (`NoopSnapshots`, `CountingSnapshots`, room.rs:1109/2412) makes boot call `rows_after(RowsAfter::All)`, not `Rev(0)`. Every scripted double in Tasks 7-9 returns its seed for both `All` and `Rev(0)` (in practice: ignores the argument at boot and filters `rev > r` for `Rev(r)`); where a test below says "rows_after(Rev(0)) at boot" it means that read.

**Interfaces:**
- Consumes: Task 1 `ApplyMeasured::{persist, raw_fallback}`; Task 5 store API; Task 6 writer.
- Produces:
  - `pub enum BootMode { Room, ReadOnly }`; `pub struct Boot { pub doc: DocHandle, pub content_epoch: i32, pub through_rev: i64, pub through_seq: i64, pub last_seq: i64, pub unlogged: bool }`; `#[derive(Debug, thiserror::Error)] pub enum BootError { Store(String), Apply(EngineError) }` (`Apply` when the snapshot's own bytes fail to apply, in both modes, and in `ReadOnly` mode also on the first row that fails to apply; in `Room` mode such a row is passed, below); `pub async fn boot(engine: &dyn Engine, updates: &dyn UpdatesStore, snapshots: &dyn SnapshotStore, doc_id: Uuid, mode: BootMode) -> Result<Boot, BootError>` (re-exported).
  - `pub async fn write_snapshot(doc_id: Uuid, stamp: SnapshotStamp, engine: &dyn Engine, doc: &DocHandle, store: &dyn SnapshotStore) -> Result<(), SnapshotWriteError>` with `SnapshotWriteError::{EpochChanged, DocGone, Other(String)}`.
  - `Room::spawn(..) -> Result<RoomHandle, BootError>` (signature otherwise unchanged; uses `WriterLimits::default()`), and `Room::spawn_with_limits(.., limits: WriterLimits)` for tests, plus `#[cfg(test)] Room::spawn_detached(..) -> Result<(RoomHandle, mpsc::Receiver<PersistJob>, mpsc::UnboundedSender<WriterEvent>, watch::Sender<bool>), BootError>`, which spawns no writer and hands the test the persist receiver, the room's `WriterEvent` sender and the sender of the `RoomHandle`'s `WriterDone` (built with Task 6's `WriterDone::pair()`; the `WriterDone` stays pending until the test sends `true` or drops that sender, so a registry test with a detached room controls when its "writer" has drained) (`a_closed_persist_channel_evicts_without_fan_out` drops the receiver; `a_writer_fatal_is_seen_before_queued_inbound_events` sends the Fatal); `RoomHandle` keeps the `WriterDone` (Task 6), used by Task 8; `AcquireError::Hydrate(#[from] BootError)`.

`boot`: in `Room` mode first `ensure_legacy_numbered`; then `head`, then `latest` snapshot; `from` = `Rev(snapshot_rev)` for a stamped snapshot, else `All` (a legacy snapshot, or none: every row is re-applied, Rulings). Apply the snapshot, then the rows. In `Room` mode every row read has a rev (numbering ran first); apply only the run of rows contiguous from the base (stamped: `snapshot_rev`; otherwise 0) and set `through_rev` to its end; in `ReadOnly` mode apply every row read and leave `through_rev = 0`. A row whose apply returns `Err`: in `Room` mode it is passed (the apply-error Ruling): `tracing::warn!(doc_id, rev, seq, error)`, `knot_room_row_apply_errors_total{phase="boot"}`, `unlogged = true`, and the contiguous run continues past it; in `ReadOnly` mode boot returns `BootError::Apply` at once (a read-only load never yields a page with part of it missing). `through_seq` = seq of the row at `through_rev`, else the snapshot's seq, else 0; `last_seq` = the max of the snapshot's seq and the seqs of the rows applied (in `Room` mode the rows read past a rev gap are not applied and do not count, so the Markdown cache never files a render under a seq that claims them; in `ReadOnly` mode every row read is applied). A `LegacyCountMismatch` or any other store error is `BootError::Store`. `boot` never writes in `ReadOnly` mode (`ensure_legacy_numbered` is the only write it makes).

Room state: `content_epoch`, `applied_through_rev`, `through_seq`, `last_applied_seq` (kept for `ExportState`), `in_flight: usize`, `max_own_rev: i64`, `unlogged: bool` (starts as `Boot::unlogged`). Rules:
- every `PersistJob` the room sends goes through one helper that sends it first and increments `in_flight`; a send error (the writer closed its channel after a Fatal) is handled as `WriterEvent::Fatal` (log, `evicted = true`, `shutdown.cancel()`), and the caller then neither fans the update out nor replies `Ok`;
- the `WriterEvent` arm (an `UnboundedReceiver<WriterEvent>`) sits before `self.rx.recv()` in the actor's `biased` select (today `applied_rx` is after it, room.rs:341 vs :683), so a Fatal is seen before further inbound events;
- `WriterEvent::Applied(Applied { seq, rev })`: `in_flight -= 1`; `max_own_rev = max(..)`; `last_applied_seq = max(..)`; if `rev == applied_through_rev + 1` advance (and `through_seq = seq`); if `rev > applied_through_rev + 1` run catch-up now; keep the every-N counter and `dirty_tx` as today;
- `WriterEvent::Fatal(reason)`: log, `knot_room_evicted_total`, `evicted = true`, `shutdown.cancel()`;
- catch-up (bus message, 5 s tick, gap) returns `bool` "caught up": `rows_after(Rev(applied_through_rev))`, skip any row with `rev <= applied_through_rev` (not a gap), apply each contiguous row with `apply_update_changed`, fan out the row's bytes only when it changed the doc, advance `applied_through_rev`/`through_seq`/`last_applied_seq`; a row whose apply returns `Err` is passed (warn, `knot_room_row_apply_errors_total{phase="catch_up"}`, `unlogged = true`, not fanned out, the watermark advances past it, later rows still apply; the apply-error Ruling); on a gap, `knot_room_rev_gap_total`, stop, and return `false`; a read error returns `false`; otherwise `true`. `SuggestionApplied` answers only on `true` (as today with `replay_since_watermark`);
- snapshots (every-N, idle, exit) only under the Global Constraints rule; a snapshot `EpochChanged` evicts the room (no further snapshot; the exit snapshot is skipped), and a snapshot `DocGone` evicts it quietly (`tracing::debug!`, `evicted = true`, `shutdown.cancel()`, no `knot_room_evicted_total`: the page is gone, nothing broke; the exit snapshot is skipped too);
- every server-side persist the room awaits is bounded by the writer (`WriterLimits`); the room itself adds no timeout;
- `on_inbound`: persist `measured.persist` through the helper and, only if the helper's send succeeded, fan out those same bytes (`wrap_sync_update(&measured.persist)`, not the raw inbound bytes; skip both when empty), with `knot_room_updates_persisted_raw_total` when `raw_fallback`; the `rejected` arm additionally sets `unlogged = true`;
- the writer is spawned with the boot's `content_epoch`.

- [ ] **Step 1: Write the failing tests**

Unit tests in `room.rs` (doubles; a scripted store whose `rows_after` returns given rows and whose `append` assigns given revs; every metric assertion below, the `knot_room_row_apply_errors_total` "+1" checks included, reads `test_metrics::CountingRecorder` from Task 6):

```rust
async fn no_snapshot_while_rows_are_in_flight() {
    // append blocks on a Notify; send one inbound update; trigger idle snapshot (policy idle 0) → no insert_stamped;
    // release → after Applied, the next idle tick writes SnapshotStamp { snapshot_rev: Some(1), epoch: Some(0), snapshot_seq: <seq> }.
}
async fn an_own_rev_past_a_gap_triggers_catch_up() {
    // peer row rev 1 visible in rows_after; own append returns rev 2 → room reads rows_after(Rev(0)) at once,
    // applies rev 1 (fanned out to the conn) and ends with applied_through_rev 2; a snapshot is then allowed.
}
async fn catch_up_applies_a_row_whose_seq_is_below_the_watermark() {
    // rows rev 1 seq 20, rev 2 seq 15 (seq order ≠ rev order): both applied. (RED on main: room.rs:755 skipped seq 15.)
}
async fn catch_up_stops_at_a_rev_gap() {
    // rows rev 1, rev 3 → only rev 1 applied; through = 1; CountingRecorder: knot_room_rev_gap_total >= 1.
    // A double that also returns rev 1 again for Rev(1) (a row at the watermark) → skipped: no second fan-out,
    // no extra knot_room_rev_gap_total.
}
async fn snapshot_doc_gone_evicts_without_exit_snapshot() {
    // Review round 4. appends succeed; insert_stamped returns SnapshotStoreError::DocGone on the first idle snapshot →
    // shutdown cancelled; the actor ends (h.tx.closed()) with exactly one insert_stamped call (no exit snapshot);
    // CountingRecorder: knot_room_evicted_total stays 0 (quiet eviction).
}
async fn a_writer_fatal_is_seen_before_queued_inbound_events() {
    // Review round 4: pins the biased select order (WriterEvent arm before rx.recv()). Room::spawn_detached; one conn
    // joined. Without yielding: try_send 20 Event::ExportState (each with its own oneshot) into h.tx, then send
    // WriterEvent::Fatal("test") on the events sender. Then await h.tx.closed().
    // → every ExportState receiver gets Err (dropped, never answered): the room ended before it read rx.
}
async fn a_raw_fallback_persist_is_counted() {
    // An inbound update whose dependency is missing (as inbound_update_with_missing_dependency_is_still_persisted,
    // :1783) → CountingRecorder: knot_room_updates_persisted_raw_total == 1; an ordinary edit after it adds nothing.
}
async fn no_snapshot_while_an_own_rev_is_past_a_gap() {
    // policy every_n 1, idle 0. Own append returns rev 3; rows_after returns only rev 1 (rev 2 unreadable).
    // After catch-up: applied_through_rev 1, max_own_rev 3. Idle tick, every-N and shutdown write no
    // insert_stamped (shutdown in a second room built the same way). Then make rev 2 (and 3) readable;
    // the next tick catches up and the next snapshot is stamped snapshot_rev Some(3).
}
async fn suggestion_applied_is_unanswered_at_a_rev_gap() {
    // rows_after returns rev 1 and rev 3 only; SuggestionApplied → the reply is dropped (Err on the receiver);
    // the room is not evicted. Once rev 2 is readable, SuggestionApplied answers.
}
async fn writer_fatal_evicts_without_snapshot() { /* append → EpochChanged: shutdown cancelled, no insert_stamped */ }
async fn snapshot_epoch_changed_evicts_the_room() {
    // appends succeed; insert_stamped returns SnapshotStoreError::EpochChanged on the first idle snapshot →
    // shutdown cancelled; the actor ends (h.tx.closed()) with exactly one insert_stamped call (no exit snapshot).
}
async fn a_rejected_update_stops_snapshots() {
    // reuse tests/nesting.rs `parked_block_and_the_edit_it_fails_on` shape (copy the fixture builder), with a
    // recording double whose rows_after returns what append stored:
    // after the rejected apply, idle and exit snapshots are never written.
    // Pinned cost (Rulings): a second conn then edits inside the integrated, unlogged paragraph; its row is persisted
    // and fanned out. Shut the room down and acquire it again: the booted doc lacks that edit (it stays pending).
}
async fn catch_up_passes_a_row_that_fails_to_apply_and_marks_the_room_unlogged() {
    // Review round 2. The double's rows_after returns rev 1 (a paragraph, ok), rev 2 (the crafted parked block),
    // rev 3 (the innocent edit it fails on), rev 4 (another paragraph); the room booted at rev 0.
    // A bus message → catch-up: rows 1, 2 and 4 are fanned out, row 3 is not; applied_through_rev 4;
    // knot_room_row_apply_errors_total{phase="catch_up"} +1; SuggestionApplied answers (caught up);
    // idle, every-N and exit snapshots are never written (unlogged); the room is not evicted.
}
async fn boot_passes_a_row_that_fails_to_apply() {
    // Same rows returned by rows_after(Rev(0)) at boot (today's boot fails on them, room.rs:255):
    // Room::spawn succeeds, the doc holds rows 1, 2's parked part and 4, applied_through_rev 4,
    // knot_room_row_apply_errors_total{phase="boot"} +1, and no snapshot is ever written.
    // hydrate::boot(.., BootMode::ReadOnly) over the same double → Err(BootError::Apply(_)) (review round 4).
}
async fn a_closed_persist_channel_evicts_without_fan_out() {
    // Review round 2. The ordering "writer closed its channel, room has not yet seen the Fatal" can't be staged
    // through a real writer, so use Room::spawn_detached and drop the persist receiver at once (no writer).
    // Conns A and B joined; A sends an edit → B receives no frame for it, the room's shutdown is cancelled and the
    // actor ends (h.tx.closed()); no Ok reply reaches a server-side event sent after it (ApplyUpdate → Err or dropped).
}
async fn a_lost_ack_attempt_is_caught_up_before_a_snapshot() {
    // Review round 3. Real writer, scripted store: the first append stores the job's row as rev 1 (visible to
    // rows_after) and returns Err(Sqlx(Io)) (a lost COMMIT acknowledgement); the retry stores the same bytes again as
    // rev 2 and returns Ok rev 2. Policy every_n 1, idle 0.
    // → Applied rev 2 with applied_through_rev 0 → catch-up reads rev 1 (changes nothing: not fanned out) and rev 2;
    // applied_through_rev 2; no insert_stamped before that; the next snapshot is stamped snapshot_rev Some(2).
}
async fn boot_last_seq_ignores_rows_past_a_gap() {
    // rows_after(Rev(0)) returns rev 1 seq 10, rev 3 seq 30 → applied_through_rev 1; ExportState's seq is 10, not 30.
}
```

DB tests in `crates/knot-crdt/tests/ordered_log.rs` (`knot_test_support::fresh_db`; a `Flaky` store wrapping `PgUpdatesStore` with two modes: `FailBefore` fails the first K `append` calls with `Sqlx(PoolTimedOut)` without touching the database, and `LostAck` runs the real `append` and then returns `Err(Sqlx(sqlx::Error::Io(..)))` for the first K successful calls, so the rows committed but the writer retries them; two connections joined to one room; edits generated with yrs client docs and the seeded xorshift; a `replay(pool, doc, upto_rev)` helper applying `rows_after(Rev(0))` up to a rev into a fresh doc):

```rust
async fn flush_failures_lose_nothing() {
    // Run once per Flaky mode (FailBefore, LostAck). K = 6 with start_paused off; 40 edits from two conns; wait until
    // count(*) stops growing and the room answers ExportState; replay(all) has the same state vector, delete set and
    // text as the room; revs are exactly 1..=count(*).
    // LostAck only: duplicate rows exist (same update_bytes at two revs), and applying each duplicate to
    // replay(rev - 1) with apply_update_measuring reports changed == false (it integrates nothing).
}
async fn every_snapshot_equals_the_log_replayed_to_its_rev() {
    // Run once per Flaky mode (K = 4). policy every_n 5; 60 edits incl. a reconnect resend and one held-back update
    // (conn A's update u2 is delivered before its predecessor u1, at least 5 edits later, so a stamped snapshot is
    // written while the room holds u2 as pending content; persisted raw: raw_fallback); for every doc_snapshots
    // row with snapshot_rev R: replay(R) == the snapshot's state (state vector, delete set, text, pending content
    // encoded as in Task 1). Then shut the room down and acquire it again: the rebooted doc equals replay(all)
    // and holds no pending content (u1's row brought the dependency).
}
async fn a_nonzero_content_epoch_reaches_rows_and_snapshots() {
    // Review round 3. Before the first acquire: INSERT INTO doc_history_heads (doc_id, content_epoch) VALUES (D, 2)
    // (as after an erase). hydrate::boot(.., BootMode::Room) returns content_epoch 2. Acquire the room (policy every_n 1);
    // one conn edits 3 times; wait for the rows and a stamped snapshot.
    // Every new doc_updates row has epoch 2; the stamped snapshot has epoch Some(2); the room was never evicted
    // (shutdown not cancelled) and every edit was persisted. (A room passing 0 to writer::spawn, or stamping
    // epoch 0, would be refused with KH002 and evict here.)
}
async fn legacy_page_boots_complete_and_continues_numbering() {
    // Review Focus 1: migrate up to 20261107120000; 5 rows (yrs updates) and a legacy snapshot at the 3rd row's seq;
    // full migrate; acquire the room: its text equals all 5 rows applied; legacy rows now have revs 1..=5 by seq;
    // one more edit gets rev 6; a snapshot written now has snapshot_rev 6.
}
async fn legacy_snapshot_missing_a_lower_seq_row_boots_with_it() {
    // Review Focus 1, the room.rs:755 loss: migrate up to 20261107120000; rows seq a < b < c (three paragraphs
    // from three clients); a legacy snapshot at seq c built from rows a and c only (b skipped, as a 0.9.x room
    // whose own row c confirmed before b committed would have); full migrate; acquire the room:
    // its text holds all three paragraphs; the first stamped snapshot (after one edit) equals replay(its rev).
}
async fn reconnect_resend_persists_only_new_content() {
    // Review Focus 4: conn A edits; conn B is joined; A "reconnects" and resends its full state plus one new
    // paragraph; the new row's update has insertions only for the new paragraph and an empty delete set beyond it,
    // and the update frame B receives for it carries exactly the persisted row's update_bytes (the effective update
    // is what is fanned out, not the raw resend).
    // Under the spike fallback (Rulings), replace the trimming assertion with: replay(all) equals the room's doc;
    // the frame-equals-row assertion stays.
}
async fn epoch_bump_evicts_the_room_on_its_next_flush() {
    // One inbound edit first (so the heads row exists; on a page with no heads row the UPDATE matches nothing and
    // the next flush succeeds for the wrong reason); wait for its row.
    // UPDATE doc_history_heads SET content_epoch = 1 (assert 1 row affected); one more inbound edit →
    // the room's shutdown is cancelled; no further row inserted.
}
async fn read_only_boot_never_numbers_legacy_rows() {
    // Review round 2. Legacy page as in legacy_page_boots_complete_and_continues_numbering (migrate up to
    // 20261107120000, 5 rows, a legacy snapshot, full migrate). boot(.., BootMode::ReadOnly) returns a doc holding
    // all 5 rows; count(*) FILTER (WHERE rev IS NULL) is still 5 and no doc_updates row changed (compare
    // xmin of every row before and after).
}
async fn legacy_snapshot_content_without_rows_is_the_base_of_stamped_snapshots() {
    // Review Focus 6. Migrate up to 20261107120000; a legacy snapshot holding paragraph P0, which no doc_updates row
    // carries (as the pre-M1.1 GC left it), plus one surviving row P1 with seq > snapshot_seq; full migrate;
    // heads(D) exists (legacy_rows 1). Acquire the room, edit (P2), wait for the stamped snapshot at rev R.
    // The snapshot holds P0, P1 and P2 and equals the legacy snapshot ∪ replay(R) (state vector, delete set, text);
    // replay(R) alone lacks P0.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-crdt`
Expected: FAIL (new API missing; the seq-skip test fails).

- [ ] **Step 3: Implement** `hydrate.rs`, the room rules, `write_snapshot` stamping, `registry.rs` error type, and `stored_snapshot` via `BootMode::ReadOnly`. Update the module docs of `room.rs` (watermark is a rev; snapshots only when the doc holds nothing beyond its pre-upgrade base and the log; rows that fail to apply are passed and stop snapshots).

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-crdt && cargo nextest run -p knot-server`
Expected: PASS, every knot-server test included (catch-up by rev, snapshot stamping, boot and effective fan-out change what `suggestions_integration`, `history_integration`, `contributors_integration` and `tasks_lockstep` rely on; `markdown_compat` and `refresh_sweep` pin the read-only load's failure on an undecodable row, unchanged; `Event` signatures don't change until Task 9, so it compiles, and a regression must not surface in Task 9 mixed with its intended changes).

- [ ] **Step 5: Commit**

```bash
git add crates/knot-crdt crates/knot-server/src/routes/api/markdown.rs
git commit -m "feat(history): rooms track a rev watermark and snapshot only what the log holds"
```

---

### Task 8: Drain writers after an eviction and on shutdown

**Files:**
- Modify: `crates/knot-crdt/src/registry.rs` (`AcquireError` :23-28 gains `#[error("the page's previous writer is still saving")] Draining`; `Rooms` :78-95 gains `draining: DashMap<Uuid, WriterDone>`; `acquire` :134-171; `drop_if_dead` :195; `evict` :274-282; new `drain_all`; constants `DRAIN_JOIN_WAIT`, `SHUTDOWN_DRAIN`; `Rooms` gains `writer_limits: WriterLimits` (default) and a builder `with_writer_limits(WriterLimits)` like `with_dirty_tx` (:113-118), and `acquire` spawns rooms with `Room::spawn_with_limits(.., self.writer_limits)`)
- Modify: `deploy/helm/knot/values.yaml:170-175` (`terminationGracePeriodSeconds: 50`; the comment gains the budget: preStop + up to 30 s of HTTP drain + up to 10 s of page-writer drain + 5 s of slack)
- Modify: `crates/knot-server/src/main.rs:307-330` (clone `state.rooms_v2` before `router_with_state(state)`; after `axum::serve(..).with_graceful_shutdown(..)` returns, drain)
- Test: unit tests in `crates/knot-crdt/src/registry.rs` (`mod tests` :285)

**Interfaces:**
- Consumes: `WriterDone` (Task 6), kept in `RoomHandle` (Task 7).
- Produces: `AcquireError::Draining`; `pub fn with_writer_limits(self, WriterLimits) -> Self`; `pub async fn drain_all(&self, within: Duration) -> usize` (ends every room, then waits for every writer, live and draining, up to `within`; returns how many were still running, and logs each one's doc id at warn level).

Behaviour: when `evict` or `drop_if_dead` removes a handle, its `WriterDone` moves into `draining` unless it is already done; both first prune entries that are done. `acquire`, after its `live` fast path and before taking the per-doc `inflight` lock, only reads: it clones the `WriterDone` of a dead handle still in `map` (shutdown cancelled), else of `draining[doc_id]` (if any), and waits on the clone up to `DRAIN_JOIN_WAIT`, so concurrent reconnects wait side by side, not one after another under the mutex; still running → `AcquireError::Draining` without taking the lock. It does not remove the dead handle, move it to `draining` or unsubscribe outside the lock (review round 4: an unlocked `bus.unsubscribe` could send its `UNLISTEN` after a concurrent boot's `LISTEN` on the same connection, and that room would get no bus notifications for its life). Under the lock: `live` again, then `drop_if_dead` as today (removes a dead handle, moves its `WriterDone` to `draining` unless done, unsubscribes), and only then the `draining` check without waiting: an entry that is not done (a room that died while this `acquire` waited, or one evicted in between) → `AcquireError::Draining`; done → remove the entry and boot. Callers already handle the error (the WS shim, `knot-server/src/room.rs:38-43`, already logs and closes the socket, and the client reconnects; the HTTP callers already map an acquire error to a 5xx). `drain_all` evicts every room (as `restart_all` does, without bumping ACL epochs), then awaits every `WriterDone` concurrently under one `tokio::time::timeout(within)`. In `main`, after the HTTP drain: `if let Some(rooms) = rooms { let pending = rooms.drain_all(SHUTDOWN_DRAIN).await; tracing::info!(pending, "page writers drained"); }`, before the telemetry flush.

- [ ] **Step 1: Write the failing tests** (`#[tokio::test(start_paused = true)]`; a store double whose `append` fails transiently until a flag flips and records successful appends; the room tests' `NoopSnapshots`)

```rust
async fn a_new_room_waits_for_the_previous_writer_to_drain() {
    // Room for D; a conn sends an edit (buffered: appends fail). evict(D) (as a revoke or Fatal would).
    // acquire(D) spawned: still pending after 1 s. Flip the flag → the old writer appends the edit; acquire then
    // boots, and the new room's doc holds the edit (its rows_after returns what append stored).
}
async fn acquire_gives_up_on_a_writer_that_keeps_draining() {
    // As above without flipping the flag: acquire(D) returns AcquireError::Draining after DRAIN_JOIN_WAIT.
    // A later acquire after the flag flips succeeds.
}
async fn concurrent_acquires_wait_for_one_drain_side_by_side() {
    // Review round 3. As acquire_gives_up_on_a_writer_that_keeps_draining, but three acquire(D) start at once:
    // all three return AcquireError::Draining within DRAIN_JOIN_WAIT + 1 s of virtual time (not 3 × DRAIN_JOIN_WAIT).
}
async fn a_self_evicted_room_is_drained_before_a_new_one_boots() {
    // Rooms::new(..).with_writer_limits(WriterLimits { server_job_deadline: 300 ms, attempt_timeout: 300 ms });
    // the first room is booted through acquire(D), so the registry holds its handle. A conn sends a WS edit
    // (buffered: appends fail), then a server-side ApplyUpdate during the outage → the writer's Evict Fatal ends the
    // room; drop_if_dead moves its WriterDone into draining. acquire(D) → waits; flip the flag → the WS edit is appended once,
    // the server-side update never; the new room boots with the WS edit.
}
async fn a_room_that_dies_while_acquire_waits_for_the_lock_is_drained_first() {
    // Review round 4. Appends fail. The test takes D's inflight mutex itself (registry tests reach the private maps),
    // as a concurrent acquire that is booting D would. Start acquire(D): no room and no draining entry, so it passes
    // the unlocked read and waits for the mutex. The test then does what that concurrent acquire would: spawns a room
    // for D, inserts its handle into map, sends it a WS edit (buffered), cancels its shutdown (as a writer Fatal
    // would), and releases the mutex. → acquire returns AcquireError::Draining (drop_if_dead ran before the draining
    // check), never a new room booted without the edit. After the flag flips, acquire(D) boots with the edit.
}
async fn a_dead_room_unsubscribes_only_under_the_lock() {
    // Review round 4. A bus double recording subscribe/unsubscribe calls in order. A room for D evicts itself (its
    // WriterDone already done); three acquire(D) start at once → the recorded sequence is exactly one unsubscribe
    // followed by one subscribe (no unsubscribe after the subscribe), and all three get the same new room.
}
async fn drain_all_writes_buffered_edits_before_returning() {
    // Two rooms, each with a buffered edit (appends fail for 2 s of virtual time, then succeed).
    // drain_all(10 s) returns 0 and both edits were appended before it returned.
    // With appends failing for longer than `within`: drain_all returns 2 after `within`.
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-crdt --lib registry`
Expected: FAIL to compile (`drain_all`, `Draining` missing).

- [ ] **Step 3: Implement** per Interfaces, and the `main.rs` call. Update `Rooms`' doc comment: a page's writer outlives its room until its buffer is written, and a new room for the page waits for it.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-crdt && cargo nextest run -p knot-server --no-run && helm lint deploy/helm/knot`
Expected: PASS; the server compiles; the chart lints.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-crdt/src/registry.rs crates/knot-server/src/main.rs deploy/helm/knot/values.yaml
git commit -m "feat(history): drain page writers after an eviction and on shutdown"
```

---

### Task 9: Tag every server-side writer and persist before fan-out

**Files:**
- Modify: `crates/knot-crdt/src/room.rs` (`Event` :93-175: `ApplyUpdate` and `ReplaceWithMarkdown` gain `source: UpdateSource, meta: Option<serde_json::Value>`; `ApplySuggestion` gains `accepted_by: Uuid`; handlers :383-422, :429-511, :512-573, :574-651)
- Modify call sites: `crates/knot-server/src/routes/api/history.rs:177` (source `Restore`, meta `{"target_snapshot_seq": seq, "target_ended_at": snap.created_at}`), `crates/knot-server/src/routes/api/markdown.rs:544, 562` (`Import`), `crates/knot-server/src/routes/api/export_import.rs:740` (`Import`), `crates/knot-server/src/routes/api/docs.rs:1267` (`Template`, meta `{"template_doc_id", "template_title"}`; fetch the template's title with `docs.get(template_id)` even when the body names a title), `crates/knot-server/src/routes/api/suggestions.rs:528` (`accepted_by: ctx.user_id`), `crates/knot-server/src/routes/api/tasks.rs:177` (no new field; the room tags `Task`). Inbound WS rows are `Edit` (room `on_inbound`). Update test call sites (`grep -rn 'Event::ApplyUpdate\|Event::ReplaceWithMarkdown\|Event::ApplySuggestion' crates`).
- Test: room unit tests (`revoke_signals_each_conn_and_ignores_its_updates`, room.rs:1934, asserts `vec![sentinel]` for an `ApplyUpdate`, whose persisted bytes become its effective update here: compare with `same_update`, Task 7); `crates/knot-crdt/tests/nesting.rs` (one new test, beside `chain_update`); `crates/knot-server/tests/history_labels_integration.rs` (new)

**Interfaces:**
- Consumes: Task 6 `PersistJob { source, meta }`; Task 7 room persist helper and eviction.
- Produces: rows tagged as in Global Constraints (`meta` shapes); `ApplySuggestion` rows carry `meta = {"suggestion_id": <id>, "accepted_by": <owner id>}` with `by_user_id` = the suggester.

Every server-side handler: decode/validate first (for `ReplaceWithMarkdown`, decode the update *before* clearing the fragment), mutate (`ReplaceWithMarkdown`, `PatchTaskChecked`, `ApplySuggestion`: in one transaction capturing `observe_update_v1`; `ApplyUpdate`: `apply_update_measuring(.., MAX_NESTING)`, persisting `measured.persist`, the effective update or the raw fallback, per the ApplyUpdate Ruling; empty `persist` → no job, no fan-out, reply `Ok(last_applied_seq)`; `rejected` → `Err`, `unlogged`, evict; `measured.max_depth > MAX_NESTING` (checked before the `persist` arms) → `Err`, evict, nothing persisted or fanned out, as `refuse_applied` does for an inbound update (the ApplyUpdate Ruling)), send the job, await the reply, and only then fan out. The await is bounded by the writer (Task 6: no deadline until an attempt fails, then `server_job_deadline`; about 25 s in the worst case of an outage). An apply error after the mutation began, or a persist error (including a deadline), replies `Err` and evicts the room (`evicted = true`, `shutdown.cancel()`), generalizing `evict_unpersisted`.

- [ ] **Step 1: Write the failing tests**

Room unit tests. The page content each test needs is seeded through the boot (the double's `rows_after` at boot, `All` or `Rev(0)` alike, returns the task list or paragraph as rev 1), never through an `ApplyUpdate`, whose own persist would fail and evict the room first:

```rust
async fn server_edit_persist_failure_reaches_no_client() {
    // Review Focus 5. Store: append fails permanently (EpochChanged). One joined conn;
    // PatchTaskChecked on the booted task list → reply Err; the conn's channel received no update frame;
    // the room is evicted; no snapshot written. Same (fresh room each) for ReplaceWithMarkdown and ApplyUpdate.
}
async fn server_edit_during_an_outage_frees_the_room_and_keeps_the_ws_edit() {
    // Review Focus 3, review round 2 (critical). Room::spawn_with_limits(WriterLimits { server_job_deadline: 300 ms,
    // attempt_timeout: 300 ms }); store: append returns Sqlx(PoolTimedOut) until a flag flips, and records every
    // successful append. Conns A and B joined.
    // A sends one WS edit (buffered, retried); A's conn then leaves (closed the tab). PatchTaskChecked → reply Err
    // within 2 s; B received A's edit frame (WS edits fan out once enqueued) and no frame for the task;
    // the room is evicted within 2 s; no successful append yet.
    // Flip the flag → exactly one successful append, holding A's edit with by_user_id = A; the task's update is never
    // appended; the room's WriterDone resolves.
}
async fn apply_update_with_a_missing_dependency_is_still_persisted() {
    // Review round 3. An ApplyUpdate whose update depends on a block the room lacks (built as in
    // inbound_update_with_missing_dependency_is_still_persisted, :1783): reply Ok; the recorded append holds the raw
    // bytes (same_update with the sent update; raw_fallback), so no row is missing the parked content. Then the
    // missing dependency arrives as a second ApplyUpdate: both rows replayed in order equal the room's doc.
}
async fn apply_update_that_changes_nothing_persists_nothing() {
    // ApplyUpdate with an update the doc already holds → reply Ok; no append recorded; no frame on a joined conn.
}
async fn replace_with_undecodable_bytes_leaves_the_doc_untouched() {
    // ReplaceWithMarkdown with b"\xff" → Err; ExportState text unchanged; room not evicted.
}
```

In `crates/knot-crdt/tests/nesting.rs`:

```rust
async fn apply_update_too_deep_is_refused_and_evicts() {
    // Review round 4. Event::ApplyUpdate with chain_update(MAX_NESTING + 1) on a room with one joined conn →
    // reply Err; no append recorded; the conn received no update frame; the room's shutdown is cancelled; no
    // snapshot written; a new acquire boots without the chain.
}
```

`crates/knot-server/tests/history_labels_integration.rs` (in the style of `contributors_integration.rs`, real server + DB), one test per writer, each reading the newest `doc_updates` row of the page:

```rust
async fn ws_edit_is_labelled_edit()            // source 'edit', by_user = editor, epoch 0, meta NULL
async fn task_toggle_is_labelled_task()        // PATCH /tasks → 'task', by_user = toggler
async fn accepted_suggestion_credits_the_suggester() // 'suggestion', by_user = suggester, meta {suggestion_id, accepted_by: owner}
async fn markdown_import_is_labelled_import()  // POST /api/docs/{id}/markdown (append and replace) → 'import'
async fn workspace_import_is_labelled_import() // zip import → each page's row 'import'
async fn from_template_is_labelled_template()  // 'template', meta {template_doc_id, template_title}
async fn duplicate_is_labelled_copy()          // 'copy', meta {source_doc_id, source_title} for top and subpage
async fn snapshot_restore_is_labelled_restore() // POST /history/{seq}/restore → 'restore', meta {target_snapshot_seq, target_ended_at}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-crdt --lib room && cargo nextest run -p knot-crdt --test nesting apply_update_too_deep && cargo nextest run -p knot-server --test history_labels_integration`
Expected: FAIL.

- [ ] **Step 3: Implement** per Interfaces.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-crdt && cargo nextest run -p knot-server`
Expected: PASS (including `history_integration`, `suggestions_integration`, `templates_integration`, `duplicate_integration`, `tasks_lockstep`).

- [ ] **Step 5: Commit**

```bash
git add crates/knot-crdt crates/knot-server
git commit -m "feat(history): label every server-side edit and persist it before fan-out"
```

---

### Task 10: Two replicas and a 0.9.9 pod on one page

**Files:**
- Create: `crates/knot-crdt/tests/two_replicas.rs`

**Interfaces:**
- Consumes: `Rooms::new` (`registry.rs:97-118`), `PgUpdatesStore`, `PgSnapshotStore`, `MemBus`, Task 2's `OLD_INSERT_UPDATES_SQL` and `OLD_SNAPSHOT_UPSERT_SQL` (copy both consts; they live in another crate's integration test).

- [ ] **Step 1: Write the test**

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn old_and_new_replicas_share_one_gapless_log() {
    // Review Focus 2. One fresh DB, one MemBus shared by two `Rooms` (replicas A and B, each with its own pool),
    // policy every_n 7. Doc D. Two conns on A, one on B, editing concurrently (seeded xorshift, 200 edits),
    // plus a task inserting yrs updates through OLD_INSERT_UPDATES_SQL (a 0.9.9 pod) and publishing on the bus.
    // The 0.9.9 pod also writes snapshots like a 0.9.9 room: every 5th insert it runs OLD_SNAPSHOT_UPSERT_SQL
    // with snapshot_seq = SELECT max(seq) FROM doc_updates WHERE doc_id = D (so it collides with stamped rows)
    // and the state of a doc holding only the updates this task inserted (deliberately not replay(R)).
    // Then quiesce (count(*) stable for 1 s) and force each room's catch-up with Event::SuggestionApplied (a random
    // suggestion_id and apply_token): it runs the rev catch-up before it answers and answers only when caught up, so
    // its reply (false) means the room holds every row committed before its read. Only then ExportState. (A bus
    // publish is not enough: the actor's biased select handles `rx` before `bus_updates_rx`, room.rs:341 vs :709, and
    // MemBus drops a subscriber whose 256-slot channel was once full, bus_mem.rs:33, so after 200 fast edits only the
    // 5 s tick would catch up.) If a reply is dropped (not caught up: a gap), retry for up to 10 s before failing.
    // Each replica's PgUpdatesStore is wrapped in a Recording store that forwards every call and keeps every Err its
    // append returned (the writer retries and only logs them, so the rows alone cannot show a retried deadlock); the
    // old-pod task keeps its own errors.
    // Assert: revs are exactly 1..=count(*); neither Recording store nor the old-pod task saw any error (none is
    // 40P01, and none at all);
    // A, B and replay(all) have the same state vector, delete set and text;
    // every snapshot with snapshot_rev R equals replay(R) (the old pod's upserts never landed on a stamped row);
    // a third `Rooms` (replica C) acquiring D afterwards boots to replay(all).
}
```

- [ ] **Step 2: Run it**

Run: `cargo nextest run -p knot-crdt --test two_replicas`
Expected: PASS. If it fails, the bug is in Tasks 5-8: fix it there (with its own failing unit test), not here.

- [ ] **Step 3: Full suite and lint**

Run: `make lint && make test.rust`
Expected: PASS, no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/knot-crdt/tests/two_replicas.rs
git commit -m "test(history): two replicas and an old pod share one gapless log"
```

---

### Task 11: Docs, deviation notes and end-to-end check

**Files:**
- Modify: `CHANGELOG.md` (new `## [0.9.10] - <release date>` above `## [0.9.9]` at :11; compare links at :1121-1122)
- Modify: `deploy/helm/knot/README.md` (new `### Upgrading from 0.9.9` after `### Upgrading from 0.9.8` at :199-213; "Operational notes" bullet "Storage growth from page history" at :223)
- Modify: `docs/superpowers/specs/2026-11-04-page-history-architecture.md` (a "Deviations recorded in the M1.2 plan" paragraph after the M1.1 one at :7)
- Modify: `.github/workflows/helm-upgrade.yaml` (seed and verify steps, :255-330), `.github/scripts/knot-upgrade-lib.sh` (an `api_md` helper next to `api` that sends a `text/markdown` body with the CSRF header)
- Modify: `e2e/support/reset.ts:73` (database name from `KNOT_E2E_DB`, default `knot`; Step 5)

- [ ] **Step 1: CHANGELOG `## [0.9.10]`**, operator-facing, lines ≤ 76 characters, `(#<PR>)` once the PR exists, compare links `[Unreleased]: …/compare/v0.9.10...HEAD` and `[0.9.10]: …/compare/v0.9.9...v0.9.10`:
  - **Upgrading:** upgrade from 0.9.9 and don't skip 0.9.10 on the way to a later release. Don't upgrade while `pg_dump`, another long transaction or an anti-wraparound autovacuum runs: every one of the five migrations waits for or gives up on them, and when a later one gives up the earlier ones have committed, so restarting 0.9.9 pods crash-loop. Don't re-run the upgrade while the migrate Job is still running: that deletes the running Job and orphans its index build in the database. With `migrations.enabled=false`, run `knot-server migrate` to completion before rolling out pods: otherwise every new pod migrates at startup, before it answers its liveness probe, which can kill it mid-build. Five migrations, each its own transaction except the index build: `20261107120000_history_log_columns` (attempts of ≤ 100 ms that pause all access to `doc_updates` and `doc_snapshots`, page loads included; then catalog-only changes); `20261107120001_history_heads` (edit saves and duplicates wait while it counts the stored edits per page, and on 0.9.9 pods a task toggle, accepted suggestion, import or restore holds up its open page meanwhile; each waiting save holds a database connection, so on a busy instance the waiting saves can fill each 0.9.9 pod's pool, after which pages stop loading and API calls fail until the count commits; size the pause by the table, not its index, since nothing guarantees an index-only count: check `SELECT pg_size_pretty(pg_relation_size('doc_updates'))` beforehand and, when `doc_updates` is large, upgrade in a quiet window); `20261107120002_history_heads_fk` (page writes, edit saves and snapshot writes pause for one pass over the per-page rows; on 0.9.9 pods an open page waiting on a task toggle, accepted suggestion, import or snapshot is held up too); `20261107120003_history_rev_index` (built while knot keeps serving; it has no lock loop and no lock timeout, so it waits as long as needed behind an anti-wraparound autovacuum or a long transaction on `doc_updates`: check `pg_stat_activity` before upgrading; it also waits for transactions older than itself, so not during `pg_dump`, and don't restart knot pods meanwhile: a starting pod waits on the migration lock, and the build may then be aborted); `20261107120004_history_rev_index_check` (does nothing unless the index build was interrupted or aborted; then all access to page edits, open 0.9.9 pages included, pauses for one index build of `doc_updates`). If the migrate Job dies during the index build, the build may keep running in the database: check `pg_stat_progress_create_index` before re-running, and wait for it or end it with `pg_terminate_backend`. In the other four migrations, lock attempts and the give-up message work as in 0.9.9; their autovacuum handling no longer pauses readers. `helm upgrade --timeout` of the count time plus twice the index build time plus 8 minutes. 0.9.9 pods that restart after the first migration commits crash-loop until new pods replace them; rolling back needs a database restore. While 0.9.9 and 0.9.10 pods run side by side, editors on a 0.9.9 pod may not see edits saved by other pods while the upgrade runs (especially edits made during the third migration's pause) until they reconnect; nothing is lost, and the page is complete once it opens on 0.9.10; on an instance with many concurrent editors, scale 0.9.9 to one replica or pick a quiet window. A page's Markdown export, My Tasks entries and search results may stay stale until the page is next edited, and an edit a 0.9.9 pod failed to save during the rollout may be lost unless its author is still connected. The first time each page edited before the upgrade opens, knot numbers its stored edits once and replays its whole stored history: that first load is slower, and `doc_updates` grows by up to the size of the renumbered rows until autovacuum reclaims it.
  - **Changed:** edits that could not be saved during a database outage are retried in order instead of being dropped, also after their editor left and when a pod shuts down (for up to 10 s; the chart's default `terminationGracePeriodSeconds` is now 50 to leave room for it, and a shorter value can lose unsaved edits when a pod stops during a database outage); a save that waits on a lock gives up after 8 s in the database and is retried, so a stalled save no longer holds a database connection; after a long outage or a refused save, editors reconnect and resend their changes, and a page reopens on a pod only once that pod has saved what it held. Changes made by knot itself (task checkboxes, imports, templates, restores, accepted suggestions) reach other editors only once they are saved; during an outage they fail after about 5 to 25 s and the page's editors reconnect. A page whose stored edits include one that cannot be applied opens again (it used to fail to load).
  - **Fixed:** a page open on several pods could miss an edit another pod saved at the same moment, until the page's room restarted; pages that already missed one this way get it back the first time they open on 0.9.10. Purging a page while someone edits it no longer risks a database deadlock.
  - **Added:** metrics `knot_writer_flush_seconds`, `knot_writer_flush_retries_total`, `knot_room_rev_gap_total`, `knot_room_row_apply_errors_total`, `knot_room_updates_persisted_raw_total`, `knot_history_legacy_mismatch_total`.
- [ ] **Step 2: Helm README `### Upgrading from 0.9.9`**: the same facts in the 0.9.8 section's depth: what each migration locks and for how long, the `--timeout`, the halt table keyed on `SELECT version FROM _sqlx_migrations WHERE version >= 20261107120000` (none: 0.9.9 keeps working, re-run later; some but not all: 0.9.9 pods that restart crash-loop, end the blocker and re-run promptly, but first check `SELECT pid, phase FROM pg_stat_progress_create_index` (or `pg_stat_activity` for `CREATE UNIQUE INDEX CONCURRENTLY`): a killed Job can leave the index build running in the database, holding the migration lock; wait for it to finish or `pg_terminate_backend(pid)` it, else the re-run hangs or fails without explanation; all five: re-run now), the lock-holder query of the 0.9.8 section extended with `doc_snapshots` and `doc_history_heads`, migration 3's pause (page writes, edit saves and snapshot writes, and on 0.9.9 pods open pages waiting on a task toggle, accept, import or snapshot), migration 2's pool-exhaustion warning (as the 0.9.8 section words it: the waiting saves can fill each old pod's database pool, so pages may stop loading) and the quiet-window advice, the index build's missing lock loop (check `pg_stat_activity` for an anti-wraparound autovacuum or a long transaction on `doc_updates` first; the repair pauses all access to page edits, open 0.9.9 pages included, for one index build), the `--timeout` formula (count + 2 × index build + 8 minutes), the index build's "don't restart pods meanwhile" (a starting knot-server, 0.9.9 or 0.9.10, migrates first and waits on sqlx's advisory lock; that wait can abort the concurrent build, after which the Job's retry runs the repair migration), the rollout caveat (editors on a 0.9.9 pod may not see edits saved by other pods while the upgrade runs, especially edits made during the third migration's pause, until they reconnect; nothing is lost, and the page is complete once it opens on 0.9.10; with many concurrent editors, scale 0.9.9 to one replica or pick a quiet window; a page's Markdown export, My Tasks entries and search results may stay stale until its next edit; an edit a 0.9.9 pod failed to save during the rollout may be lost unless its author is still connected), the pre-flight checks for all five migrations (no `pg_dump`, long transaction or anti-wraparound autovacuum; size the count by `pg_relation_size('doc_updates')`, or `EXPLAIN` the seed's `SELECT` first), "don't re-run the upgrade while the migrate Job is still running: that orphans the index build", the `migrations.enabled=false` line (run `knot-server migrate` to completion first: pods migrate at startup, before `/api/healthz` answers, and the liveness probe can kill one mid-build), what `knot_room_row_apply_errors_total` means (a stored edit that cannot be applied; the page opens with what of it applies, and its boot cache is no longer refreshed; report it with the doc id), that a pod's shutdown waits up to 10 s after the HTTP drain to save buffered edits, so the shutdown budget is `preStopSleepSeconds` + 30 s (HTTP request timeout) + 10 s, plus a few seconds of slack (the budget includes them): the chart's default `terminationGracePeriodSeconds` is now 50, and a shorter value can lose buffered edits when a pod stops during a database outage, the once-per-page numbering and its temporary growth, what the error log "legacy rows do not match the count taken at upgrade" (metric `knot_history_legacy_mismatch_total`) means — the page's stored edits were changed outside knot, the page will not open, report it with the doc id; the edits themselves are intact — and "don't skip 0.9.10". Extend the storage bullet: rows are now smaller (only what each edit changed is stored).
- [ ] **Step 3: Architecture deviation notes**: one paragraph "Deviations recorded in the M1.2 plan" summarising each Ruling of this plan that differs from the architecture text (five migrations and why; the FK split and the snapshot epoch trigger moved to migration 3; `knot_lock_tables`; the repair migration and the advisory-lock/CIC interaction; `NOT VALID` check; trigger on every column and the explicit-column rule; both heads-upsert insert arms number a page's first rows from 1 (trigger path `head_rev = 1`, flush path `head_rev = n`, so base = 0, where the architecture's `VALUES ($1)` gives rev 0 and base −n) and set `last_write_at = clock_timestamp()`, so M1.3's `ORDER BY last_write_at` never meets a NULL; every flush rewrites `last_write_at`, an indexed column, so each flush leaves one non-HOT heads version for vacuum (the architecture's "the heads row does not bloat" means one heads update per flush, not one per row; kept because M3's `page_active` reads `last_write_at` as the latest write); `created_at` override only for the default; the lock order `documents` (row KEY SHARE) → `doc_history_state` (row) → `doc_history_heads` (row) → `doc_updates` → `doc_contributors` → `doc_snapshots`, with the rule that the page row's KEY SHARE is the first lock of any transaction that locks a page-scoped history row (`doc_history_state`, `doc_history_heads`, or the page's `doc_updates` rows), which amends the architecture's "Lock order is doc_history_state → heads, the same as erase" (entry_builder BUILDER) to "documents (KEY SHARE) → doc_history_state → heads": M1.3's builder runs `SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE` before its `doc_history_state` claim (`FOR UPDATE SKIP LOCKED`) and before inserting a missing `doc_history_state` row, and M3's erase before Phase B step 2's first SELECT (`… FROM doc_history_state … FOR UPDATE`), because a purge holds the page row and cascades into `doc_history_state`, and nothing retries an erase (M1.2 follows it for appends, trigger-path inserts, snapshot writes and legacy numbering), and the purge retry; migration 3 asks for `doc_history_heads` before `documents` in its helper call (the wait goes to the busy table); the trigger's `NEW.rev IS NOT NULL` arm takes no page lock, unlike its trigger path; immutable stamped snapshots; titles in `meta`; legacy restore meta; persist-before-fan-out for server-side events and its deadline; retried batches may land twice (M1.3 must show no entry for a row that integrates nothing); rejected updates stop snapshots, and M1.3's replay must tolerate rows that stay pending forever because they build on unlogged content; seq kept for the Markdown cache; once-per-flush bus publish; boot fails on read errors and on a legacy count mismatch; a legacy snapshot triggers a full replay (at a page's first room boot, and on every read-only load (Markdown refresh and sweep, Duplicate without a live room, export-check) of a page no room has opened since the upgrade, since only a room writes a stamped snapshot); the architecture's "snapshot_seq keeps old replicas' seq-based hydration working" does not hold for trigger-path rows during the rollout (many pages at once after migration 3's pause; 0.9.9 editors miss those edits until they reconnect); `ensure_legacy_numbered` only at room boot in M1.2, short-circuited by the `rev = legacy_rows` probe; the lone wait in `SHARE UPDATE EXCLUSIVE`; the server-job deadline armed by the first failed attempt; Evict Fatal still writing the jobs ahead of its cause, the per-replica drain barrier in `acquire` and the shutdown drain, with the residual that a client reconnecting to another replica while one drains can be credited with the drained text; every page seeded into `doc_history_heads` with `last_write_at` = the migration's `now()`, so M1.3's builder visits pages whose rows were pruned, oldest first; the snapshot invariant as `base ∪ replay(R)`, and M1.3's genesis must apply every surviving row with `created_at < history_since`, including rows with seq ≤ the baseline's `snapshot_seq` (the architecture's "seq > its snapshot_seq" would leave out a row the late-seq skip kept out of the baseline but the first stamped snapshot holds, and M1.3 would write a spurious authorless gap entry for it on every such page; replay is idempotent, so the extra rows change nothing else), and M3's erase rewrite of snapshots must keep the base; `doc_snapshots_epoch` honours `knot.history_rewrite`, so M3's erase may insert its snapshot at the new epoch before it bumps `content_epoch`; `ApplyUpdate` persists its effective update (or the raw bytes), not an observed update; append attempts are bounded inside Postgres (`lock_timeout`/`statement_timeout` 8 s, and a cancel for an abandoned attempt); the chart's grace period raised to 50 s; deterministic store errors are permanent and timeouts halve the writer's batch; read-only loads still fail on a row that does not apply, while room boots pass it; an `ApplyUpdate` past `MAX_NESTING` is refused and evicts; the spike's yjs-side half rests on the CI Playwright gate; rows that fail to apply are passed at boot and in catch-up and stop snapshots (M1.3's replay must pass them too); edits other editors saved inside unlogged content are lost from the live page at the next boot; content only a 0.9.9 pod's legacy snapshot held during the rollout is lost once a stamped snapshot exists, with no gap entry), plus the Task 1 spike outcome.
- [ ] **Step 4: Extend the Helm upgrade test** (review round 3: today it seeds a title-only page and reads it back, so it never boots a room over legacy rows with the new binary). In `.github/workflows/helm-upgrade.yaml`'s seed step, after creating the canary page, write content through the 0.9.9 API: `api_md POST "/api/docs/$DOC/markdown?mode=replace"` with a body of a heading, one paragraph `upgrade canary text` and one task `- [ ] upgrade canary task`, then `api GET "/api/docs/$DOC/markdown"` and save it to `/tmp/doc.before.md` (these go through a 0.9.9 room, so they leave legacy rows and, at room exit or idle, a legacy snapshot). In the verify step, with the old cookie: `GET /api/docs/$DOC/markdown` must contain `upgrade canary text` and `- [ ] upgrade canary task` (no room is live on the fresh pod, so this is the read-only load, `stored_snapshot` → `BootMode::ReadOnly`, over unnumbered legacy rows; it numbers nothing); `api_md POST "/api/docs/$DOC/markdown?mode=append"` with `appended after upgrade` (this boots the 0.9.10 room over the legacy rows: numbering and full replay); `PATCH /api/docs/$DOC/tasks/0` with `{"checked": true}` (`tasks.rs:24-27`, body `PatchBody` :126); then the Markdown must contain the original paragraph, `appended after upgrade` and `- [x] upgrade canary task`, each once. Fail with `::error::` naming which assertion broke. (The workflow runs on `migrations/**`, so this PR triggers it.)

- [ ] **Step 5: End-to-end check** against a per-run database (a branch that adds migrations must not migrate the shared `knot` database). First make `reset()` follow it: in `e2e/support/reset.ts:73` replace the hard-coded `-d knot` with `-d ${process.env.KNOT_E2E_DB ?? "knot"}` (CI sets nothing and keeps `knot`; without this, `reset()` truncates the shared database, where `doc_history_heads` does not exist, and every spec fails with "relation does not exist"). Then (review round 5: `CI=1` would make Playwright start its own `cargo run --bin knot-server`, since `reuseExistingServer: !process.env.CI`, `e2e/playwright.config.ts:49, 82`, against the hard-coded shared `knot` database, `:58`, or abort on the busy port; so leave `CI` unset and let Playwright reuse the server started here):
  1. Stop any server on :3000 and :5173. `make compose.up` (Postgres, Dex on :5556, toxiproxy). Dex is required: the e2e server enables OIDC and knot-server exits 2 when OIDC discovery fails.
  2. Collab traffic: Vite runs with `VITE_COLLAB_VIA_PROXY=1`, so the editor connects to :3001. Check `curl -s localhost:8474/proxies`; toxiproxy runs with host networking, which Docker Desktop for Mac does not expose to the Mac, so when :3001 does not reach :3000, run a TCP forwarder in the background (from the scratchpad, not the repo): `node -e 'require("net").createServer(c=>{const u=require("net").connect(3000,"127.0.0.1");c.pipe(u);u.pipe(c);c.on("error",()=>u.destroy());u.on("error",()=>c.destroy())}).listen(3001)'`. Without either, every collab spec fails, not only `ws-reconnect`; with the forwarder, `ws-reconnect` (it needs toxiproxy's admin API) fails locally and the CI `playwright` job counts for it.
  3. `docker compose -f deploy/compose/dev.yml exec -T postgres createdb -U knot knot_e2e_ordered_log`; `cargo build --bin knot-server`.
  4. Start `./target/debug/knot-server` in the background from the worktree root with the full env of `e2e/playwright.config.ts:56-76`, only the database swapped: `KNOT_DATABASE_URL=postgres://knot:knot@localhost:5432/knot_e2e_ordered_log KNOT_SESSION_KEY=test-key-32-bytes-aaaaaaaaaaaaaa KNOT_OIDC_ENABLED=true KNOT_OIDC_ISSUER=http://localhost:5556/dex KNOT_OIDC_CLIENT_ID=knot KNOT_OIDC_CLIENT_SECRET=knot-dev-secret KNOT_OIDC_REDIRECT_URL=http://localhost:5173/auth/oidc/callback KNOT_BASE_URL=http://localhost:5173 KNOT_OIDC_AUTO_PROVISION=always KNOT_SNAPSHOT_EVERY_N=1` (the history spec relies on `KNOT_SNAPSHOT_EVERY_N=1`); wait until `curl -sf localhost:3000/api/healthz` answers.
  5. `cd e2e && KNOT_E2E_DB=knot_e2e_ordered_log npx playwright test flows/collab.spec.ts flows/two-users-converge.spec.ts flows/ws-reconnect.spec.ts flows/history.spec.ts flows/duplicate.spec.ts flows/templates.spec.ts flows/suggestions.spec.ts flows/import-markdown.spec.ts flows/import-export.spec.ts` (Playwright reuses the server on :3000 and starts Vite itself).
Expected: PASS (yjs clients accept the effective updates), except `ws-reconnect` when only the forwarder runs (above); the `ci` workflow's `playwright` job runs it with real toxiproxy and is a merge gate ("After M1.2"). Then `SELECT count(*) FROM doc_updates WHERE rev IS NULL AND doc_id IN (SELECT doc_id FROM doc_history_heads WHERE legacy_rows = 0)` in that database is 0. Afterwards stop the server and the forwarder, and `DROP DATABASE knot_e2e_ordered_log WITH (FORCE)`.
- [ ] **Step 6: Verify**

Run: `make lint && make test`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add CHANGELOG.md deploy/helm/knot/README.md docs/superpowers/specs/2026-11-04-page-history-architecture.md .github/workflows/helm-upgrade.yaml .github/scripts/knot-upgrade-lib.sh e2e/support/reset.ts
git commit -m "docs(history): 0.9.10 upgrade notes and M1.2 deviations"
```

---

## After M1.2

Merge gates: CI's "previous release → working tree" Helm upgrade test (`.github/workflows/helm-upgrade.yaml`, triggered by `migrations/**`) runs 0.9.9 → this branch and must pass before merge, and so must the `ci` workflow's `playwright` job (`.github/workflows/ci.yml:104`), whose `ws-reconnect` (real toxiproxy, often not runnable locally) and `two-users-converge` are the yjs-side half of the effective-update spike; with Task 11 Step 4 it writes content and a task through 0.9.9 and reads, appends and toggles them through 0.9.10, so it boots a room over legacy rows with the real new binary. It still runs one replica and no concurrent 0.9.9 writer during the upgrade; those paths are covered only by the filtered-migrator DB tests (Tasks 3, 7, 10). The release is the merge plus an annotated `v0.9.10` tag. M1.3 (crates/knot-history: builder, entries, read API) starts from the rev-ordered, labelled log this milestone leaves; it adds its own `ensure_legacy_numbered` call on a builder's first visit. M1.3 inherits, from this plan: every page has a heads row (dirty, `last_write_at` = the upgrade's `now()`) from the seed, so ordering by `last_write_at` reaches every seeded page before pages written after the upgrade (over many ticks, not one pass); a stamped snapshot equals `base ∪ replay(R)`, so gap detection must compare against the genesis-based replay, and genesis must apply every surviving row with `created_at < history_since`, including rows with seq ≤ the baseline's `snapshot_seq` (replay is idempotent), or pages that hit the late-seq skip (`room.rs:755`) get spurious authorless gap entries; M3's erase sets `knot.history_rewrite` to write its snapshot at the new epoch before bumping `content_epoch`; the lock order `documents` (row KEY SHARE) → `doc_history_state` (row) → `doc_history_heads` (row) → `doc_updates` → `doc_contributors` → `doc_snapshots` binds M1.3 and M3 too, with the rule that the page row's KEY SHARE is the first lock of any transaction that locks a page-scoped history row (`doc_history_state`, `doc_history_heads`, or the page's `doc_updates` rows): M1.3's builder runs `SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE` before its `doc_history_state` claim (`FOR UPDATE SKIP LOCKED`) and before inserting a missing `doc_history_state` row, so before the heads row it clears `dirty` on; M3's erase runs it before Phase B step 2's first SELECT (`SELECT … FROM doc_history_state WHERE doc_id = $1 FOR UPDATE`), so before its `doc_history_heads … FOR UPDATE` and its step 5 snapshot insert (which takes the page row's KEY SHARE again through `doc_snapshots_epoch` and the FK); otherwise it deadlocks with a purge, which holds the page row FOR UPDATE and cascades into `doc_history_state` and `doc_history_heads`, and nothing retries an erase; M1.3's `doc_history_state` table gets `ON DELETE CASCADE` to `documents`, a TRUNCATE entry in `e2e/support/reset.ts` and a line in `migrations_apply.rs`'s `expected`; rows may repeat (a retried attempt that had committed) or never integrate (unlogged content, rows that fail to apply), and both must produce no entry.
