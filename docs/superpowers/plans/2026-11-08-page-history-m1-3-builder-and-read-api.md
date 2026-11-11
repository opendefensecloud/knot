# Page history M1.3: builder and read API (implementation plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From release 0.9.11 on, every page's history is built in the background into timeline entries (one per person's editing session, plus restore, copy, import, template, gap and pre-rollout baseline entries, each with who and when, and word counts for sessions), and editors can page through them with `GET /api/docs/{id}/history/entries`. The old History drawer is unchanged.

**Architecture:** A new crate `crates/knot-history` replays each page's rev-ordered log (M1.2) on a GC'd yrs doc, outside every room: every replica runs a builder that claims dirty pages with a short committed lease on their `doc_history_state` row (which also commits a crash marker, so a page that kills the process is backed off and eventually given up; no lock is held while it computes), credits each row's *effect* (the transaction's insert and delete sets) to its author, credits content that integrates late to the row that parked it, groups effects into per-author sessions sealed after 20 idle minutes, writes checkpoints by the max(64 KiB, state size) policy, and records content that a room snapshot holds but no row carries as authorless gap entries. Sessions get word counts from the same union-doc diff M1.4's "Show changes" will render. A list endpoint pages through the entries with a keyset cursor, behind `require_editor`, with names resolved by `history_author_view` (also used by the contributors API; the "Former member" output is wired but inert until M4).

**Tech Stack:** Rust (yrs 0.27.3, sqlx 0.9, tokio, axum 0.8), Postgres 18, Helm chart values, Playwright reset list (TypeScript).

**Spec:** [docs/superpowers/specs/2026-11-04-page-history-design.md](../specs/2026-11-04-page-history-design.md), technical architecture [docs/superpowers/specs/2026-11-04-page-history-architecture.md](../specs/2026-11-04-page-history-architecture.md) (deviation notes at its top; sections `storage_model` → NEW TABLES 4-6 and OPS, `entry_builder` → BUILDER to READ API, `attribution`, `view_and_diff` → SHOW CHANGES (for word counts only), `former_member` → READ PATH, `baseline` → "M1.3 builder, first visit", `risks`, `milestones` → M1.3, `open_spikes`). Earlier milestones, whose Rulings and "After M1.x" sections bind this plan: [M1.1](2026-11-04-page-history-m1-1-stop-the-loss.md), [M1.2](2026-11-07-page-history-m1-2-ordered-log.md).

All commands run in the worktree `/Users/christianhuening/code/odc/knot/.claude/worktrees/page-history-m1-3`.

## Global Constraints

- Release 0.9.11. Never edit an applied migration (`20261104*`, `20261107*`): sqlx checks checksums.
- One migration, `migrations/20261108120000_history_builder.sql`, one transaction, run by the Helm `pre-upgrade` Job while 0.9.10 pods keep serving. Its first statements: `SET LOCAL lock_timeout = '5s'; SELECT knot_lock_tables('20261108120000_history_builder', ARRAY['documents', 'users', 'doc_history_baselines']::regclass[], 'SHARE ROW EXCLUSIVE');` (the helper from `20261107120000_history_log_columns.sql`; the new tables' foreign keys need SHARE ROW EXCLUSIVE on each referenced table, and the helper never waits on one table while holding another). Then only `CREATE TABLE` / `CREATE INDEX` on the new, empty tables. Nothing after the helper takes a lock the app holds; no `ACCESS EXCLUSIVE` anywhere.
- Tables, exactly:
  - `doc_history_state(doc_id uuid PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE, built_rev bigint NOT NULL DEFAULT 0, genesis_rev bigint NULL, builder_version int NOT NULL DEFAULT 0, snapshot_checked_rev bigint NOT NULL DEFAULT 0, parked jsonb NOT NULL DEFAULT '[]', skip_revs bigint[] NOT NULL DEFAULT '{}', skip_baselines bigint[] NOT NULL DEFAULT '{}', failures int NOT NULL DEFAULT 0, timeouts int NOT NULL DEFAULT 0, crashes int NOT NULL DEFAULT 0, retry_after timestamptz NULL, last_error text NULL, claimed_by uuid NULL, claimed_until timestamptz NULL, updated_at timestamptz NOT NULL DEFAULT now())`. `genesis_rev` NULL means "genesis not built yet"; `retry_after = 'infinity'` means "given up" (Rulings); `claimed_by` / `claimed_until` are the builder lease; `skip_revs` are rows a replay never applies (a row whose apply panicked or corrupted the replay); `skip_baselines` are `doc_history_baselines` ids genesis never starts from (a baseline whose merge panicked or hung; their entries stay listed); `timeouts` counts replay timeouts with no input to blame and `crashes` passes that died with the process, both since the last release (Rulings).
  - `doc_history_checkpoints(doc_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE, rev bigint NOT NULL, kind text NOT NULL CHECK (kind IN ('genesis','periodic','gap')), state_bytes bytea NOT NULL, state_vector bytea NOT NULL, created_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (doc_id, rev))`.
  - `doc_history_entries(id bigserial PRIMARY KEY, doc_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE, kind text NOT NULL CHECK (kind IN ('session','restore','copy','import','template','erase','baseline','gap')), author_id uuid NULL REFERENCES users(id), started_at timestamptz NOT NULL, ended_at timestamptz NOT NULL, start_rev bigint NULL, end_rev bigint NULL, sealed bool NOT NULL, baseline_id bigint NULL REFERENCES doc_history_baselines(id) ON DELETE CASCADE, ins_ids bytea NULL, del_ids bytea NULL, words_added int NULL, words_removed int NULL, words_pending bool NOT NULL DEFAULT false, meta jsonb NOT NULL DEFAULT '{}', updated_at timestamptz NOT NULL DEFAULT now())`, `CHECK (kind = 'baseline' OR end_rev IS NOT NULL)`. Indexes: `doc_history_entries_log_idx (doc_id, end_rev DESC, id DESC)`, `doc_history_entries_open_idx (doc_id, author_id) WHERE NOT sealed`, `doc_history_entries_idle_idx (ended_at) WHERE NOT sealed`, `doc_history_entries_words_idx (doc_id) WHERE words_pending`, `doc_history_entries_baseline_idx (baseline_id) WHERE baseline_id IS NOT NULL` (the FK's `ON DELETE CASCADE` from each purged baseline probes it; without it every purge scans the whole entries table once per baseline).
  - `ins_ids` / `del_ids` are `yrs::IdSet::encode_v1()` bytes; NULL reads as the empty set.
- Lock order (M1.2, binding): `documents` (row, KEY SHARE) → `doc_history_state` (row) → `doc_history_heads` (row) → `doc_updates` → `doc_contributors` → `doc_snapshots`; the page row's KEY SHARE is the first lock of any transaction that locks a page-scoped history row. `doc_history_checkpoints` and `doc_history_entries` rows are written only by the holder of the page's builder lease (`claimed_by`, checked under the state row's `FOR UPDATE` in every write transaction; M3's erase takes the lease the same way), or by `give_up`'s claim form, which holds the state row's `FOR UPDATE` while nobody holds the lease, and the builder writes them before its final heads UPDATE (Rulings).
- Builder pass, transactions in order (Rulings: no transaction stays open across compute, so no lock is held while yrs runs):
  1. **Claim** (`SET LOCAL lock_timeout = '1s'`): `SELECT 1 FROM documents WHERE id = $1 FOR KEY SHARE` (no row: `Gone`); `INSERT INTO doc_history_state (doc_id) VALUES ($1) ON CONFLICT (doc_id) DO NOTHING`; `SELECT …, retry_after = 'infinity' AS given_up, coalesce(retry_after > now(), false) AS deferred, coalesce(claimed_until > now(), false) AS leased FROM doc_history_state WHERE doc_id = $1 FOR UPDATE SKIP LOCKED` (no row: `Busy`; `retry_after` itself is never selected, next constraint); **version lift**, before every other check: `builder_version < BUILDER_VERSION AND (given_up OR genesis_rev IS NOT NULL)` (a page an older builder built or gave up; a page whose first genesis is still failing keeps its counts) → Rust sets `failures`, `timeouts` and `crashes` to 0 and treats `deferred` as false in the values it read (Rulings); `builder_version > BUILDER_VERSION` or `deferred` → `Deferred`; `leased` → `Busy`; `$crashes` = the read `crashes` + 1 when `last_error = 'pass interrupted'` (the previous claim's pass died with its process; a graceful shutdown writes `'shutdown'` instead, Task 7), else the read value; `failures >= MAX_FAILURES` (reason `failures`) or `$crashes >= MAX_CRASHES` (reason `crash`) → give up in this transaction, under this `FOR UPDATE` (`give_up`'s claim form: `WHERE doc_id = $1`, no `claimed_by` predicate, since nobody holds the lease; Task 7) → COMMIT → `Failed`; **clean check**: `SELECT head_rev, dirty FROM doc_history_heads WHERE doc_id = $1` (plain read, no lock) — when `genesis_rev IS NOT NULL AND builder_version = BUILDER_VERSION AND NOT dirty AND built_rev = head_rev` (no heads row counts as `head_rev` 0, not dirty), no stamped snapshot has `snapshot_rev > snapshot_checked_rev` and no open session of the page has `ended_at < now() − SESSION_GAP − SEAL_MARGIN`, COMMIT without touching the row → `Clean` (Rulings); else `UPDATE … SET claimed_by = $me, claimed_until = now() + $lease, failures = $failures, timeouts = $timeouts, crashes = $crashes, retry_after = now() + $backoff, last_error = 'pass interrupted'` (`$timeouts` the read value, 0 after a lift; `$failures` = the read value + 1, which is 1 after a lift, and `$backoff = failure_backoff($failures − 1)`, both bound from Rust: the crash marker); COMMIT. SQLSTATE `55P03` anywhere in the claim → `Busy`, never a failure.
  2. `ensure_legacy_numbered`, in its own transaction (Rulings).
  3. **Read**: one `REPEATABLE READ READ ONLY` transaction loads the pass inputs (sizes checked in SQL first, Rulings), COMMIT.
  4. **Compute** under `run_isolated` with a timeout, no transaction open. Every compute (each genesis attempt, the pass, each word count) is preceded by `renew_lease` (`SET LOCAL lock_timeout = '10s'`; page `FOR KEY SHARE`; state `FOR UPDATE`; `UPDATE doc_history_state SET claimed_until = now() + $lease WHERE doc_id = $1 AND claimed_by = $me AND claimed_until > now()`; no row matched → `Failed`, "lease lost", no `record_failure`), so the lease always covers the compute's timeout plus 30 s (Rulings).
  5. **Write** (`SET LOCAL lock_timeout = '10s'`, above `APPEND_STATEMENT_TIMEOUT` = 8 s, `updates_store.rs:202`): page `FOR KEY SHARE` (no row: `Gone`); state row `FOR UPDATE`; unless `claimed_by = $me AND claimed_until > now() AND built_rev = <the loaded built_rev>`, roll back → `Failed` ("lease lost"); checkpoints, entries, state (with `skip_revs` extended by the pass's `refused_revs`); when nothing is left for the words phase and the pass found no rev gap, also the release and the heads UPDATE below; COMMIT. A write with `stats.rev_gap` never releases and never runs the words phase: `record_failure` follows it (Rulings).
  6. **Words phase** (Task 7), each step its own short lease-checked transaction.
  7. **Release**, in the last write transaction, or in a transaction of its own after the words phase (`SET LOCAL lock_timeout = '10s'`; page `FOR KEY SHARE`, no row: `Gone`; state row `FOR UPDATE`; unless `claimed_by = $me AND claimed_until > now()`, roll back → `Failed`, "lease lost", with no `record_failure`, since the lease is someone else's): state `failures = 0, timeouts = 0, crashes = 0, retry_after = NULL, last_error = NULL, claimed_by = NULL, claimed_until = NULL, updated_at = now()`, then last `UPDATE doc_history_heads SET dirty = (head_rev > $2 OR $3) WHERE doc_id = $1` (`$2` the new `built_rev`, `$3` whether a sealed session of the page still has `words_pending`).
- Constants (`crates/knot-history/src/builder.rs` unless stated): `BUILDER_VERSION: i32 = 1`; `TICK = 2 s`; `CLAIM_LIMIT = 20`; `SWEEP = 60 s`; `DEBOUNCE = 10 s` (`BuildLimits::debounce`) and `DEBOUNCE_MAX_LAG_REVS: i64 = 200` (Rulings); `LEASE = 5 min` (`BuildLimits::lease`); `MAX_FAILURES: i32 = 10`; `MAX_TIMEOUTS: i32 = 2`; `MAX_CRASHES: i32 = 3`; `PASS_MAX_ROWS = 5_000`; `PASS_MAX_ROW_BYTES = 16 MiB`; `PASS_MAX_SNAPSHOTS = 8` (snapshots probed per pass); `PASS_ROWS_BUDGET = 10 s` (compute time for new rows, counted from the first new row's apply, so the checkpoint decode and the replay prefix never eat it; at least one new row per pass); `PASS_WORDS_BUDGET = 10 s` (wall time of the words phase); `REPLAY_TIMEOUT = LEASE − 30 s` (one pass or genesis compute); `WORDS_TIMEOUT = 60 s` (one word count; `Builder::new` asserts `replay_timeout` and `words_timeout` are both below `lease`); `REPLAY_STACK = 16 MiB` (`isolate.rs`). Every replay's input (checkpoint or baseline, rows, gap states, snapshots) is capped by `history_replay_max_bytes` (Config; Rulings); there is no fixed build cap; `pub fn failure_backoff(failures: i32) -> chrono::Duration` = `min(30 s × 2^min(failures, 7), 1 h)` (computed in Rust: `interval * 2 ^ failures` overflows in Postgres from 39). `sessions.rs`: `SESSION_GAP = 20 min`, `SEAL_MARGIN = 60 s`. `pass.rs`: `CHECKPOINT_FLOOR_BYTES = 64 * 1024`, `CHECKPOINT_STATE_FRACTION: (u64, u64) = (1, 1)` (Task 5 may change it to `(1, 2)`). `list.rs`: `DEFAULT_LIMIT = 50`, `MAX_LIMIT = 200`, `CATCHING_UP_REVS = 500`. `sizes.rs`: `SIZE_SAMPLE_EVERY = 10 min`, `SIZE_LOCK_KEY: i64 = 0x6b6e_6f74_6873_0001` ("knoths" + 1).
- Config (`crates/knot-config/src/lib.rs`): `history_workers: u32` (env `KNOT_HISTORY_WORKERS`, default 1, Rulings (CPU); `0` disables the builder and the size sampler on that replica, logged as `history builder disabled`); `history_replay_max_bytes: u64` (env `KNOT_HISTORY_REPLAY_MAX_BYTES`, default `DEFAULT_HISTORY_REPLAY_MAX_BYTES`, set by Task 5's spike; caps the input of every replay, builds and word counts alike; validated ≥ 1 MiB, message `"KNOT_HISTORY_REPLAY_MAX_BYTES must be >= 1048576"`; the env spelling, as the existing messages at `lib.rs:231-236`); `history_cpu_share: f64` (env `KNOT_HISTORY_CPU_SHARE`, default `0.5`, the share of one core each worker may use on average, Rulings (CPU); validated `0 < x ≤ 1`, message `"KNOT_HISTORY_CPU_SHARE must be > 0 and <= 1"`). Helm: `history.workers`, `history.replayMaxBytes`, `history.cpuShare` → those env vars through `templates/configmap.yaml`.
- Session rule: rows whose source is NULL, `edit`, `task` or `suggestion` join their author's open session when `row.created_at − session.ended_at ≤ SESSION_GAP`, else that session seals and a new one opens. Rows with `restore`, `restore_fallback`, `import`, `template` or `copy` seal the same author's open session and become a sealed single-row entry (`restore_fallback` → kind `restore` with `meta.fallback = true`). A row whose effect (after parked-claim crediting) is empty touches no session and creates no entry, except a `copy` row at `rev = genesis_rev + 1` (the page's first row: "Created from …" of an empty source), which still creates its entry. An empty template writes no row at all (the room persists nothing for an update that changes nothing), so it gets no entry (Rulings). A sweep seals sessions with `ended_at < now() − SESSION_GAP − SEAL_MARGIN`, and only in a pass that processed every row up to the head.
- Genesis: `G = coalesce(max(rev), 0)` over the page's rows with `created_at < documents.history_since`; the genesis state is the newest baseline that applies plus every row with `rev ≤ G`, in rev order; it is written as the `genesis` checkpoint at rev `G` (also for new pages: an empty state at rev 0). `built_rev`, `snapshot_checked_rev` and `genesis_rev` start at `G`.
- Never decode `doc_history_state.retry_after` into chrono: `'infinity'` decodes through `postgres_epoch_datetime() + Duration::microseconds(i64::MAX)` (sqlx-postgres 0.9.0 `types/chrono/datetime.rs:56`), and chrono 0.4.45's `NaiveDateTime + TimeDelta` panics on overflow (`naive/datetime/mod.rs:1636`). The claim, the list, the sweep and the gauges compute it in SQL as booleans (`retry_after = 'infinity' AS given_up`, `coalesce(retry_after > now(), false) AS deferred`); writes bind values, never a timestamp read back; test assertions use the same SQL predicates (`SELECT retry_after = 'infinity'`, `SELECT retry_after > now()`), never `Option<DateTime<Utc>>`.
- Word counts: for sealed `session` entries only, `split_whitespace().count()` over each maximal run of consecutive `Added` (or `Removed`) chunks within one text segment of `diff::text_chunks` (Task 3: yrs 0.27.3's `DiffAssembler::process` starts a new chunk at every changed item and at every format item, `types/text.rs:619-673`, so one word typed in two non-squashed items or bolded mid-word comes back as two chunks; a run never crosses an `XmlText` or a non-string insert, which is what the view renders as one highlighted word); NULL for every other kind, for a session whose replay input (`octet_length` of the starting checkpoint plus the rows through `end_rev`, summed in SQL before any bytes are fetched) exceeds `history_replay_max_bytes`, and for a session whose count errored or panicked. Each session's count is attempted once.
- API: `GET /api/docs/{id}/history/entries?cursor=&limit=` behind `require_doc_role_mw` + `require_editor` (`crates/knot-server/src/routes/api/history.rs:22`), `Cache-Control: private, no-store`. Response JSON exactly: `{"history_since", "history_complete", "catching_up", "unavailable", "entries": [{"id", "kind", "author", "started_at", "ended_at", "open", "words_added", "words_removed", "meta"}], "next_cursor"}`; `author` is an `AuthorView` (`{"key", "name", "former"}`) or `null` (baseline, gap, author-less rows). `meta` is rebuilt per kind from an allowlist (Task 9), never returned raw.
- Metrics (names exact): `knot_history_table_bytes{table}`, `knot_history_builder_lag_revs`, `knot_history_given_up_pages`, `knot_history_replay_threads_stalled` (replay threads of this replica that timed out and still run, Rulings (CPU)) (4 gauges); `knot_history_rows_built_total`, `knot_history_rows_unapplied_total{reason}` (`apply_error` | `undecodable` | `skip_underflow` | `skipped`), `knot_history_rows_parked_total`, `knot_history_gap_total`, `knot_history_snapshots_unchecked_total{reason}` (`refused` | `corrupt` | `undecodable` | `panic` | `timeout` | `too_large` | `kept_gap`), `knot_history_build_failures_total`, `knot_history_replay_timeouts_total{phase}`, `knot_history_pages_given_up_total{reason}` (`failures` | `too_large` | `timeout` | `crash`), `knot_history_words_skipped_total{reason}` (`too_large` | `diff_error` | `corrupt` | `gap_state` | `panic` | `timeout`), and M1.2's `knot_history_legacy_mismatch_total` (recorded at `updates_store.rs:734`, undescribed until now) (10 counters); `knot_history_replay_seconds{phase}` (`genesis` | `build` | `words`, 1 histogram, with its own buckets `REPLAY_BUCKETS_SECONDS = [0.01, 0.05, 0.1, 0.5, 1, 2, 5, 10, 30, 60, 120, 180, 270, 300]` through `PrometheusBuilder::set_buckets_for_metric(Matcher::Full("knot_history_replay_seconds".into()), ..)`, since `LATENCY_BUCKETS_SECONDS` stops at 30 s and a replay may run `REPLAY_TIMEOUT` = 270 s). `crates/knot-obs/src/metrics.rs` describes them in one `fn describe_history_metrics()` called from `init`, and its unit test pins the names (Task 10). Metrics are recorded by `build_page` on its own task from `PassStats` after `run_isolated` returns, never on the replay thread. Metric assertions in knot-history tests use a local `CountingRecorder` copied from `crates/knot-crdt/src/test_metrics.rs`, extended to keep each gauge's last value (current-thread tests only).
- New crate `crates/knot-history` (workspace member in the root `Cargo.toml`), dependencies all from `[workspace.dependencies]`: `knot-storage` and `knot-config` (path), `yrs`, `sqlx`, `tokio`, `serde`, `serde_json`, `uuid`, `chrono`, `base64`, `metrics`, `tracing`, `thiserror`, `tokio-util` (the shutdown `CancellationToken`); dev: `knot-test-support` (path), `tokio` with `test-util`. `knot-server` gains `knot-history` (path). Commit `Cargo.lock` with the task that changes it (the Dockerfile builds with `--locked`).
- Pure modules (`replay`, `claims`, `sessions`, `diff`, `words`, `canonical`, `genesis`, `pass`) touch no database and no tokio; `store`, `builder`, `list`, `people`, `sizes` do.
- New tables go into `e2e/support/reset.ts` `TABLES` (next to `doc_history_heads`; that list is not alphabetical) and `crates/knot-storage/tests/migrations_apply.rs` `expected` (alphabetical: `doc_history_baselines`, `doc_history_checkpoints`, `doc_history_entries`, `doc_history_heads`, `doc_history_state`).
- Tests that rewrite `doc_updates` (`DELETE`, `UPDATE`) run `SET LOCAL knot.history_rewrite = 'on'` in the same transaction (the append-only trigger raises KH001 otherwise). Rows with an explicit `created_at` are inserted raw **without** `rev` (the trigger assigns the rev, keeps the explicit `created_at`, bumps `head_rev` and sets `dirty`, `20261107120001_history_heads.sql:111-127`). An explicit `created_at` must be at or after the page's `documents.history_since` (which defaults to the page's creation time, `20261104120001_history_since.sql:132`), unless the test means legacy rows: choose times as `history_since + offset`, or backdate `history_since` (and `created_at`) first; a row before it folds into genesis instead of forming a session.
- Verification: `make lint`, `make test.rust` (cargo nextest against `knot-dev-postgres` on :5432), `make test.web`. After adding a migration run `cargo clean -p knot-storage -p knot-test-support -p knot-history` once if tests report a missing table (`migrate!` embeds at build time). Local Playwright is not possible; CI runs it. Commits are SSH-signed through 1Password; never bypass signing.

## Review Focus

1. **A page archived, restored, moved or purged while a builder pass on it computes.** The page operation must not wait for the compute, nothing may deadlock, and a purge must leave no history rows. Pinned in Task 7 (`a_purge_racing_builder_passes_never_deadlocks`, `a_long_pass_holds_no_lock_on_the_page`, which archives, restores, moves and attempts an owner-grant removal during the compute).
2. **A page built over many small passes, a snapshot that lands after its rev was built, and a rebuild after that snapshot was thinned.** Resuming from the latest checkpoint (also one that holds parked content), the stored parked claims and the open sessions must give exactly the entries one big pass gives, a room's idle snapshot written 30 s after the build must still be gap-checked, and a rebuild must keep every gap's content and its entry although the snapshot it came from is gone (`a_rebuild_keeps_gap_content_whose_snapshot_is_gone`, Task 7). Pinned in Task 4 (`many_small_passes_equal_one_pass` with a 256-byte checkpoint floor, `a_snapshot_written_after_its_rev_was_built_is_still_gap_checked`) and Task 7 (`small_passes_build_what_one_pass_builds`, `a_late_snapshot_is_gap_checked_by_the_sweep`).
3. **Rows and snapshots that never integrate or that break the replay, in genesis as in later passes.** Content parked for good, a row yrs rejects part-way, an undecodable row, a row that would hit yrs 0.27.3's skip underflow (`block_store.rs:318`), a row or a snapshot probe that panics, corrupts the replay or hangs, a hang with nothing to blame that comes back on every attempt (each one strands a worker), a page that kills the process: no panic escapes, no entry for what did not integrate, the failing row (never an innocent one) is skipped or the failing snapshot left unchecked, the reason is counted, a refused row's content still surfaces as a gap entry, and a crashing page is backed off, then given up, never retried by every replica in turn. Pinned in Task 1 (`the_skip_underflow_is_refused_not_applied`, `a_resend_over_blocks_held_past_a_gap_is_trimmed_and_applied`, `undecodable_and_rejected_rows_are_reported`, `a_checkpoint_with_pending_nested_content_resumes_like_one_that_never_round_tripped`), Task 4 (`a_row_parked_for_good_creates_no_entry`, `a_refused_row_surfaces_as_a_gap_entry`, `a_window_over_a_union_gap_checkpoint_merges_it`, `a_failing_prefix_row_blames_the_checkpoint_not_the_row`, `a_legacy_row_that_corrupts_genesis_is_skipped_and_no_checkpoint_is_written`) and Task 7 (`a_pass_that_dies_mid_compute_defers_the_page`, `a_row_that_always_panics_is_skipped`, `a_prefix_row_that_panics_is_never_skipped`, `a_session_spanning_a_union_gap_gets_its_word_count`, `an_oversized_snapshot_is_left_unchecked_not_given_up`, `a_legacy_row_that_always_panics_in_genesis_is_skipped`, `a_snapshot_whose_gap_merge_corrupts_is_skipped_not_a_row`, `a_hung_replay_times_out_and_backs_off_the_page`, `a_row_held_just_past_the_timeout_never_skips_the_next_row`, `a_replay_that_hangs_while_encoding_a_checkpoint_is_given_up`, `a_hang_with_nothing_to_blame_is_given_up_at_its_second_timeout`, `a_baseline_whose_merge_hangs_is_skipped`, `a_page_that_crashes_the_process_three_times_is_given_up`).
4. **The same update stored twice** (a retried append that had committed, M1.2 Rulings), including a server-side restore or import row. Exactly one entry. Pinned in Task 4 (`a_duplicated_row_creates_no_second_entry`).
5. **A history entry that names a page the reader cannot see** (a restricted template instantiated into an open page, a copy of a restricted page, a purged source). The title and id must not reach the reader. Pinned in Task 9 (`copy_and_template_sources_are_hidden_from_readers_who_cannot_see_them`).

## Rulings

- Ruling: the list endpoint is `GET /api/docs/{id}/history/entries`, not the architecture's `GET /api/docs/{id}/history` — that path is the old drawer's snapshot list (`routes/api/docs.rs:92-95`, `web/src/lib/history.api.ts`), and "the old drawer stays" in M1.3 — cost if wrong: M1.4 points its client at `/history/entries` (or moves the route when it retires the old list).
- Ruling: snapshot thinning stays at "5 newest + 1/day for 30 days" in M1.3; the switch to the 3 newest per page ships with M1.4, which retires the old drawer — the old drawer lists `doc_snapshots` (`history.rs` `list`, 50 newest), and the architecture's baseline section promises no visible regression until M1.4; thinning to 3 would cut 0.9.11's only history UI to three versions — cost if wrong: one more release of today's snapshot storage (up to 35 full states per page).
- Ruling: genesis ends at `G = max(rev)` of rows with `created_at < history_since` and applies every row with `rev ≤ G` — `created_at` is the transaction start, so around the M1.1 migration rows with `created_at < history_since` are not a rev prefix (a save already waiting on the migration's lock has an earlier `created_at` and a later seq); a later row inside that prefix folds into genesis uncredited, which M1.1's ROLLOUT FLAG already accepts; applying rows with seq ≤ the baseline's `snapshot_seq` is what M1.2's "After M1.2" requires (replay is idempotent) — cost if wrong: an edit saved inside the migration's lock window shows as part of "state at rollout" instead of a credited session.
- Ruling: every page gets a `genesis` checkpoint, an empty state at rev 0 for pages created after `history_since` — every replay then starts from a checkpoint — cost if wrong: one tiny row per page.
- Ruling: stamped snapshots with `snapshot_rev ≤ G` are never gap-checked (`snapshot_checked_rev` starts at `G`) — genesis is not a replay position below `G` — cost if wrong: a gap in such a snapshot is reported at the next snapshot instead.
- Ruling: the "state at rollout" baseline entry is written when the page predates `history_since`, its genesis is not empty, and its visible content differs from the newest baseline's (or it has none), compared with `canonical::visible_content` (tags, sorted attributes, text with sorted formatting) — the architecture compares `pm_json`, which M1.4 builds — cost if wrong: an extra pre-rollout entry that looks like the one below it.
- Ruling: a row whose effect is empty (nothing integrated, nothing newly deleted, after crediting parked claims) creates no entry and seals no session — that is how M1.2's "drop a sealed entry whose insertions and deletions are both empty" is met: duplicated commits, resends and no-op server rows never produce an entry. Exception: a `copy` row at `rev = genesis_rev + 1` creates its entry even when empty — the design's copy "starts its own fresh history whose first entry is 'Created from …'", also for an empty source; Duplicate always writes that row (`append_in_tx`, `knot-storage/src/duplicate.rs:137-148`), and a duplicated commit of it has a later rev, so it still creates no second entry — cost if wrong: none.
- Ruling: an empty template gets no "Created from template" entry in M1.3 — `from_template` (`routes/api/docs.rs:1265-1281`) sends `Event::ApplyUpdate`, and `Room::on_apply_update` returns before persisting when the update changes nothing (`crates/knot-crdt/src/room.rs:716-718`), so no `template` row exists for the builder to see; this is an accepted gap, and the follow-up (M2, which touches the from-template path for version metadata) appends the template row directly as Duplicate does — cost if wrong: a page made from an empty template starts its timeline with its first edit.
- Ruling: content that integrates late (parked by an earlier row) is credited to the parked row's author as a contribution at the draining row's rev and `created_at`, fed to the grouper before the draining row's own effect — that content first appears in the page at the draining rev, and adding its ids to the parked row's entry (maybe sealed long ago) would put them outside that entry's window, where "Show changes" cannot show them; the design already accepts offline edits "timestamped at reconnect" — cost if wrong: such text sits in the parked author's session at drain time.
- Ruling: a gap merge credits parked claims exactly like a row: `claims.credit` runs on the merge's effect, each credit is fed to the grouper (the parked author, at the gap's rev and the snapshot's `created_at`) before the gap entry, and the gap entry holds only the remainder; what the merge newly parks is recorded as a *gap claim* (`ParkedClaim::gap = true`, author `None`), and a later credit of a gap claim is fed as gap content (its own sealed `gap` entry at the draining rev), never to the draining row's author — M1.2 says rows can build on unlogged or `base` content and stay pending until it shows up, and in the replay it shows up through exactly that merge; without crediting, a person's text lands in "Changes saved without an author" and their claim stays in `parked` forever — cost if wrong: none.
- Ruling: genesis records what each legacy row newly parks as that row's claim (`rev` = the row's rev ≤ `G`, author `None`), exactly as a pass does, and every credit of a claim with `parked_rev ≤ genesis_rev` is removed from the draining row's effect and fed to nobody — the architecture says rows before `history_since` "only feed genesis and are never credited"; without the claim, legacy content drained by a later row (a 0.9.9 pod's raw resend after `history_since`) would be credited to that row's author — cost if wrong: such content appears in the page with no entry showing it.
- Ruling: a row's parked claim is what it newly parked (pending structs' insertions and pending deletes after the apply, minus before), not its whole update, and the claims live in `doc_history_state.parked` as of `built_rev`; the pending content itself rides in each checkpoint, because `encode_state_as_update_v1` merges pending structs and deletes into its output (yrs `transaction.rs:139-144`) — a resend's whole update would make every claim as large as the page — cost if wrong: M1.2 recorded (`crates/knot-crdt/tests/effective_update.rs:456-462`) that yrs re-encodes pending structs "with wrong parent ids", and `merge_pending_v1` (`transaction.rs:375-391`) builds the checkpoint's pending part from exactly that re-encoding; so Task 1's `a_checkpoint_with_pending_nested_content_resumes_like_one_that_never_round_tripped` decides it (the same-encoder byte comparison in `checkpoint_round_trip_keeps_pending_content` cannot), and Task 4's property tests resume from periodic checkpoints that hold parked content (a 256-byte `checkpoint_floor`).
- Ruling (pending fallback, taken only if that Task 1 test fails): `Replayer::encode_state` drops pending content (`encode_state_as_update`, the encoder-taking method, which skips `merge_pending_v1`); Task 6 adds `pending_revs bigint[] NOT NULL DEFAULT '{}'` to `doc_history_checkpoints` (the revs of the row claims open at that checkpoint, which the pass has in hand when it emits it); every replay from a checkpoint (passes through `PassInput::reapply`, word counts, M1.4 views) first re-applies those rows' original bytes, in rev order, which re-parks their pending structs with their real parent ids; Task 1 records the outcome for Task 11's deviation notes — cost if wrong: gap claims' pending content (no row to re-apply) is lost on resume, and shows as gap content again at the next probe that holds it.
- Ruling: the replay applies a resend or a full state that overlaps blocks it holds past a gap — yrs 0.27.3 trims everything the doc already holds before integrating (`transaction.rs:825-827`; `BlockStore::known_state`, `block_store.rs:372-389`, counts blocks past a gap as known and `BlockSet::exclude` turns them into Skips), so such a row integrates exactly its new content; the review of this plan's round 1 checked this on a debug build, and refusing on overlap would throw away legitimate rows and refuse every gap probe of a page whose replay holds blocks past a gap — cost if wrong: none; `a_resend_over_blocks_held_past_a_gap_is_trimmed_and_applied` and `a_full_state_probe_over_a_gapped_replay_reports_the_missing_range` pin it.
- Ruling: pre-validation refuses only the update shape that reaches the underflow at `block_store.rs:318` (`skip.next_clock() - block.next_clock()`, which panics in debug and wraps in release, corrupting the block list), as reproduced in Task 1 Step 1 from M1.2's property generator with its `has_gaps` guards removed (M1.2 hit it at seed 5, on the log doc); the predicate is written from that reproducer, not guessed. A release-build safety net backs it: after any apply to a replay that held blocks past a gap for a client the update names, the replay checks that its held ranges are within what it held before plus the update's insertions; a violation is `RowOutcome::Corrupt`, `run_pass` and `build_genesis` stop with `Corrupt { rev }`, and the builder adds that rev to `skip_revs` and never writes a checkpoint from the corrupted doc. A refused row's content is missing from the replay; a later room snapshot holds it, often squashed with the blocks after it into one struct, which is the refused shape again, so the probe of that snapshot is refused too. Then the gap check falls back to `Replayer::union`: decode the snapshot into a fresh doc (which holds no blocks past a gap), apply the replay's state to it, and take the set difference as the gap's effect; the union doc replaces the replay, which also clears the replay's past-gap blocks. The gap checkpoint written from it holds that squashed shape too, so every replay that later passes a gap checkpoint's rev (word counts, M1.4 views; a builder pass never does, since it starts at the latest checkpoint at or below `built_rev`) merges it with `Replayer::merge_gap_state`: `merge_state`, falling back to `union` when that is `Refused`; only if the union fails too is that replay given up (a word count: skipped, `gap_state`). In the gap check itself, only when the union is refused as well is the snapshot left unchecked (`knot_history_snapshots_unchecked_total{reason="refused"}`) and `snapshot_checked_rev` moved past it — cost if wrong: that page's gap detection stays blind until a snapshot no longer meets the refused shape; the counter shows it. `a_refused_row_surfaces_as_a_gap_entry` (Task 4) pins the fallback; if Task 1 found no reproducer, it is dropped with `the_skip_underflow_is_refused_not_applied`.
- Ruling: a row yrs rejects part-way is credited with what its transaction integrated before the error, to its author, and counted (`apply_error`); an undecodable row is passed (`undecodable`) — rooms persist what they applied, so a row that fails here failed only because of replay order or a bug, and what did integrate is that row's content — cost if wrong: partially applied content is credited to the author whose row carried it.
- Ruling: all yrs work of a pass, of genesis and of a word count runs on a dedicated thread with a `REPLAY_STACK` stack inside `catch_unwind` (`isolate::run_isolated`), awaited with a timeout (`REPLAY_TIMEOUT`, `WORDS_TIMEOUT`); a panic or a timeout fails the pass (a word count: skips it) — yrs deletes nested types recursively, and some yrs paths panic; a thread panic must not end the process, and in release builds the skip underflow wraps instead of panicking, so an apply can also loop. `run_pass` and `build_genesis` publish what is running through `Progress` (`Stage::Checkpoint` while decoding the starting checkpoint, `Prefix(rev)` while re-applying a row at or below `built_rev`, `Row(rev)` for a new row, `Probe(snapshot_rev)`, `Emit(rev)` while encoding and verifying a checkpoint, else `Idle`); the builder maps a panic, a timeout or a `ReplayError` to the input that caused it (Task 7 step 7): a new row goes to `skip_revs` (every later replay, passes, genesis, word counts and M1.4 views, skips it, counted `unapplied{reason="skipped"}`), a snapshot is left unchecked, a checkpoint is dropped or rebuilt past (also for a failing prefix row, next ruling), and never is an innocent row skipped — a row that panics every time would otherwise stop the page's history for good, and blaming the row being built for a snapshot's failure would drop good content and loop.
- Ruling: a timeout freezes the stage: on `IsolateError::TimedOut`, `build_page` calls `Progress::cancel()`, which stops every later `set` and returns the stage then current and how long it had been current; `run_pass`, `build_genesis` and `replay_window` check `Progress::is_cancelled()` before each row, prefix row and probe and return `Cancelled` (the result is discarded), so a timed-out thread stops at the next row boundary instead of finishing its whole pass. The stage's action (`SkipRev`, `SkipSnapshot`, `DropCheckpoint`, `Rebuild`) applies only when the stage had been current for at least half the timeout; otherwise the timeout is `None` (a page that is merely slow, CFS throttling, a long prefix: backed off, nothing skipped, counted in `timeouts`; the second-timeout ruling below gives it up at its second). A timeout dominated by `Stage::Emit` (a checkpoint encode that hangs: no input to blame, and each attempt strands a permit and a core) gives the page up at once (`pages_given_up_total{reason="timeout"}`); a panic there stays `None` — the replay thread cannot be stopped, so without the freeze a slow row that finished just after the timeout put the next, innocent row into `skip_revs`, and a timeout at `Prefix` cascaded down to rebuilds that skip legacy rows — cost if wrong: a row that hangs for less than half the timeout after a slow prefix is backed off instead of skipped, and is given up at its second timeout.
- Ruling: a timeout with nothing to blame (action `None`) counts `timeouts = timeouts + 1` in its failure transaction, and when that makes `timeouts >= MAX_TIMEOUTS` (2) the same transaction gives the page up (`give_up` after-claim form, reason `timeout`, ERROR log); a panic or a timeout of at least half the timeout at `Stage::Checkpoint` in genesis (the merge of the baseline genesis starts from) blames that baseline: `SkipBaseline(id)` appends it to `skip_baselines`, and the next genesis starts from the next older baseline not in that list (its entry stays listed) — every timed-out replay keeps its permit until its thread ends, so with the default single worker one hang that never ends stops that replica's builder, and a hang that the next replica meets again stops it too: with only `MAX_FAILURES`, every replica would be stuck before the page reached ten failures, and history would stop cluster-wide until pods restart; `Stage::Emit` was already given up at once for that reason. Retrying genesis with an older baseline inside the same `build_page` is not done: it needs a second permit while the first is stranded — cost if wrong: a page whose replay is merely slow enough to time out twice in a row (no release between) is given up until an operator re-enables it (README), and at most two workers are stranded per hanging page.
- Ruling: a row at or below `built_rev` that fails while the pass re-applies it (the replay prefix: a panic or timeout at `Stage::Prefix(rev)`, `Corrupt`, or `Refused`, all reported as `ReplayError::Prefix { rev }`) never goes to `skip_revs`: an earlier pass applied it, credited it and folded it into later checkpoints, and only the decoded checkpoint's block layout differs; the starting checkpoint is dropped (`periodic`, `DropCheckpoint`) or the page rebuilt (`genesis` or `gap`, `Rebuild`), and in the rebuild the row is a new row again. To keep the prefix deterministic, a row that the pass (or genesis) refuses goes to `skip_revs` at once (`PassOutput::refused_revs`, `GenesisOutput::refused_revs`, written with the pass): it was never applied, so skipping it in every later replay is exact — skipping a prefix row as the round-3 table did would drop content already in its author's entry from every later replay, and the next snapshot probe would report it again as an authorless gap — cost if wrong: a row that fails only when re-applied from a decoded checkpoint, with no periodic checkpoint between it and the genesis or gap checkpoint, rebuilds the page once per new edit until a checkpoint past it is written (counted by `knot_history_build_failures_total`).
- Ruling: the worker permits are one `Arc<Semaphore>` of `workers` permits that `main.rs` creates and hands to `Builder::new`, so M1.4's view endpoint can share it through `AppState` and the memory bound `workers × F × cap` (F the doc-memory factor Task 5 measures) stays one bound; the permit rides the replay thread: `build_page` takes an owned permit and hands it to each `run_isolated` call as its guard; the thread hands it back with its result, and on a timeout it stays with the thread until the thread ends — `workers` then bounds live replay threads and their memory, timed-out ones included; on a timeout the page gets its failure (row skipped, backed off) at once, logged at ERROR (`history replay timed out`) and counted (`knot_history_replay_timeouts_total{phase}`), and `knot_history_replay_threads_stalled` counts the thread until it ends (the guard wraps the permit with a shared `AtomicU8` state, 0 running → 1 timed out (CAS by `build_page`, which then adds 1 to the builder's `Arc<AtomicI64>` of stalled threads) → 2 ended (swap in the guard's `Drop`, which subtracts 1 only when it was 1), so a thread that ends during the timeout race is never counted; the gauge is set from that counter on the builder's task, after every `build_page` and in every sweep, never from the replay thread, Global Constraints) — cost if wrong: each hung replay costs its replica one worker until the process restarts; a skipped row cannot hang a second one, and the README tells operators to restart the pod when that gauge stays above 0. What `catch_unwind` cannot catch (a stack overflow on deep pages, `knot-crdt/src/nesting.rs`; a second panic in `TransactionMut::drop` → `commit()`, `transaction.rs:488-492`; an OOM kill) is covered by the claim's committed crash marker (next ruling) — cost if wrong: that process dies, at most `MAX_CRASHES` (3) times per page, each followed by a growing backoff.
- Ruling: the builder holds no transaction across compute: it claims a page with a committed lease (`claimed_by`, `claimed_until = now() + LEASE`), reads its inputs in a short read-only transaction, computes with none open, and writes in a short transaction that re-checks the lease under the state row's `FOR UPDATE` — the page row's `FOR KEY SHARE` held across compute would stall `archive` (`doc_store.rs:874`), restore from archive (`:460`, which first takes the workspace's top-level group lock and holds it while waiting), `move_to` (`:800`), `begin_purge`/`purge` (`:1235`) and owner-grant removal (`grant_store.rs:526`), all `FOR UPDATE` on the page row, for up to the whole pass; and two first visits of one page would block on each other's uncommitted `INSERT … ON CONFLICT DO NOTHING` (Postgres waits for the inserting transaction; `SKIP LOCKED` never gets a chance) and report false failures. This replaces the round-1 ruling on `idle_in_transaction_session_timeout = '10min'`, which no longer applies: no builder transaction idles. One claim can run several computes (genesis retries after `BaselineRejected`, a pass and then word counts, the claim, `ensure_legacy_numbered` and the reads before them), so the lease is renewed before every compute (`renew_lease`, Global Constraints step 4): each compute's timeout is at most `lease − 30 s`, so the lease never lapses while this builder computes, and no second replica claims and replays the page meanwhile — cost if wrong: one extra short transaction per pass and per word count; a builder whose lease was taken anyway (an operator, a clock jump) fails its write ("lease lost") and is retried (Task 7's `a_pass_that_lost_its_lease_writes_nothing` pins the write's check, for the row write, the words phase and the release after it; `a_build_that_outlasts_one_lease_renews_it` pins the renewal).
- Ruling: the claim commits a crash marker before any compute: `failures = failures + 1`, `retry_after = now() + failure_backoff(failures)`, `last_error = 'pass interrupted'`; only the pass's release resets them — a page that kills the process (stack overflow, abort, OOM) rolls back nothing that was committed, so it stays deferred instead of being handed by `dirty_candidates` (oldest `last_write_at` first) to every replica in turn, each of which would die too, taking the web service with it — cost if wrong: none; a healthy pass resets the marker in its last write.
- Ruling: a graceful shutdown is told apart from a crash, so crashes give a page up after `MAX_CRASHES` (3) instead of `MAX_FAILURES` (10): `Builder::spawn` takes `AppState::shutdown` (`crates/knot-server/src/lib.rs:93`); on cancel the loops stop claiming, abort their `JoinSet`s, and run one statement for the pages that were in flight (`UPDATE doc_history_state SET claimed_by = NULL, claimed_until = NULL, failures = greatest(failures - 1, 0), retry_after = NULL, last_error = 'shutdown' WHERE doc_id = ANY($1) AND claimed_by = $me`), within the chart's 50 s grace period; a claim that reads `last_error = 'pass interrupted'` therefore counts a crash (`crashes`), and at `MAX_CRASHES` gives the page up (reason `crash`) — each attempt on a page that aborts the process (stack overflow, a double panic in `TransactionMut::drop`, OOM) kills a pod's web server and its rooms — cost if wrong: an innocent page whose pass was running when its pod was OOM-killed for another reason three times in a row (no release between) is given up until an operator re-enables it; a pod killed before its release statement ran (SIGKILL after the grace period) leaves `'pass interrupted'` and counts once.
- Ruling: a page whose claim finds `failures >= MAX_FAILURES`, or whose smallest replay input exceeds `max_input_bytes` (= `history_replay_max_bytes`; summed with `octet_length` in SQL before any bytes are fetched: the starting checkpoint or the baseline genesis starts from (re-checked for each older baseline genesis falls back to), the replay prefix, and the first new row; snapshots are not part of it), is given up: `retry_after = 'infinity'`, `builder_version = CASE WHEN genesis_rev IS NULL THEN BUILDER_VERSION ELSE builder_version END` (a page an older builder built keeps that version, so a version bump still rebuilds it), `last_error` names the reason, `claimed_*` cleared, an ERROR log with the doc id, `knot_history_pages_given_up_total{reason}` (`failures` | `too_large`, `timeout` from the frozen-stage and second-timeout rulings, `crash`); its built entries stay listed, and the list reports `unavailable: true`, `catching_up: false`; a page given up while `genesis_rev IS NULL` has no built entries yet, so `give_up` then also inserts its metadata-only baseline entries (one per `doc_history_baselines` row of the page that no entry names yet: `kind 'baseline'`, `author_id` NULL, `started_at = ended_at = created_at`, `baseline_id`, revs NULL, sealed, meta `{}`; exactly what `build_genesis` writes for them, and viewing them needs no replay), and `write_genesis` deletes the page's `kind = 'baseline' AND baseline_id IS NOT NULL` entries before inserting its own, so a lifted give-up never duplicates them — the design lists "Earlier versions (author unknown)" for every legacy page, and the pages given up at genesis (large, long-lived) need that list most; given-up pages are left out of `knot_history_builder_lag_revs` and counted by the `knot_history_given_up_pages` gauge; an operator re-enables one with `UPDATE doc_history_state SET failures = 0, timeouts = 0, crashes = 0, retry_after = NULL WHERE doc_id = …` (all three counts: a page given up for crashes would otherwise be given up again by its next claim) (Helm README); a newer `BUILDER_VERSION` lifts the give-up in the claim (Global Constraints) and sweep (b) finds such pages; the claim's version lift runs before its `MAX_FAILURES` check and also covers a page an older builder built whose failures reached the limit without a give-up (the old builder's tenth failure, or an operator who reset only `retry_after`): giving such a page up under the new version would stamp the new version on the old builder's entries, so no rebuild would ever run, and it would erase a `Rebuild` request (`builder_version = 0`) whose failure was the tenth; the lift is limited to `given_up OR genesis_rev IS NOT NULL` because `builder_version` is 0 on every page before its first genesis, which must still be given up after `MAX_FAILURES` — retrying forever burns a worker, and an unbounded replay is what OOM-kills the process — cost if wrong: a page over the replay cap, or one that failed ten times, gets no further history until an operator acts or a fixed builder ships.
- Ruling: one cap, `history_replay_max_bytes`, bounds every replay's input, builds included (the round-2 fixed 64 MiB build cap is gone): at the doc-memory factor F that Task 5 measures (peak heap of a whole pass with a late-snapshot probe, a union fallback and a checkpoint emit, and of a word count, over the replay input bytes, worst case over interleaved one-character typing and pastes; not assumed), `workers × F × cap` must fit the chart's `resources.limits.memory: 512Mi` (`values.yaml:142`) next to a `WEB_ALLOWANCE` of 256 MiB for the web server and its rooms, which Task 5's rule gives for up to 2 workers (the default is 1); snapshots count toward it, and a pass probes snapshots oldest first while the input stays under the cap (at most `PASS_MAX_SNAPSHOTS`), counting the rows each snapshot needs (those up to its rev), loading rows only up to the snapshot it left out, and loading no rows in a pass whose late snapshots leave less than the first new row's size (those snapshots are its progress; Task 7 step 6). The cap is a hard bound on what a pass loads: `rows_in_range` stops before the row that would cross its byte limit (it returns a lone first row over the limit, which the give-up check already admitted), so the input never exceeds the cap — the round-4 "cut after the crossing row" let one large paste push a replay to about twice the cap, and a snapshot taken far ahead of `built_rev` left one row of budget, re-reading the cap on every pass while advancing one row. A snapshot that does not fit even alone (`base` + the rows it needs + its size over the cap, with at most one row left to postpone) is left unchecked (`snapshots_unchecked{reason="too_large"}`, `snapshot_checked_rev` moved past it when the pass reaches its rev), never a reason to give the page up: a probe is optional work, and the page's state, its replay prefix and a full snapshot of about the same size would otherwise give up every page above a third of the cap on its first pass after a room snapshot — cost if wrong: legacy pages over 32 MiB of genesis input are given up until an operator raises `replayMaxBytes` (and the memory limit); snapshots of pages above about half the cap are never gap-checked, which the counter shows.
- Ruling (CPU): `history.workers` defaults to 1, not 2 — replay threads are plain OS threads outside tokio, each up to a whole core, and the chart's `resources.limits.cpu: 1000m` (`values.yaml:141`) is shared with the web server, the room actors and the flush scheduling of saves, all on tokio; one CPU-bound worker running replays back to back is already one full core, the whole 1000m quota, and would throttle the tokio runtime (edit latency) for the whole post-upgrade backlog. So every worker is paced: after its last transaction (the release, a failure record or a give-up), `build_page` sleeps `pacing_delay(compute_time, history_cpu_share) = compute_time × (1 / share − 1)` on its own task, `compute_time` being the summed wall time of the call's `run_isolated` computes that returned, while still holding its permit (a released permit would let the next queued pass compute at once) and holding no lease (the page is released, so a sleep never outlasts a lease); each worker then uses at most `share` of a core on average, and the default `0.5` gives the builder half the default CPU limit. A timed-out compute's permit stays with its thread, which paces nothing. M1.4's views share the permits and wait out the pacing with the builder. Timed-out threads that keep spinning show in `knot_history_replay_threads_stalled`, which the README tells operators to alert on and answer with a pod restart. The liveness probe is not tied to it (`/api/healthz` stays "process is running": a probe that restarts pods on builder state would also restart them mid-backlog), and replay threads are not reniced (per-thread `setpriority` needs `libc` and is Linux-only) — cost if wrong: the first build takes about four times as long on a single replica as one unpaced worker on two would; operators with CPU headroom raise `history.cpuShare` or `history.workers`.
- Ruling: `KNOT_HISTORY_WORKERS=0` turns the builder and the size sampler off on that replica (an incident switch: a replay that kills the process takes the web service with it) — the list then reports `catching_up` while pages stay dirty; repeated crashes on one page are handled by `crashes` (the graceful-shutdown ruling) — cost if wrong: an operator sets the value to 0 while investigating.
- Ruling: `ensure_legacy_numbered` runs in its own transaction after the claim and before the read — inside a pass transaction its heads-row lock (`updates_store.rs:685-751`) would be held for the whole pass and stall every save of the page; after the claim, its failures are recorded on the claimed state row — cost if wrong: none (it is idempotent and one index probe once done).
- Ruling: a genesis pass commits on its own and is exempt from `PASS_MAX_ROWS`, `PASS_MAX_ROW_BYTES` and `PASS_ROWS_BUDGET`, but not from `max_input_bytes` nor `REPLAY_TIMEOUT`; it skips `skip_revs`, publishes `Progress`, and on `Corrupt` writes nothing (`GenesisError::Corrupt { rev }`), exactly like a pass; it loads the bytes of one baseline at a time (newest first; the rest by `id`, `snapshot_seq`, `created_at` only) and tries the next older one, on a fresh `Replayer`, only when `build_genesis` reports the one it got did not apply; normal passes stop at whichever limit comes first — genesis cannot be split without storing a partial replay — cost if wrong: the first visit of a large legacy page holds one worker for as long as that replay takes.
- Ruling: the grouper seals idle sessions only in a pass that processed every row up to the page's head, with `SEAL_MARGIN` — a row by the same author may still be committing (an append attempt can take up to `APPEND_STATEMENT_TIMEOUT`, 8 s) — cost if wrong: a session continued within the margin after 20 idle minutes stays one session.
- Ruling: word counts are computed for sealed `session` entries only, in the pass's words phase after its row writes, from the nearest checkpoint at or before `start_rev − 1` on a skip-GC replay (`pass::replay_window`, gap checkpoints merged with `merge_gap_state` at their revs); counts the words budget (`PASS_WORDS_BUDGET`, separate from the rows budget, so a pass whose rows used up their time still counts) did not reach stay `words_pending` and keep the page dirty, but a pass that reached the head (`rows_exhausted`, no rev gap, not stopped early) always runs at least one count whatever the words budget says; `run_pass` checks its rows deadline only when another row follows, so a pass that consumed its last row is never "stopped early" and still seals idle sessions and forces its count; and `run_pass` decodes no checkpoint when it has no rows and no snapshots, so a page with nothing but pending counts makes progress on every pass instead of spending its budget on a replay that changes nothing. Each count is attempted once: a short lease-checked transaction clears that entry's `words_pending` before its compute, so an error, a panic or a crash during it leaves both counts NULL instead of wedging the page; a replay input over `history_replay_max_bytes` is skipped the same way. In the words replay, `Refused`, `ApplyError` and `Undecodable` rows are passed over as in the pass (not counted again: the pass counted them), and `Corrupt` aborts that count (`corrupt`: the skip-GC doc can differ in block structure from the GC'd pass), as does a gap checkpoint that neither `merge_state` nor `union` takes (`gap_state`). Every skip counts `knot_history_words_skipped_total{reason}` (`too_large` | `diff_error` | `corrupt` | `gap_state` | `panic` | `timeout`); none fails the pass — the timeline shows counts only for sessions (design, "Timeline entries"), and counting needs the union-doc diff; a word-count error inside the row transaction would roll back the rows and fail identically on every retry — cost if wrong: some sessions show no counts.
- Ruling: M1.3 builds the diff core (`diff::entry_diff`, `diff::text_chunks`) that the architecture assigns to M1.4's view — "words are counted in exactly the diff the view renders" (entry_builder, SEALING AND WORD COUNTS); M1.4 adds `pm_json` on top of these two functions — cost if wrong: none.
- Ruling (spike fallback): if Task 3's spike shows `encode_state_from_snapshot` failing or misclassifying on a skip-GC doc seeded from a GC'd checkpoint, the union doc is dropped and the signatures change, because the texts inserted during the window can only be collected while the window replays (texts inserted and then deleted are not reachable from `default` afterwards): `pub struct WindowTexts` (the `XmlTextRef`s reachable from `default` when it is installed, plus every `XmlText` an `observe_deep` subscription on `default` sees inserted while it lives), `Replayer::collect_texts(&mut self) -> WindowTexts` (installed after replaying to `start_rev − 1`, before applying `start_rev..=end_rev`), `entry_diff_with(h: &yrs::Doc, texts: &WindowTexts, inserted, deleted)` and `entry_word_counts_with(h, texts, inserted, deleted)`, running `diff_range(S, S_lo)` on each collected text of the replay doc itself; Task 7 step 9 then installs the collector before replaying the window; Task 3 records the failing case in the architecture deviation notes for M1.4 — cost if wrong: M1.4 renders removed blocks from the same text list instead of a union doc.
- Ruling: `AuthorView` has no `color` — the web client already colours avatars from the user id (`Avatar seed={user_id}`, `DocByline.tsx:110`); `former: true` tells it to use grey — cost if wrong: M4 adds a field.
- Ruling: whether a person is anonymized comes from one SQL expression, `people::ANONYMIZED_SQL = "false"`, used by every history read (the list API and the contributors API); M4 changes it to `u.history_anonymized_at IS NOT NULL` — the column does not exist before M4 — cost if wrong: none.
- Ruling: `GET /api/docs/{id}/contributors` keeps its JSON and adds `former: false` to `created_by` and to each contributor; for a former member (M4) the id and name fields become `null` — the byline (`DocByline.tsx`) ignores unknown fields — cost if wrong: M4 updates the web validators.
- Ruling: the list works on archived pages for Editors and Owners (the middleware allows GET; viewers get 404) — read-only, like today's snapshot list — cost if wrong: none (the UI does not offer the drawer there). Pinned in Task 9 (`editors_list_entries_and_viewers_are_refused`, archived case).
- Ruling: a pass on a page whose `builder_version` is above `BUILDER_VERSION` does nothing; below it, the pass deletes the page's entries except `kind = 'erase'` and its `genesis` and `periodic` checkpoints (never its `gap` checkpoints: next ruling), resets the state row (`built_rev`, `genesis_rev` NULL, `snapshot_checked_rev`, `parked`; **not** `skip_revs`: a row that panicked or corrupted yrs still does) and sets `doc_history_heads.dirty = true` (after the state row, in lock order), then rebuilds from genesis; while `genesis_rev` is NULL the list reports `catching_up` — a mixed-version rollout must not let the older builder overwrite the newer one's entries — cost if wrong: a rebuild pass is long on large pages; a builder version that fixes a skip cause clears `skip_revs` in its own plan.
- Ruling: gap states are source data, not derived data: a `gap` checkpoint is the only lasting copy of content no row carries (and of refused rows' content, `skip_revs`), while the snapshot it was probed from is a boot cache that thinning removes (today after about 30 days; after M1.4 beyond the 3 newest). The architecture's ATOMICITY AND REBUILD ("That works because the log is complete") does not hold for that content. So no rebuild deletes a gap checkpoint; after `reset_for_rebuild` every checkpoint above `built_rev` is a *kept gap* (no other checkpoint ever lies above `built_rev`: periodic and gap checkpoints are emitted at revs the pass built, and a write stores them with its `built_rev`). A pass loads the kept gaps up to its last row (`PassInput::kept_gaps`, counted toward the input cap like rows, Task 7 step 6) and, right after row R (applied, skipped or passed over, before the snapshot probes at R and the deadline check), merges the kept gap at R with `merge_gap_state` exactly like a gap found by a probe: its effect credits parked claims, newly parks a gap claim, becomes a `gap` entry at R (`started_at = ended_at` = the kept checkpoint's `created_at`, the time it was found) when not empty, and the state is emitted as the `gap` checkpoint at R, also when the effect is empty, which replaces the old one (an old builder's state must not become a start checkpoint). Snapshots still present are probed as usual and find only what the replay still lacks, so the content is reported once: at the kept gap's rev, or at an older snapshot's rev that holds it. A kept gap that `merge_gap_state` does not take (neither `Applied` nor `ApplyError`), that corrupts the replay, or whose merge panics or times out (`Stage::KeptGap(rev)`) fails the pass with `ReplayError::KeptGap { rev }` → `DropKeptGap(rev)`: that row is deleted (`kind = 'gap' AND rev > built_rev`), logged at ERROR and counted `snapshots_unchecked_total{reason="kept_gap"}`; it is the one case where unlogged content leaves history — cost if wrong: each kept gap costs one merge per rebuild; without the rule a rebuild (the `Rebuild` action, any `BUILDER_VERSION` bump) silently drops every gap entry whose snapshot was thinned, from the timeline and from every replay after it, and re-reports content still on the page as a new authorless gap at the newest snapshot.
- Ruling: a failed pass (store error, `LegacyCountMismatch`, panic, timeout, `ReplayError`, `GenesisError::Corrupt`, lease lost, a rev gap) keeps the claim's crash marker (`failures`, `retry_after`), and its failure transaction (`SET LOCAL lock_timeout = '1s'`; page `FOR KEY SHARE`; `UPDATE doc_history_state SET last_error = $2, claimed_by = NULL, claimed_until = NULL[, skip_revs = array_append(skip_revs, $rev)] WHERE doc_id = $1 AND claimed_by = $me`) only adds the reason; it counts `knot_history_build_failures_total`, logs at warn with the doc id (ERROR for a rev gap), and leaves the page dirty; the claim query skips pages until `retry_after` — retrying a deterministic failure every 2 s would burn a worker; SQLSTATE `55P03` on a claim statement is `Busy`, never a failure — cost if wrong: a transient failure delays that page by up to an hour.
- Ruling: a rev gap (`stats.rev_gap`) is a failed pass: the write transaction stores what was built up to the gap without releasing (and no words phase runs), then the failure path records `last_error = 'rev gap after <rev>'` — rev order is commit order, so a gap is a bug (or M5's compaction, which must then renumber), and without a failure the page would be re-claimed every 2 s with no progress and no log — cost if wrong: none.
- Ruling: `run_pass` decodes every checkpoint it emits once more (`Replayer::from_state` and a state-vector comparison) before returning it, so a checkpoint the builder writes always decodes; a later rejection is storage damage. A rejected `periodic` checkpoint (`ReplayError::State`, or a panic or timeout at `Stage::Checkpoint`) is deleted by the failure transaction (`DELETE … WHERE rev = $cp AND kind = 'periodic'`), and the next pass resumes from an older one: entries, claims and open sessions as of `built_rev` are consistent with every checkpoint at or below it, because the replay prefix is replayed. A rejected `genesis` or `gap` starting checkpoint sets `builder_version = 0`, so the page is rebuilt (`Rebuild { drop_gap }`); a rejected `gap` start is also deleted, that one row only (`drop_gap = Some(rev)`), since it cannot be merged and the rebuild keeps every other gap checkpoint (gap-states ruling); its content comes back only through its snapshot, if that still exists, which the rebuild probes again (`snapshot_checked_rev` restarts at `G`). A pass never merges a gap checkpoint at or below `built_rev` other than its start (the start is the latest checkpoint at or below `built_rev`); word counts that meet a rejected gap checkpoint skip that count, `gap_state` — resuming past a gap checkpoint without its state would drop that content from replays and report the same snapshot content again as a second gap entry — cost if wrong: one rebuild of the page; a rejected gap checkpoint whose snapshot was thinned loses its content from history (logged at ERROR with the rev).
- Ruling: rows after genesis with `by_user_id` NULL form sessions with `author_id` NULL (listed with `author: null`, "Unknown author" in M1.4), never credited to anyone — the architecture only says such rows "are never credited"; production writers always stamp an author (`by_user: None` appears only in tests) — cost if wrong: none.
- Ruling: a gap found while probing at rev R (the snapshot's rev while the pass replays it; `built_rev` for a late snapshot, next ruling) becomes a `gap` checkpoint at R (replacing a periodic one at the same rev; `run_pass` emits at most one checkpoint per rev, the last state at that rev, so several late snapshots probed at `built_rev` give one gap checkpoint) and a sealed `gap` entry with `start_rev = end_rev = R`, `started_at = ended_at = snapshot.created_at`, `author_id` NULL; the union state replaces the replay doc — the architecture says only "the union state" — cost if wrong: the gap's content shows at the probe's rev, not at the unknown rev it entered the page.
- Ruling: a stamped snapshot whose `snapshot_rev` is at or below `built_rev` when a pass starts (rooms write the idle snapshot 30 s after the last apply, `snapshot_idle_sec`, `room.rs:535-537`, the exit snapshot later; the builder ticks every 2 s) is probed against the replay at `built_rev` right after the replay prefix, oldest first — the state at `built_rev` holds everything `replay(snapshot_rev)` holds, so anything the probe reports is content no row up to `built_rev` carries; the sweep also selects pages with a stamped snapshot above `snapshot_checked_rev`, because a snapshot write never sets `doc_history_heads.dirty` — without this, gap detection only worked while the builder lagged — cost if wrong: one probe per late snapshot; the sweep query scans `doc_history_state` once a minute.
- Ruling: a checkpoint is written when the row bytes since the last one reach `threshold(size) = max(CHECKPOINT_FLOOR_BYTES, size × CHECKPOINT_STATE_FRACTION)` for the state size at the last encode, re-checked against the fresh size after encoding; the same threshold decides when to encode, so a crossing costs at most one `encode_state` — the round-1 fallback halved only the encode trigger, which left the spacing unchanged and re-encoded the state on every row — cost if wrong: checkpoint spacing drifts while the state shrinks.
- Ruling: there is no `knot_history_view_seconds` in M1.3 — it measures M1.4's view endpoint; the list endpoint is already measured by `knot_http_request_duration_seconds{route}` — cost if wrong: none.
- Ruling: `knot_history_table_bytes{table}` is sampled every 10 minutes by whichever replica wins `pg_try_advisory_lock(SIZE_LOCK_KEY)` on a detached connection (the GC's pattern, `crates/knot-crdt/src/gc.rs:43-79`); `knot_history_builder_lag_revs` is the largest `head_rev − built_rev` over dirty pages that are not given up, and `knot_history_given_up_pages` the number of given-up pages, both sampled by every replica's sweep — dashboards read them with `max` — cost if wrong: a replica that never wins the lock reports no table sizes.
- Ruling: a snapshot's failure never fails a row: a probe runs on a clone, so a probe that is `Corrupt` or `Undecodable` (or a `union` that is `Corrupt`) leaves the snapshot unchecked and the pass goes on; only a `merge_state` that corrupts the replay itself, or a panic or timeout at `Stage::Probe`, fails the pass, and its failure transaction moves `snapshot_checked_rev` past that snapshot (`greatest(snapshot_checked_rev, $snapshot_rev)`) instead of touching `skip_revs`; each counts `knot_history_snapshots_unchecked_total{reason}` — cost if wrong: content only that snapshot held is reported at the next snapshot that holds it.
- Ruling: a session's `started_at` is the minimum and `ended_at` the maximum of the times it has seen; a contribution whose time is before the session's `ended_at` (late gap credits carry the snapshot's `created_at`, the rows after it can be older) joins the session — `ended_at` decides the idle seal and the day an entry is listed under — cost if wrong: none.
- Ruling: `tick` and `sweep` run as two loops in one task (`tokio::join!`), each with its own `JoinSet` that it refills instead of awaiting: every `TICK` (or `SWEEP`) the loop reaps finished tasks (`try_join_next`) and tops its set up to `CLAIM_LIMIT` with candidates that are not in flight on this replica (the in-flight ids are passed to the candidate query as `$2 uuid[]`), so up to `CLAIM_LIMIT` passes wait for one of `workers` permits, and one long genesis never holds back newly dirty pages, the sweep or the lag gauge; `Builder::tick()` and `Builder::sweep()` (one selection round, awaited) stay for tests; they take `self: &Arc<Self>`, because each candidate's `build_page` runs as a spawned `'static` task holding an `Arc<Builder>` clone — cost if wrong: none. Queue order stays oldest `last_write_at` first, so a page edited continuously during the post-upgrade backlog waits until the backlog's older pages are built (its readers see `catching_up`).
- Ruling: `dirty_candidates` debounces: a page written less than `debounce` (`DEBOUNCE`, 10 s) ago is not a candidate unless `head_rev − built_rev ≥ DEBOUNCE_MAX_LAG_REVS` (200, below `CATCHING_UP_REVS`, so the debounce never makes a page read as catching up); the sweep is not debounced — every pass decodes the page's latest checkpoint and re-applies its prefix (up to about the state size again) before its few new rows, so without it a large page under continuous typing is rebuilt on every 2 s tick, about a quarter of a core per such page at Task 5's 500 ms replay bound, and a handful of them fill the default single worker; with it, such a page is built once per 200 rows while typing goes on, and 10 s after it stops — cost if wrong: a page's entries trail its edits by about 10 s, or by up to 200 rows while it is edited without pause. Tests that build through `tick()` or read `dirty_candidates()` set `limits.debounce` to zero unless they test the debounce.
- Ruling: a claim that finds nothing to do returns `Clean` without taking the lease (Global Constraints, clean check) — every replica selects the same 20 oldest dirty pages and waits for a permit before claiming, so a queued task often claims a page another replica built and released meanwhile; without the check it would load the checkpoint, prefix and snapshots, replay them and write a no-op, and the post-upgrade backlog's database reads and CPU would scale with the replica count — cost if wrong: one extra small read per claim.
- Ruling: `build_page`'s per-replica in-flight membership is an RAII guard whose `Drop` removes the id, so a dropped `build_page` future (shutdown, an aborted task) never leaves the page marked in flight on that replica — the "removed on every return" of round 3 did not hold for cancellation — cost if wrong: none.
- Ruling: the list's ACL check fails closed: no ACL cache (`AppState::acl` is `None`) or a `ResolveError` hides the source (`*_hidden: true`) and logs a warning, never a 500 for the whole list — a history entry must not leak a restricted title because the cache was unavailable — cost if wrong: a source that is readable shows as hidden during an ACL outage. The decision lives in one function, `readable_sources` (Task 9), unit-tested on its `None` and `Err` arms: an HTTP test cannot reach the handler with `acl: None`, because `require_doc_role_mw` answers 500 before any handler runs (`auth/require_doc_role.rs:58-60`).
- Ruling: the real-instance measurement spike (architecture open_spikes, last item) does not block M1.3 — no executor has a real instance; Task 5's synthetic throughput spike sets the defaults, and the Helm README asks operators to run `history-stats` and report — cost if wrong: defaults tuned on synthetic data.
- Ruling: `replay_window` merges a gap state at rev R after row R when R is below the window's `end_rev`, and a gap state at `end_rev` itself only when `WindowInput::merge_gap_at_end` is true — a gap entry found at R is written after the rows at R, so its id is higher and the list (`end_rev DESC, id DESC`) shows it above every entry ending at R; the page after such an entry excludes the gap content, the gap entry's own view includes it. One exception: a session that holds credits from the gap merge at its own `end_rev` (a parked row drained by that merge, fed as `ContributionKind::GapCredit`, Task 2) has those ids in its `inserted` set, and they enter the replay only through that merge; the grouper records it as the session's internal `meta.gap_credit_rev` (the rev of the latest such credit; never rendered, Task 9's allowlist), and every replay of the session (word counts, M1.4's view) passes `merge_gap_at_end = (meta.gap_credit_rev == end_rev)` — the rest of the gap's content is not in the session's sets and diffs as `Plain`. So word counts pass `true` exactly for such sessions, `false` otherwise; M1.4 passes `true` for those sessions and when viewing the gap entry itself — cost if wrong: none; without the exception a session ending at that rev would count none of the drained words (a credit-only session 0/0), and "Show changes" would not show them.
- Ruling: the Markdown import route tags its rows `meta: {"mode": "append" | "replace"}` (an enum, which M1.2's meta rule allows), and the list renders `import` meta as `{"mode"}` (null when absent: workspace import and 0.9.10 rows) — both modes write `source: Import` (`routes/api/markdown.rs:535`, `:555`), and the design's "`<Name>` replaced the content (import)" would otherwise mislabel every append; M1.4 words `mode: null` neutrally ("imported content") — cost if wrong: one small meta object per import row.
- Ruling: in the gap check, a `merge_state` that is `Refused` after a non-empty `Applied` probe falls back to `union` exactly like a refused probe (the probe and the merge share pre-validation, so this is not expected; if it happens, the content still surfaces as a gap entry), and a `union` that does not apply leaves the snapshot unchecked (`refused`) with no gap checkpoint emitted — cost if wrong: none.
- Ruling: the "state at rollout" entry is `kind = 'baseline'` with `baseline_id` NULL and `meta.rollout = true` — the list's section split (`kind = 'baseline'` is the "Earlier versions" section) and `render_page`'s `baseline → {"rollout"}` depend on it, and the CHECK would allow any kind, since it has an `end_rev` — cost if wrong: none.
- Ruling: a word count that times out is skipped, not a pass failure: it counts `knot_history_replay_timeouts_total{phase="words"}` and `knot_history_words_skipped_total{reason="timeout"}`, ends the words phase (the permit stays with the thread) and goes on to the release (`Built`); only pass and genesis computes that time out fail the pass — the count was already marked attempted (`take_words_entry`), so a failure would add nothing but a backoff — cost if wrong: none.

---

### Task 1: The `knot-history` crate and the replay engine

**Files:**
- Modify: `Cargo.toml` (workspace `members`, after `"crates/knot-export"`), `Cargo.lock`
- Create: `crates/knot-history/Cargo.toml`, `crates/knot-history/src/lib.rs`, `crates/knot-history/src/replay.rs`, `crates/knot-history/src/isolate.rs`
- Test: `crates/knot-history/tests/replay.rs` (`mod common;`), `crates/knot-history/tests/common/mod.rs` (`#![allow(dead_code)]`, since `make lint` runs clippy `--all-targets -D warnings` over every test binary that includes it, and `pub mod underflow;`; Task 4 adds the generator, Task 7 `pub mod metrics;`), `crates/knot-history/tests/common/underflow.rs` (the reproducer)

**Interfaces:**
- Produces:
  - `pub enum ReplayMode { Gc, KeepDeleted }` (`Gc` = `Doc::new()`; `KeepDeleted` = `Doc::with_options(Options { skip_gc: true, ..Default::default() })`).
  - `pub struct Replayer` with `pub fn new(mode: ReplayMode) -> Replayer`, `pub fn from_state(mode: ReplayMode, state: &[u8]) -> Result<Replayer, ReplayError>`, `pub fn apply(&mut self, update: &[u8]) -> RowEffect`, `pub fn merge_state(&mut self, state: &[u8]) -> RowEffect` (same as `apply`; used for gap states), `pub fn probe(&self, state: &[u8]) -> RowEffect` (applies `state` to a clone made from `encode_state()`, leaves `self` untouched; used by the gap check), `pub fn union(&mut self, state: &[u8]) -> RowEffect` (the gap check's fallback when `probe` is `Refused`, Rulings: decodes `state` into a fresh doc of the same mode, applies `self.encode_state()` to it with the same pre-validation and post-check as `apply`; on `Applied` or `ApplyError` the fresh doc replaces the replay (its past-gap set recomputed as `from_state` does) and the effect is the set difference, `inserted` = held insertions after − before, `deleted` = delete set after − before, `parked_*` = pending after − before; any other outcome leaves `self` untouched), `pub fn merge_gap_state(&mut self, state: &[u8]) -> RowEffect` (`merge_state`, and when that is `Refused`, `union`; what a replay that passes a gap checkpoint's rev calls, Rulings), `pub fn encode_state(&self) -> Vec<u8>` (`encode_state_as_update_v1(&StateVector::default())`, which includes pending content), `pub fn state_vector(&self) -> Vec<u8>` (v1), `pub fn doc(&self) -> &yrs::Doc`, `pub fn has_pending(&self) -> bool`.
  - `#[derive(Debug, Clone, PartialEq)] pub struct RowEffect { pub outcome: RowOutcome, pub inserted: IdSet, pub deleted: IdSet, pub parked_inserts: IdSet, pub parked_deletes: IdSet }` and `impl RowEffect { pub fn is_empty(&self) -> bool }` (inserted and deleted both empty).
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum RowOutcome { Applied, ApplyError(String), Undecodable(String), Refused, Corrupt }` with `pub fn metric_reason(&self) -> Option<&'static str>` (`None`, `"apply_error"`, `"undecodable"`, `"skip_underflow"`, `None` for `Corrupt`, which `run_pass` turns into an error). After a `Corrupt` from `apply` or `merge_state` the `Replayer` must not be used again (`probe` and `union` never corrupt `self`).
  - `#[derive(Debug, thiserror::Error)] pub enum ReplayError { State(String), Prefix { rev: i64 }, Corrupt { rev: i64 }, CorruptSnapshot { snapshot_rev: i64 }, KeptGap { rev: i64 }, Cancelled }` (`KeptGap`: a gap state kept across a rebuild that `merge_gap_state` does not take or that corrupts the replay, Rulings; `Cancelled`: the builder cancelled the timed-out compute through `Progress` (Task 4), its result is discarded; `State`: the starting checkpoint; `Prefix`: a row at or below `built_rev` that the replay prefix re-applies is `Corrupt` or `Refused` (Rulings: blames the checkpoint, never the row); `Corrupt`: a new row; `CorruptSnapshot`: a gap merge).
  - `isolate.rs`: `#[derive(Debug)] pub enum IsolateError { Panicked(String), TimedOut }`; `pub async fn run_isolated<T: Send + 'static, G: Send + 'static>(guard: G, timeout: std::time::Duration, f: impl FnOnce() -> T + Send + 'static) -> (Result<T, IsolateError>, Option<G>)` — a `std::thread::Builder::new().name("knot-history-replay").stack_size(REPLAY_STACK)` thread that owns `guard`, runs `catch_unwind(AssertUnwindSafe(f))` and sends `(result, guard)` through a `tokio::sync::oneshot`; the await is wrapped in `tokio::time::timeout`; on a timeout the guard stays with the thread and is dropped when it ends (`None`). The builder passes its worker permit as the guard (Rulings).
  - `lib.rs` re-exports `Replayer, ReplayMode, RowEffect, RowOutcome, ReplayError, run_isolated, IsolateError`.

- [ ] **Step 1: Write the failing tests** in `crates/knot-history/tests/replay.rs`

Build histories with plain yrs docs (`Doc::with_client_id(n)`, a `default` `XmlFragment` holding `paragraph` elements with `XmlText`). Include knot-crdt's yrs-only fixtures by path rather than copying them: `#[path = "../../knot-crdt/tests/common/fixtures.rs"] mod fixtures;` (`para`, `text_page`, `parked_block_and_the_edit_it_fails_on`, `Rng`, `random_edit`).

```rust
#[test]
fn apply_reports_what_the_row_changed() {
    // client 1 types "hello" (u1), then deletes "h" (u2)
    let mut r = Replayer::new(ReplayMode::Gc);
    let e1 = r.apply(&u1);
    assert_eq!(e1.outcome, RowOutcome::Applied);
    assert!(!e1.inserted.is_empty() && e1.deleted.is_empty());
    let e2 = r.apply(&u2);
    assert!(e2.inserted.is_empty() && !e2.deleted.is_empty());
    assert!(r.apply(&u1).is_empty(), "a resend integrates nothing");
}

#[test]
fn parked_content_is_reported_then_credited_on_drain() {
    // one client types "ab" (u_a), then "c" right after "b" in the same text (u_b), so u_b's left
    // origin is u_a's item: yrs 0.27.3 integrates a block whose own origins it holds past a gap
    // without parking, so u_b must depend on u_a to park. Apply u_b first.
    let mut r = Replayer::new(ReplayMode::Gc);
    let parked = r.apply(&u_b);
    assert!(parked.is_empty() && !parked.parked_inserts.is_empty());
    assert!(r.has_pending());
    let drained = r.apply(&u_a);
    assert!(drained.inserted.contains(&first_id_of(&u_b)), "u_b's blocks integrate in u_a's transaction");
    assert!(!r.has_pending());
}

#[test]
fn a_delete_of_unknown_items_parks_deletes_only() {
    // delete set over items the doc lacks (M1.2 Task 1's delete-before-insert step)
    let e = r.apply(&delete_only);
    assert!(e.parked_inserts.is_empty() && !e.parked_deletes.is_empty());
}

#[test]
fn checkpoint_round_trip_keeps_pending_content() {
    // r holds pending structs and a pending delete set
    let again = Replayer::from_state(ReplayMode::Gc, &r.encode_state()).unwrap();
    assert_eq!(pending_bytes(&again), pending_bytes(&r)); // pending_update().update.encode_v1(), pending_ds().encode_v1()
    assert_eq!(again.state_vector(), r.state_vector());
}

#[test]
fn a_checkpoint_with_pending_nested_content_resumes_like_one_that_never_round_tripped() {
    // client 1 inserts paragraph P (an XmlElement in `default`) holding an XmlText "ab" (u1), then a
    // nested element inside P's text's sibling list (u2); client 2 types into P's text right after "b"
    // and inside the nested element (u3, u4: their parents are nested types, named by id, not by root
    // name). Replay r: apply u1, then u3 and u4 park (u2 withheld; u4 depends on u2).
    // Resume: again = Replayer::from_state(Gc, &r.encode_state()).
    // Both r and again then apply u2, which drains u4.
    assert_eq!(again.state_vector(), r.state_vector());
    assert_eq!(delete_set_of(&again), delete_set_of(&r));
    assert_eq!(xml_string(&again), xml_string(&r)); // `default`.get_string(): canonical.rs comes in Task 4
    assert!(!again.has_pending() && !r.has_pending());
}

#[test]
fn a_resend_over_blocks_held_past_a_gap_is_trimmed_and_applied() {
    // client 1: three transactions t1, t2, t3 on separate root-level paragraphs, t3 inserted at index 0,
    // so t3 does not depend on t2. Apply t1 and t3 (t3 integrates past a gap: the replay holds blocks
    // past client 1's state-vector clock, nothing pending), then a resend of t2 ∪ t3
    // (encode_state_as_update_v1 against the state vector after t1). yrs trims t3 as known (Rulings).
    let e = r.apply(&resend);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(&t2));
    assert_eq!(visible_text(&r), visible_text(&doc_with_t1_t2_t3));
    assert!(!r.has_pending());
}

#[test]
fn a_full_state_probe_over_a_gapped_replay_reports_the_missing_range() {
    // replay holds t1 + t3 (as above); probe the full state of t1 ∪ t2 ∪ t3
    let e = r.probe(&full_state_t1_t2_t3);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(&t2));
}

#[test]
fn the_skip_underflow_is_refused_not_applied() {
    // (doc_state, update, room_state) = skip_underflow_case(), the minimised reproducer from Step 1 (M1.2's seed 5)
    let mut r = Replayer::from_state(ReplayMode::Gc, &doc_state).unwrap();
    let before = r.encode_state();
    let e = r.apply(&update);
    assert_eq!(e.outcome, RowOutcome::Refused);
    assert!(e.is_empty());
    assert_eq!(r.encode_state(), before, "the doc is unchanged");
    assert_eq!(r.probe(&update).outcome, RowOutcome::Refused);
}

#[test]
fn union_takes_a_snapshot_whose_probe_is_refused() {
    // same case; room_state squashes the update's content with the blocks after it
    let mut r = Replayer::from_state(ReplayMode::Gc, &doc_state).unwrap();
    assert_eq!(r.probe(&room_state).outcome, RowOutcome::Refused, "the refused shape again");
    let e = r.union(&room_state);
    assert_eq!(e.outcome, RowOutcome::Applied);
    let ins = insertions_of(&update);
    assert_eq!(e.inserted.intersect(&ins), ins);  // the refused row's content is the union's effect (IdSet has no subset_of in yrs 0.27.3)
    assert_eq!(xml_string(&r), xml_string_of_state(&room_state));
    let mut r2 = Replayer::from_state(ReplayMode::KeepDeleted, &doc_state).unwrap();
    let e2 = r2.merge_gap_state(&room_state);              // merge_state is Refused, so it takes the union
    assert_eq!(e2.outcome, RowOutcome::Applied);
    assert_eq!(xml_string(&r2), xml_string_of_state(&room_state));
}

#[test]
fn union_adds_what_the_state_holds_and_the_replay_lacks() {
    let e = r.union(&state_with_extra_paragraph);
    assert_eq!(e.outcome, RowOutcome::Applied);
    assert_eq!(e.inserted, insertions_of(&extra_paragraph_update));
    assert!(e.deleted.is_empty());
}

#[test]
fn undecodable_and_rejected_rows_are_reported() {
    assert!(matches!(r.apply(&[0xff, 0xff, 0xff]).outcome, RowOutcome::Undecodable(_)));
    // a row yrs rejects part-way: fixtures::parked_block_and_the_edit_it_fails_on()
    let (crafted, edit, _) = fixtures::parked_block_and_the_edit_it_fails_on();
    r.apply(&crafted);                       // parks its string
    let e = r.apply(&edit);                  // integrates the edit, then the parked string fails
    assert!(matches!(e.outcome, RowOutcome::ApplyError(_)));
    assert!(!e.inserted.is_empty(), "what integrated before the error is reported");
}

#[test]
fn probe_leaves_the_replay_untouched() {
    let before = r.encode_state();
    let e = r.probe(&state_with_extra_paragraph);
    assert!(!e.inserted.is_empty());
    assert_eq!(r.encode_state(), before);
}

#[tokio::test]
async fn run_isolated_turns_a_panic_into_an_error_and_returns_the_guard() {
    let (r, g) = run_isolated((), Duration::from_secs(5), || 7).await;
    assert_eq!((r.unwrap(), g), (7, Some(())));
    let (r, g) = run_isolated((), Duration::from_secs(5), || -> u8 { panic!("boom") }).await;
    assert!(matches!(r, Err(IsolateError::Panicked(_))) && g.is_some());
}

#[tokio::test]
async fn a_timed_out_replay_keeps_its_guard_until_the_thread_ends() {
    // guard = DropFlag(Arc<AtomicBool>) setting the flag on drop
    let (r, g) = run_isolated(guard, Duration::from_millis(100), || std::thread::sleep(Duration::from_millis(600))).await;
    assert!(matches!(r, Err(IsolateError::TimedOut)) && g.is_none());
    assert!(!flag.load(SeqCst), "the thread still holds the guard");
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(flag.load(SeqCst));
}
```

If `t3` cannot be made to integrate past the gap with root-level paragraphs, build it the way `a_block_integrated_past_a_gap_persists_the_raw_update` in `crates/knot-crdt/tests/effective_update.rs:544` does.

Before writing `the_skip_underflow_is_refused_not_applied`, reproduce the panic: add `#[test] #[ignore] fn find_the_skip_underflow()` to `tests/replay.rs`, a port of M1.2's property (`crates/knot-crdt/tests/effective_update.rs`: `struct Client` at :79, `struct Server` at :195, `deliveries` at :380 and the three-client property loop through :470) with both `has_gaps` guards removed, that applies every delivery to a plain yrs doc inside `catch_unwind` (a debug build, so the subtraction panics) and, on the first panic, prints the seed, the step, the doc's full state, the update and the full state of the generator's reference doc that holds both (the "room", which applied them in an order that worked) as hex. Run it (`cargo nextest run -p knot-history --test replay --run-ignored only find_the_skip_underflow --no-capture`); M1.2 hit the panic at seed 5. Minimise the triple by hand into `fn skip_underflow_case() -> (Vec<u8>, Vec<u8>, Vec<u8>)` (doc state, update, room state; hex literals, in `tests/common/underflow.rs` so Task 4 uses it too) and note in its doc comment which struct of the update meets which Skip of the doc, and that the room state holds the update's content squashed with the blocks after it. If 2,000 seeds give no panic, record that in Task 11's deviation notes, drop `the_skip_underflow_is_refused_not_applied`, `union_takes_a_snapshot_whose_probe_is_refused` and Task 4's `a_refused_row_surfaces_as_a_gap_entry`, and ship only the post-apply check below.

If `a_checkpoint_with_pending_nested_content_resumes_like_one_that_never_round_tripped` fails once the replay is implemented, take the pending fallback (Rulings): `encode_state` without pending content, keep the test with the resume re-applying u3 and u4 after `from_state`, and record it for Tasks 4, 6, 7 and 11.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --test replay`
Expected: FAIL to compile (crate or items missing).

- [ ] **Step 3: Implement `replay.rs` and `isolate.rs`**

`apply`: decode (`Update::decode_v1`; error → `Undecodable`, empty sets); pre-validate; read pending fingerprints (`store().pending_update()` insertions, `pending_ds()`) before; `txn.apply_update(u)` (error → `ApplyError`, keep going); clone `insert_set()` and `delete_set()` before `commit()`; pending after; `parked_* = after − before`; then update the past-gap set and run the post-apply check. The past-gap bookkeeping is the one part the signature does not determine:

```text
past_gap: IdSet — blocks the doc holds beyond its state-vector clock (behind a Skip).
from_state / new: compute once, as crates/knot-crdt/tests/effective_update.rs:294 `has_gaps` does:
  decode encode_state_as_update_v1(default) → insertions(true), keep ranges at or above sv[client],
  minus the pending update's insertions.
after each apply: past_gap.merge(insert_set); drop every range below the new sv[client].
pre-validate update U: fn would_underflow(&self, u: &Update) -> bool, written from Step 1's
  reproducer (its doc comment cites block_store.rs:318 and the reproducer's shape). It must refuse
  that reproducer and nothing the two "trimmed and applied" tests apply. Overlap with past_gap alone
  is NOT a refusal: yrs trims held blocks first (Rulings).
post-apply check, only when past_gap held a range of a client U names before the apply:
  held_after = insertions(true) of the decoded full state (as for past_gap);
  ok = held_after ⊆ held_before ∪ U.insertions(true), and for every client the highest held clock
  ≤ max(before, U's); not ok → outcome Corrupt (release builds wrap instead of panicking).
```

`merge_state`, `probe` and `union` pre-validate and post-check the same way; a refused one returns `Refused` without touching the doc. `merge_gap_state` is `merge_state`, then `union` on `Refused`. Unit test in `replay.rs` (`mod tests`): `post_apply_check_flags_held_ranges_the_update_did_not_bring` over synthetic `IdSet`s.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-history --test replay && cargo nextest run -p knot-history --lib replay`
Expected: PASS. Check once that `the_skip_underflow_is_refused_not_applied` panics at `block_store.rs:318` with `would_underflow` stubbed to `false` (debug build), then restore it.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock crates/knot-history
git commit -m "feat(history): knot-history crate with a replay engine that never panics on gapped rows"
```

---

### Task 2: Parked claims and session grouping

**Files:**
- Create: `crates/knot-history/src/claims.rs`, `crates/knot-history/src/sessions.rs` (unit tests in-module, `#[cfg(test)] mod tests`)
- Modify: `crates/knot-history/src/lib.rs`

**Interfaces:**
- Consumes: `yrs::IdSet`, `knot_storage::UpdateSource`.
- Produces:
  - `claims.rs`: `pub struct ParkedClaim { pub rev: i64, pub author: Option<Uuid>, pub gap: bool, pub inserts: IdSet, pub deletes: IdSet }`; `pub struct Credit { pub parked_rev: i64, pub author: Option<Uuid>, pub gap: bool, pub inserted: IdSet, pub deleted: IdSet }`; `#[derive(Default, Clone)] pub struct Claims` with `pub fn from_json(v: &serde_json::Value) -> Result<Claims, ClaimsError>`, `pub fn to_json(&self) -> serde_json::Value` (array of `{"rev", "author", "gap", "inserts", "deletes"}`, the sets as base64 (standard) of `encode_v1`), `pub fn record(&mut self, rev: i64, author: Option<Uuid>, inserts: IdSet, deletes: IdSet)` (ignores empty), `pub fn record_gap(&mut self, rev: i64, inserts: IdSet, deletes: IdSet)` (a gap merge's newly parked content, `gap = true`, author `None`), `pub fn revs(&self) -> Vec<i64>` (the open row claims' revs, ascending; the pending fallback's `pending_revs`), `pub fn credit(&mut self, inserted: &mut IdSet, deleted: &mut IdSet) -> Vec<Credit>` (oldest claim first: intersect, move the intersection from the row's sets into the credit, shrink the claim, drop empty claims), `pub fn len(&self) -> usize`.
  - `sessions.rs`: `pub const SESSION_GAP: chrono::Duration` (20 min), `pub const SEAL_MARGIN: chrono::Duration` (60 s); `pub enum EntryKind { Session, Restore, Copy, Import, Template, Erase, Baseline, Gap }` with `as_str` and `FromStr` (the SQL spellings); `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum ContributionKind { Row(Option<UpdateSource>), GapCredit, Gap }` (`Row(None)`: a 0.9.9-era or old-replica row; `GapCredit`: a parked row's content that a gap merge drained, credited to that row's author, grouped exactly like `Row(Some(Edit))` and also setting the session's `meta.gap_credit_rev` to its rev (Rulings, `merge_gap_at_end`); `Gap`: content a snapshot holds and no row carries, or a gap claim's credit); `pub struct Contribution { pub rev: i64, pub at: DateTime<Utc>, pub author: Option<Uuid>, pub kind: ContributionKind, pub meta: Option<serde_json::Value>, pub inserted: IdSet, pub deleted: IdSet }`; `#[derive(Debug, Clone, PartialEq)] pub struct EntryDraft { pub id: Option<i64>, pub kind: EntryKind, pub author: Option<Uuid>, pub started_at: DateTime<Utc>, pub ended_at: DateTime<Utc>, pub start_rev: Option<i64>, pub end_rev: Option<i64>, pub sealed: bool, pub inserted: IdSet, pub deleted: IdSet, pub meta: serde_json::Value, pub baseline_id: Option<i64> }`; `pub struct Grouper` with `pub fn new(open: Vec<EntryDraft>) -> Grouper`, `pub fn feed(&mut self, c: Contribution)`, `pub fn seal_idle(&mut self, now: DateTime<Utc>)` (seals open sessions with `ended_at < now − SESSION_GAP − SEAL_MARGIN`), `pub fn finish(self) -> Vec<EntryDraft>` (every entry it changed or created, sealed or open, in creation order; open sessions it never touched are not returned).

Session `meta`: `{"accepted_by": [uuid, …], "suggestion_ids": [uuid, …]}` accumulated (deduplicated, insertion order) from `suggestion` rows' `meta.accepted_by` (a single uuid per row, `routes/api/suggestions.rs:533`) and `meta.suggestion_id`, plus `"gap_credit_rev": <rev>` after a `GapCredit` (overwritten by a later one); `{}` otherwise. Special entries copy the row's `meta` (or `{}`) and, for `restore_fallback`, add `"fallback": true`. A `Gap` contribution touches no session (seals none, joins none) and becomes a sealed single-rev `Gap` entry (author `None`, meta `{}`). A session's `started_at` = min and `ended_at` = max of the times it has seen; `c.at − ended_at` that is negative counts as joining (Rulings).

- [ ] **Step 1: Write the failing unit tests**

```rust
#[test] fn two_authors_at_once_get_two_sessions()        // A rows at 09:00, 09:15, 09:30, 09:40 (each ≤ 20 min apart); B rows at 09:10, 09:20 → two open sessions, A start_rev..end_rev spans B's revs
#[test] fn a_row_more_than_20_minutes_later_opens_a_new_session() // gap 20 min exactly joins; 20 min + 1 s seals and opens
#[test] fn a_special_row_seals_its_authors_session_only()
         // for src in [Restore, RestoreFallback, Import, Template, Copy] (Copy at a non-first rev):
         // A session open, B session open; A row with Row(Some(src)) and meta {"k": 1} → A sealed + one sealed entry of
         // the matching EntryKind (Restore for both restore sources) with meta {"k": 1} (+ "fallback": true for RestoreFallback); B still open
#[test] fn restore_fallback_is_a_restore_entry_marked_fallback()
#[test] fn task_and_legacy_rows_join_the_authors_session()
         // A rows with Row(Some(Edit)) at 09:00, Row(Some(Task)) at 09:05, Row(None) at 09:10 → one open Session, revs 1..3
#[test] fn a_gap_is_its_own_sealed_entry_even_with_an_author_less_session_open()
         // author-less Row(Some(Edit)) at 09:00 opens a session (author None); Gap contribution at 09:05 → a sealed Gap entry,
         // the author-less session still open, unchanged; a further author-less row at 09:10 joins that session
#[test] fn an_earlier_contribution_never_moves_the_end_back()
         // A rows at 09:00 and 09:20; then a contribution by A at 09:10 (a late gap credit) → one session, started_at 09:00,
         // ended_at 09:20, end_rev the latest rev
#[test] fn a_gap_credit_joins_the_authors_session_and_marks_its_rev()
         // A Row(Some(Edit)) at rev 1, 09:00; GapCredit by A at rev 3, 09:05 → one open session, end_rev 3,
         // meta["gap_credit_rev"] == 3; A Row(Some(Edit)) at rev 4 → end_rev 4, gap_credit_rev still 3;
         // A's session sealed, then a GapCredit by A at rev 9 → a new session with start_rev = end_rev = 9, gap_credit_rev 9
#[test] fn suggestion_rows_join_the_suggesters_session_and_note_who_accepted()
         // assert_eq!(meta["accepted_by"], json!([owner])); assert_eq!(meta["suggestion_ids"], json!([s1, s2]));
#[test] fn seal_idle_respects_the_margin()                 // ended_at = now − 21 min → open; now − 21 min − 1 s → sealed
#[test] fn sessions_accumulate_their_rows_effects()        // inserted/deleted of two rows merged; start/end rev and time
#[test] fn author_less_rows_group_under_no_author()
#[test] fn claims_credit_the_parking_rows_author_first()
         // claims.record(5, Some(a), I_parked, D_parked); row by b integrates I_parked ∪ I_own
         // let credits = claims.credit(&mut ins, &mut del);
         // assert_eq!(credits[0].author, Some(a)); assert_eq!(ins, I_own); assert_eq!(claims.len(), 0)
#[test] fn gap_claims_credit_as_gap_content()
         // claims.record_gap(5, I_p, ∅); a row integrates I_p → one Credit with gap == true, author None
#[test] fn claims_round_trip_through_json()           // row and gap claims
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --lib`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `claims.rs` and `sessions.rs`** per Interfaces and the session rule in Global Constraints. A special contribution with an empty effect reaches the grouper only as the page's first `copy` row (Global Constraints; the pass filters every other), and `feed` turns it into its entry with empty sets.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-history --lib`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-history/src
git commit -m "feat(history): parked claims and per-author sessions"
```

---

### Task 3: Spike and diff core — union-doc diff and word counts

**Files:**
- Create: `crates/knot-history/src/diff.rs`, `crates/knot-history/src/words.rs`
- Modify: `crates/knot-history/src/lib.rs`
- Test: `crates/knot-history/tests/words.rs`

**Interfaces:**
- Consumes: `Replayer` (Task 1) to build the skip-GC doc H in tests.
- Produces (M1.4 builds `pm_json` on these):
  - `diff.rs`: `pub struct EntryDiff { pub s: Snapshot, pub s_lo: Snapshot, pub union: yrs::Doc, pub added: IdSet, pub removed: IdSet }`; `pub fn entry_diff(h: &yrs::Doc, inserted: &IdSet, deleted: &IdSet) -> Result<EntryDiff, DiffError>` (`h` must be skip-GC, replayed to the entry's `end_rev`); `pub enum ChunkKind { Plain, Added, Removed }`; `pub struct TextChunk { pub segment: usize, pub kind: ChunkKind, pub text: String }`; `pub fn text_chunks(d: &EntryDiff) -> Vec<TextChunk>` (every `XmlText` reachable from `d.union`'s `default` fragment, depth-first, each through `diff_range(&mut d.union.transact_mut(), Some(&d.s), Some(&d.s_lo), |c| c)`; string inserts only; `segment` numbers the runs of consecutive string inserts, starting a new segment at each `XmlText` and after each non-string insert, so words never join across a paragraph or an inline node); `pub fn strip_zero_clocks(sv: StateVector) -> StateVector`; `pub enum DiffError { Encode(String), NotKeepDeleted }`.
  - `words.rs`: `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub struct WordCounts { pub added: i32, pub removed: i32 }`; `pub fn count_words(chunks: &[TextChunk]) -> WordCounts` (concatenates each maximal run of consecutive chunks of the same kind within one `segment`, then `split_whitespace().count()` once per `Added` or `Removed` run: yrs splits one word into several chunks at item and format boundaries, Global Constraints); `pub fn entry_word_counts(h: &yrs::Doc, inserted: &IdSet, deleted: &IdSet) -> Result<WordCounts, DiffError>`.

The sets, exactly as the architecture's SHOW CHANGES: `removed = deleted.diff(inserted)`; `added = inserted.diff(&s.delete_set)`; `s_lo = Snapshot::new(s.state_map.clone(), s.delete_set.diff(&removed).merge(inserted))`; `s_union = Snapshot::new(strip_zero_clocks(s.state_map.clone()), s.delete_set.diff(&removed))`; `union` = a fresh skip-GC doc with `h.transact().encode_state_from_snapshot(&s_union, &mut EncoderV1)` applied.

- [ ] **Step 1: Write the spike tests** in `crates/knot-history/tests/words.rs`

The history (the design's overlap case, on a doc seeded from a GC'd checkpoint):
- checkpoint state (a `ReplayMode::Gc` replay, then `encode_state()`): paragraph P1 "alpha beta", paragraph P2 "old words here", plus one word typed and deleted earlier (so the checkpoint holds GC blocks).
- H = `Replayer::from_state(ReplayMode::KeepDeleted, &checkpoint)`, then rows: A types " gamma" at the end of P1; B types " delta" at the end of P1; A deletes "beta"; B deletes "alpha"; A deletes P2 entirely; A types "zeta" and deletes it again. I and D of A = the union of A's rows' `inserted` / `deleted`.

```rust
#[test]
fn union_diff_on_a_doc_seeded_from_a_gc_checkpoint_shows_only_the_authors_changes() {
    let d = entry_diff(h.doc(), &i_a, &d_a).expect("encode_state_from_snapshot on a mixed doc");
    let chunks = text_chunks(&d);
    let added: Vec<_> = chunks.iter().filter(|c| c.kind == ChunkKind::Added).map(|c| c.text.as_str()).collect();
    let removed: Vec<_> = chunks.iter().filter(|c| c.kind == ChunkKind::Removed).map(|c| c.text.as_str()).collect();
    assert_eq!(added.concat(), " gamma");
    assert_eq!(removed.concat(), "betaold words here");     // P2's text is reachable through the union doc
    assert!(!chunks.iter().any(|c| c.text.contains("delta") && c.kind != ChunkKind::Plain), "B's insert is plain");
    assert!(!chunks.iter().any(|c| c.text.contains("alpha")), "B's delete is visible in neither state");
    assert!(!chunks.iter().any(|c| c.text.contains("zeta")), "typed and deleted inside the entry shows nowhere");
    assert_eq!(entry_word_counts(h.doc(), &i_a, &d_a).unwrap(), WordCounts { added: 1, removed: 4 });
}

#[test]
fn b_sees_only_bs_changes() { /* same history; for B: added " delta" (1), removed "alpha" (1) */ }

#[test]
fn strip_zero_clocks_prevents_the_clock_minus_one_underflow() {
    // a snapshot whose state_map names a client the doc holds blocks for with clock 0
    let mut sv = s.state_map.clone();
    sv.set_min(client_of_p1, 0);             // a client the doc holds blocks for, at clock 0
    let snap = Snapshot::new(strip_zero_clocks(sv), s.delete_set.clone());
    assert!(h.doc().transact().encode_state_from_snapshot(&snap, &mut EncoderV1::new()).is_ok());
}

#[test]
fn entry_diff_refuses_a_gc_doc() {
    assert!(matches!(entry_diff(gc_doc, &i, &d), Err(DiffError::NotKeepDeleted)));
}

#[test]
fn words_count_whitespace_tokens_per_run() {
    // chunk(segment, kind, text)
    assert_eq!(count_words(&[chunk(0, Added, "gam"), chunk(0, Added, "ma two "), chunk(0, Removed, "x"),
                             chunk(0, Plain, "a b c"), chunk(1, Added, "y")]),
               WordCounts { added: 3, removed: 1 });           // "gamma two" is 2, "y" is 1
    assert_eq!(count_words(&[chunk(0, Removed, "beta"), chunk(1, Removed, "old")]),
               WordCounts { added: 0, removed: 2 });           // runs never cross a segment
    assert_eq!(count_words(&[chunk(0, Added, "ab"), chunk(0, Plain, " "), chunk(0, Added, "cd")]),
               WordCounts { added: 2, removed: 0 });           // a Plain chunk ends the run
}

#[test]
fn a_word_typed_in_two_items_counts_once() {
    // on H: A types "gam" at the end of P1, then "x" at the start of P2 (the cursor left), then "ma" right after "gam"
    // (a later clock, not squashed with "gam"); I_a, D_a = those rows' sets
    let chunks = text_chunks(&entry_diff(h.doc(), &i_a, &d_a).unwrap());
    assert!(chunks.iter().filter(|c| c.kind == ChunkKind::Added).count() >= 3, "precondition: gamma comes back split");
    assert_eq!(entry_word_counts(h.doc(), &i_a, &d_a).unwrap(), WordCounts { added: 2, removed: 0 }); // "gamma", "x"
}

#[test]
fn bold_applied_mid_word_counts_once() {
    // on H: A types " gamma" at the end of P1, then formats its "mm" bold (both rows in A's entry)
    assert_eq!(entry_word_counts(h.doc(), &i_a, &d_a).unwrap(), WordCounts { added: 1, removed: 0 });
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --test words`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `diff.rs` and `words.rs`** per Interfaces (`DiffError::NotKeepDeleted` when `h.skip_gc()` is false).

- [ ] **Step 4: Run them; this is the spike**

Run: `cargo nextest run -p knot-history --test words`
Expected: PASS. If `encode_state_from_snapshot` errors, or the chunks contradict the assertions on the mixed doc while the same history from an empty start passes, apply the spike-fallback ruling (Rulings): keep the failing test as `#[ignore]` with the observed output in its doc comment, implement the text-list fallback with the signatures the ruling names (`WindowTexts`, `Replayer::collect_texts`, `entry_diff_with`, `entry_word_counts_with`; the tests then install the collector on H before replaying the entry's rows), make every other assertion pass, and note the case for Task 7 (step 9 installs the collector) and Task 11's deviation notes.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-history
git commit -m "feat(history): entry diff on a union doc and word counts"
```

---

### Task 4: Pass compute — attribution, gap check, checkpoints, genesis

**Files:**
- Create: `crates/knot-history/src/pass.rs`, `crates/knot-history/src/genesis.rs`, `crates/knot-history/src/canonical.rs`
- Modify: `crates/knot-history/src/lib.rs`
- Modify: `crates/knot-history/tests/common/mod.rs` (Task 1's; add the history generator)
- Test: `crates/knot-history/tests/pass.rs`

**Interfaces:**
- Consumes: Tasks 1-2; `knot_storage::DocUpdate` (`updates_store.rs:97`).
- Produces:
  - `canonical.rs`: `pub fn visible_content(doc: &yrs::Doc) -> serde_json::Value` (the `default` fragment as nested `{"tag", "attrs" (sorted, `Out::to_json`), "children"}` / `{"text": [{"insert", "attrs" (sorted)}]}` from `XmlTextRef::diff`).
  - `pass.rs`: `pub const CHECKPOINT_FLOOR_BYTES: u64`; `pub enum CheckpointKind { Genesis, Periodic, Gap }` (`as_str`, `FromStr`); `pub struct CheckpointWrite { pub rev: i64, pub kind: CheckpointKind, pub state_bytes: Vec<u8>, pub state_vector: Vec<u8> }`; `pub struct SnapshotProbe { pub rev: i64, pub created_at: DateTime<Utc>, pub state_bytes: Option<Vec<u8>> }` (`None`: the snapshot is too large to probe even alone (Rulings); `run_pass` leaves it unchecked, `snapshots_unchecked["too_large"]`, when it reaches its rev); `#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)] pub enum Stage { #[default] Idle, Checkpoint, Prefix(i64), Row(i64), Probe(i64), KeptGap(i64), Emit(i64) }` (`Checkpoint` in genesis: merging the baseline); `#[derive(Clone, Default)] pub struct Progress(Arc<Mutex<ProgressState>>)` (`ProgressState { stage: Stage, since: std::time::Instant, cancelled: bool }`) with `pub fn set(&self, s: Stage)` (records `since`; a no-op once cancelled), `pub fn get(&self) -> Stage`, `pub fn cancel(&self) -> (Stage, std::time::Duration)` (freezes and returns the stage then current and how long it had been current; Rulings) and `pub fn is_cancelled(&self) -> bool` (a poisoned lock is read through `into_inner`); `#[doc(hidden)] #[derive(Debug, Clone, Default)] pub struct ReplayHooks { pub panic_at_rev: Option<i64>, pub corrupt_at_rev: Option<i64>, pub panic_probe_at: Option<i64>, pub corrupt_merge_at: Option<i64>, pub sleep_at_rev: Option<(i64, std::time::Duration)>, pub sleep_at_emit: Option<std::time::Duration>, pub sleep_at_baseline: Option<(i64, std::time::Duration)>, pub corrupt_kept_gap_at: Option<i64> }` (tests only: `sleep_at_baseline` sleeps in genesis at `Stage::Checkpoint` while merging the baseline with that id; `corrupt_kept_gap_at` turns the merge of the kept gap at that rev into `Corrupt`; `panic_at_rev` panics while that row's apply is in progress, `Progress` already at `Row(rev)`, or at `Prefix(rev)` when the replay prefix re-applies it; `corrupt_at_rev` turns that row's outcome into `Corrupt` after its apply, as a new row or in the prefix; `panic_probe_at` panics in the probe of the snapshot at that rev; `corrupt_merge_at` turns that snapshot's gap merge into `Corrupt`; `sleep_at_rev` `std::thread::sleep`s at that row's stage, like `panic_at_rev`; `sleep_at_emit` sleeps inside the first `emit`, at `Stage::Emit`); `pub struct PassInput { pub checkpoint_rev: i64, pub checkpoint_state: Vec<u8>, pub reapply: Vec<DocUpdate>, pub replay: Vec<DocUpdate>, pub genesis_rev: i64, pub built_rev: i64, pub rows: Vec<DocUpdate>, pub rows_exhausted: bool, pub open: Vec<EntryDraft>, pub claims: Claims, pub kept_gaps: Vec<KeptGap>, pub snapshots: Vec<SnapshotProbe>, pub snapshot_checked_rev: i64, pub skip_revs: BTreeSet<i64>, pub progress: Progress, pub now: DateTime<Utc>, pub rows_budget: Option<std::time::Duration>, pub checkpoint_floor: u64, #[doc(hidden)] pub hooks: ReplayHooks }` (`rows_budget`: the rows deadline starts when the first new row's apply starts, so the decode and the prefix never use it up) (`reapply`: empty unless the pending fallback was taken (Rulings), then the rows of the checkpoint's `pending_revs`; `replay`: the rows after the checkpoint through `built_rev` (no gap checkpoint lies in that range: the start is the latest checkpoint at or below `built_rev`, so a pass never merges a gap state below its rows; the only gap states it merges are `kept_gaps`); `kept_gaps`: `pub struct KeptGap { pub rev: i64, pub created_at: DateTime<Utc>, pub state_bytes: Vec<u8> }`, the gap checkpoints above `built_rev` that a rebuild kept (Rulings, gap states), ascending, only those at or below the last row in `rows`; empty outside a rebuild; `snapshots`: stamped snapshots with `snapshot_checked_rev < snapshot_rev`, ascending, one per rev, as many as the builder's input cap allowed (Task 7 step 6), oversized ones with `state_bytes: None`; `rows` stop before the rev of a snapshot left out for the cap, and can be empty when only late snapshots fit (Task 7 step 6); `checkpoint_floor`: `CHECKPOINT_FLOOR_BYTES` in production, a few hundred bytes in tests; every `DocUpdate` here has `rev: Some(_)`, since `rows_in_range` selects numbered rows only, and `run_pass` reads it as `row.rev.unwrap_or(0)`, which a 0 turns into a rev gap); `#[derive(Default)] pub struct PassStats { pub rows: u64, pub unapplied: BTreeMap<&'static str, u64>, pub snapshots_unchecked: BTreeMap<&'static str, u64>, pub parked: u64, pub gaps: u64, pub rev_gap: bool, pub stopped_early: bool, pub encodes: Vec<(i64, usize)> }` (`encodes`: rev and size of every `encode_state` the checkpoint policy ran); `pub struct PassOutput { pub built_rev: i64, pub snapshot_checked_rev: i64, pub entries: Vec<EntryDraft>, pub checkpoints: Vec<CheckpointWrite>, pub claims: Claims, pub refused_revs: Vec<i64>, pub stats: PassStats }` (at most one checkpoint per rev; `refused_revs`: new rows whose outcome was `Refused`, which the write adds to `skip_revs`, Rulings); `pub fn run_pass(input: PassInput) -> Result<PassOutput, ReplayError>`.
  - `pass.rs`, the replay every reader that does not build uses (word counts; M1.4 views): `pub struct WindowInput { pub mode: ReplayMode, pub checkpoint_state: Vec<u8>, pub reapply: Vec<DocUpdate>, pub rows: Vec<DocUpdate>, pub gap_states: Vec<(i64, Vec<u8>)>, pub skip_revs: BTreeSet<i64>, pub end_rev: i64, pub merge_gap_at_end: bool, pub progress: Progress }` (rows after the checkpoint through `end_rev` in rev order; `gap_states` the gap checkpoints after it through `end_rev`, ascending; `merge_gap_at_end`: whether a gap state at `end_rev` itself is merged, true only when replaying for that gap entry, Rulings); `#[derive(Debug, thiserror::Error)] pub enum WindowError { State(String), Corrupt { rev: i64 }, GapState { rev: i64 }, Cancelled }`; `pub fn replay_window(input: WindowInput) -> Result<Replayer, WindowError>` — `Replayer::from_state(mode, ..)`, the reapply rows, then each row not in `skip_revs` (`Refused`, `ApplyError`, `Undecodable` passed over; `Corrupt` → `Err(Corrupt)`; `progress.is_cancelled()` before each row → `Err(Cancelled)`), and, once the replay has passed each gap state's rev R (after row R, applied, skipped or passed over, and before row R + 1), `merge_gap_state` for R < `end_rev`, and for R = `end_rev` only when `merge_gap_at_end` (not `Applied` / `ApplyError` → `Err(GapState { rev })`).
  - `genesis.rs`: `pub struct BaselineInput { pub id: i64, pub snapshot_seq: i64, pub state_bytes: Option<Vec<u8>>, pub created_at: DateTime<Utc> }`; `pub struct GenesisInput { pub page_created_at: DateTime<Utc>, pub history_since: DateTime<Utc>, pub genesis_rev: i64, pub baselines: Vec<BaselineInput>, pub rows: Vec<DocUpdate>, pub skip_revs: BTreeSet<i64>, pub progress: Progress, #[doc(hidden)] pub hooks: ReplayHooks }` (baselines ascending by `snapshot_seq`, rows `rev ≤ genesis_rev` in rev order); `pub struct GenesisOutput { pub checkpoint: CheckpointWrite, pub entries: Vec<EntryDraft>, pub claims: Claims, pub refused_revs: Vec<i64>, pub stats: PassStats }` (`claims`: one claim per legacy row that newly parked content, `rev` = that row's rev, author `None`); `#[derive(Debug, thiserror::Error)] pub enum GenesisError { BaselineRejected { id: i64 }, Corrupt { rev: i64 }, Encode, Cancelled }` (`Cancelled` as `ReplayError::Cancelled`: checked before each row); `pub fn build_genesis(input: GenesisInput) -> Result<GenesisOutput, GenesisError>`. `BaselineInput::state_bytes` is `Some` only for the one baseline genesis starts from (the newest whose bytes the builder loaded) and `None` for the rest (they only make entries).

`run_pass`, in order (the part the signatures do not determine):

```text
threshold(size) = max(checkpoint_floor, size × CHECKPOINT_STATE_FRACTION)
emit(kind, rev): progress.set(Emit(rev)); (hooks.sleep_at_emit: sleep, first emit only)
    state = replayer.encode_state(); verify Replayer::from_state(Gc, &state) gives the same state
    vector (else: a Periodic is not emitted; a Gap returns Err(CorruptSnapshot{snapshot_rev}) of the snapshot it
    came from); replace any checkpoint already emitted at rev (Gap wins over Periodic); progress.set(Idle)
cancelled check: before each prefix row, each probe and each new row: if progress.is_cancelled() → return Err(Cancelled)
credit(effect, rev, at, kind):                              (rows: kind Row(Some(Edit)); gap merges: kind GapCredit)
    for c in claims.credit(&mut effect.inserted, &mut effect.deleted):
        if c.parked_rev ≤ genesis_rev: drop it                (genesis content, never credited)
        else if c.gap: grouper.feed(Contribution{rev, at, author: None, kind: Gap, meta: None, c's sets}); stats.gaps += 1
        else grouper.feed(Contribution{rev, at, author: c.author, kind, meta: None, c's sets})
after_gap_merge(e, at_rev, at):                              (both gap sources, once e is Applied or ApplyError)
    credit(e, at_rev, at, GapCredit)
    if e.parked_* non-empty: claims.record_gap(at_rev, e.parked_inserts, e.parked_deletes)
    if !e.is_empty(): grouper.feed(Contribution{at_rev, at, author: None, kind: Gap, meta: None, e's sets}); stats.gaps += 1
    emit(Gap, at_rev); bytes_since_cp = 0
kept(k):                                                     (a gap state kept across a rebuild, right after row k.rev)
    progress.set(KeptGap(k.rev)); e = replayer.merge_gap_state(k.state_bytes)
    if e.outcome ∉ {Applied, ApplyError} || hooks.corrupt_kept_gap_at == Some(k.rev) → return Err(KeptGap{rev: k.rev})
    progress.set(Idle); after_gap_merge(e, k.rev, k.created_at)   (emits the Gap checkpoint at k.rev even when e is empty)
gap(snapshot, at_rev):                                       (both probe sites)
    if snapshot.state_bytes is None → snapshots_unchecked["too_large"] += 1; return Ok    (oversized, Rulings)
    progress.set(Probe(snapshot.rev)); if hooks.panic_probe_at == Some(snapshot.rev) { panic!("test hook") }
    g = replayer.probe(snapshot.state_bytes)
    match g.outcome:
        Undecodable | Corrupt → snapshots_unchecked["undecodable" | "corrupt"] += 1; progress.set(Idle); return Ok
        Refused → e = replayer.union(snapshot.state_bytes)               (fallback, Rulings)
                  if e.outcome ∉ {Applied, ApplyError} → snapshots_unchecked["refused" | "corrupt" | ..] += 1; progress.set(Idle); return Ok
        Applied | ApplyError → if g.is_empty() { progress.set(Idle); return Ok }
                  e = replayer.merge_state(snapshot.state_bytes)
                  if e.outcome == Corrupt || hooks.corrupt_merge_at == Some(snapshot.rev) → return Err(CorruptSnapshot{snapshot_rev: snapshot.rev})
                  if e.outcome == Refused → e = replayer.union(snapshot.state_bytes)   (Rulings: as a refused probe)
                  if e.outcome ∉ {Applied, ApplyError} → snapshots_unchecked["refused" | "corrupt" | ..] += 1; progress.set(Idle); return Ok   (no Gap emitted)
    progress.set(Idle)
    after_gap_merge(e, at_rev, snapshot.created_at)

grouper = Grouper::new(open); claims = input.claims; next = built_rev + 1
if rows and snapshots are both empty: skip the replay entirely (no decode, no prefix: nothing below needs it), go to the seal step
progress.set(Checkpoint); replayer = Replayer::from_state(Gc, checkpoint_state)?   (Err → ReplayError::State)
for row in reapply: replayer.apply(row)                       (pending fallback only; effects ignored)
for row in replay (rev ≤ built_rev): unless row.rev ∈ skip_revs:
    progress.set(Prefix(row.rev)); if hooks.panic_at_rev == Some(row.rev) { panic!("test hook") }
    e = replayer.apply(row)
    if e.outcome ∈ {Corrupt, Refused} || hooks.corrupt_at_rev == Some(row.rev) → return Err(Prefix{rev: row.rev})   (Rulings: never SkipRev)
    progress.set(Idle)
bytes_since_cp = Σ replay[].update_bytes.len(); last_state_size = checkpoint_state.len()
for snapshot in snapshots with rev ≤ built_rev (ascending):   (late snapshots, Rulings)
    gap(snapshot, built_rev)?; snapshot_checked_rev = max(snapshot_checked_rev, snapshot.rev)
for row in rows:
    if row.rev != next { stats.rev_gap = true; break }          (rev order is commit order: a gap is a bug)
    if row.rev ∈ skip_revs { count "skipped"; kept gap at row.rev: kept(it)?; built_rev = row.rev; next += 1; continue }
    progress.set(Row(row.rev)); deadline ??= now_instant + rows_budget       (set once, at the first new row)
    if hooks.panic_at_rev == Some(row.rev) { panic!("test hook") }           (sleep_at_rev likewise sleeps here and in the prefix)
    effect = replayer.apply(row.update_bytes)
    if effect.outcome == Corrupt || hooks.corrupt_at_rev == Some(row.rev) → return Err(ReplayError::Corrupt{rev: row.rev})
    progress.set(Idle)
    count effect.outcome.metric_reason(); if effect.outcome == Refused { refused_revs.push(row.rev) }
    credit(effect, row.rev, row.created_at, Row(Some(Edit)))
    if parked_* non-empty { claims.record(row.rev, row.by_user_id, ..); stats.parked += 1 }
    if !effect.is_empty() || (row.rev == genesis_rev + 1 && row.source == Some(Copy)) {
        grouper.feed(Contribution{row's rev, created_at, by_user_id, Row(source), meta, effect sets}) }
    kept gap at row.rev: kept(it)?                                 (before the probes at this rev)
    snapshots with rev == row.rev: gap(snapshot, row.rev)?; snapshot_checked_rev = row.rev
    bytes_since_cp += row.update_bytes.len()
    if bytes_since_cp >= threshold(last_state_size):
        progress.set(Emit(row.rev)); state = replayer.encode_state(); progress.set(Idle)
        last_state_size = state.len(); stats.encodes.push((row.rev, state.len()))
        if bytes_since_cp >= threshold(state.len()) { emit(Periodic, row.rev) (unless Gap there); bytes_since_cp = 0 }
    built_rev = row.rev; next += 1; stats.rows += 1
    if another row follows && deadline passed { stats.stopped_early = true; break }   (the last row never stops a pass early)
if rows_exhausted && !rev_gap && !stopped_early: grouper.seal_idle(now)
```

`emit` may reuse the `state` the policy just encoded. `build_genesis` publishes `Emit(genesis_rev)` while encoding and verifying its checkpoint and checks `is_cancelled()` before each row, as `run_pass` does. `build_genesis`: on a fresh `Replayer::new(Gc)`, merge the one baseline that has bytes (not `Applied` → `Err(BaselineRejected { id })`; the builder loads the next older baseline's bytes and calls again, so every attempt starts from a fresh replay); apply every row not in `skip_revs` (counted `skipped`), publishing `Progress` (`Row(rev)`: genesis has no prefix) and honouring `hooks.panic_at_rev` / `corrupt_at_rev` exactly as `run_pass` does for new rows, recording each row's newly parked content as its claim and each `Refused` row in `refused_revs`, and on `Corrupt` returning `Err(GenesisError::Corrupt { rev })` with no checkpoint; checkpoint `Genesis` at `genesis_rev` (verified as `emit` does; a state that does not decode is `Err(GenesisError::Encode)`, a plain failure that blames no row); one sealed `Baseline` entry per baseline (`author` None, `started_at = ended_at = created_at`, `baseline_id`, revs None, empty sets, meta `{}`); the "state at rollout" entry per Rulings, compared with the baseline genesis started from (`kind: EntryKind::Baseline`, sealed, `author` None, `baseline_id` None, `start_rev` None, `end_rev = Some(genesis_rev)`, `started_at = ended_at = history_since`, empty sets, meta `{"rollout": true}`). The baseline genesis starts from is the newest one not in `skip_baselines` (the builder passes only those bytes); `hooks.sleep_at_baseline` sleeps during its merge.

- [ ] **Step 1: Write the history generator** in `tests/common/mod.rs`

`fixtures::Rng` and `fixtures::random_edit` (included as in Task 1; from `tests/common/mod.rs` the path is `../../../knot-crdt/tests/common/fixtures.rs`) drive 3 clients (authors A, B, C, `Doc::with_client_id(1..=3)`) editing `default` (insert paragraphs, insert and delete text via `random_edit`, plus a generator step of its own that formats a random range of a paragraph's text with `bold`, since `fixtures::random_edit` (`fixtures.rs:182`) never formats). Each step emits one row (`DocUpdate` with `rev` 1.., `seq` = rev, `by_user_id`, `created_at` advancing by a random 1 s to 30 min, `source` Edit) chosen among: the client's incremental update; a reconnect resend (`encode_state_as_update_v1(&server_sv)`), only while the reference doc has no blocks past a gap (port `has_gaps` from `crates/knot-crdt/tests/effective_update.rs:294` into `tests/common`, as M1.2's generator does: otherwise yrs itself panics in the reference doc); an update held back and delivered later (parks); the previous row's bytes again (a duplicate); and, rarely, a `restore` row by A whose update is a whole-paragraph replace. A reference "room" doc applies every row in order; `fn room_snapshot(&self) -> Vec<u8>` encodes it. `fn rows(&self) -> &[DocUpdate]`. Also expose a fixed builder `History::overlap_case()`: A rows at 09:00, 09:15, 09:30, 09:40, B rows at 09:10, 09:20.

- [ ] **Step 2: Write the failing tests** in `tests/pass.rs`

A test harness `fn build_all(rows, snapshots, max_rows, checkpoint_floor) -> (Vec<EntryDraft> by stable key, Vec<CheckpointWrite>)` emulates the store between passes: genesis (`build_genesis` with no baselines, `genesis_rev` 0), then repeated `run_pass` with at most `max_rows` rows, carrying `claims` and `snapshot_checked_rev`, merging returned entries into its table (by `id`, assigning ids to new ones), choosing the latest checkpoint at or before `built_rev` and feeding the `replay` rows after it, adding each pass's `refused_revs` to its `skip_revs`, and the snapshots with `snapshot_checked_rev < rev ≤ max(built_rev, last row's rev)`. Each snapshot is `(visible_after_pass: usize, SnapshotProbe)`: the harness hands it to passes from that pass index on (0 = from the start), which models a room snapshot written after the builder passed its rev. The property tests pass `checkpoint_floor = 256`, so their passes resume from periodic checkpoints, many of them holding parked content (assert in `many_small_passes_equal_one_pass` that, over all seeds, at least 50 resumes started from a periodic checkpoint whose replay `has_pending()`).

```rust
#[test] fn overlapping_sessions_are_credited_to_their_own_authors()   // overlap_case: A's entry holds only A's ids, B's only B's
#[test] fn a_reconnect_resend_credits_only_what_is_new()               // resend after offline typing: entry of the resender holds only the offline text
#[test] fn parked_content_is_credited_to_the_row_that_delivered_it()   // A's update parked; B's row drains it → A gets an entry at B's rev holding A's ids, B's entry does not
#[test] fn a_row_parked_for_good_creates_no_entry()                     // dependency never delivered: no entry for it, claims.len() == 1, built_rev at the last row, stats.parked == 1
#[test] fn a_refused_row_surfaces_as_a_gap_entry()
    // (doc_state, update, room_state) = skip_underflow_case(): checkpoint_state = doc_state at rev 0, genesis_rev 0;
    // row 1 = update by A (refused: unapplied["skip_underflow"] == 1, no entry for A); snapshot at rev 1 = room_state,
    // whose probe is refused too → union fallback: exactly one Gap entry (author None) whose ins_ids ⊇ the update's
    // insertions, no Session entry holds any of them, snapshots_unchecked is empty, a Gap checkpoint at rev 1,
    // output.refused_revs == [1]; a next pass with skip_revs {1} and one more row by A resumes from that Gap checkpoint
    // with no Prefix error
#[test] fn a_window_over_a_union_gap_checkpoint_merges_it()
    // the same case: genesis checkpoint (doc_state) at rev 0, the union Gap checkpoint at rev 1, then rows 2..3 by A;
    // replay_window(KeepDeleted, checkpoint 0, rows 1..3, gap_states [(1, gap checkpoint)], skip_revs {1}) → Ok, and its
    // canonical::visible_content equals that of Replayer::from_state(Gc, gap checkpoint) plus rows 2..3;
    // with merge_gap_state stubbed to merge_state only (a local copy of the loop) the gap state is Refused: the union is what makes it work
#[test] fn replay_window_merges_a_gap_at_its_end_only_when_asked()
    // a snapshot with an extra paragraph probed at rev 3 (Gap checkpoint at 3), rows 1..3 by A:
    // replay_window(end_rev 3, merge_gap_at_end false) → visible_content lacks the paragraph (the session ending at 3);
    // merge_gap_at_end true → has it (the gap entry's own view); end_rev 4 with row 4 → has it either way
#[test] fn the_rows_budget_starts_at_the_first_new_row()
    // rows_budget Some(0) with a 50-row replay prefix and 3 new rows → stats.rows == 1, stopped_early;
    // rows_budget Some(0) with exactly 1 new row and rows_exhausted → stopped_early false and an idle session is sealed
#[test] fn a_cancelled_pass_stops_at_the_next_row()
    // progress.cancel() before run_pass → Err(Cancelled); Progress::set after cancel leaves get() at the frozen stage
#[test] fn a_failing_prefix_row_blames_the_checkpoint_not_the_row()
    // build rows 1..6 in one pass with checkpoint_floor 256 so a periodic checkpoint sits at rev c < 6; a pass from it with
    // hooks.corrupt_at_rev = Some(c + 1) (a prefix row) → Err(ReplayError::Prefix{rev: c + 1}), not Err(Corrupt);
    // the same hook on a new row → Err(Corrupt{rev})
#[test] fn an_oversized_snapshot_is_left_unchecked()
    // a SnapshotProbe at rev 2 with state_bytes None → Ok, snapshots_unchecked["too_large"] == 1, snapshot_checked_rev 2,
    // no Gap entry; a probe-able snapshot at rev 3 after it is still probed
#[test] fn a_pass_with_nothing_to_replay_decodes_no_checkpoint()
    // checkpoint_state = [0xff, 0xff] (undecodable), no rows, no snapshots, an open session idle for 30 min, rows_exhausted
    // → Ok (not ReplayError::State), the session sealed, no checkpoint emitted
#[test] fn a_gap_merge_parks_as_a_gap_claim()
    // the snapshot holds B's paragraph text whose origin is in content neither the replay nor the snapshot has → the
    // merge parks it: claims has one gap claim; a later row by C delivers the origin → C's entry holds only C's
    // content, and a second Gap entry at C's rev holds the drained text
#[test] fn a_snapshot_whose_probe_corrupts_or_is_undecodable_is_left_unchecked()
    // snapshot bytes [0xff, 0xff, 0xff] at rev 2 → Ok, snapshots_unchecked["undecodable"] == 1, snapshot_checked_rev 2, rows' entries intact
#[test] fn a_corrupt_gap_merge_blames_the_snapshot()
    // hooks.corrupt_merge_at = Some(2) with a snapshot at rev 2 that holds an extra paragraph → Err(CorruptSnapshot{snapshot_rev: 2})
#[test] fn a_corrupt_row_stops_the_pass_with_its_rev()
    // hooks.corrupt_at_rev = Some(3) → Err(Corrupt{rev: 3}); with skip_revs {3} → Ok, unapplied["skipped"] == 1
#[test] fn a_duplicated_row_creates_no_second_entry()                   // edit row twice → one session; restore row twice → exactly one restore entry
#[test] fn rows_without_effect_do_not_seal_a_session()                  // A types at 09:00; A's restore row with an empty effect at 09:05; A types at 09:10 → one open session over both edits, no restore entry
#[test] fn snapshots_of_the_log_report_no_gap()                         // 300 seeds × generator, room_snapshot at every 20th rev: stats.gaps == 0, no Gap entry
#[test] fn a_snapshot_with_content_no_row_carries_is_a_gap_entry()      // snapshot = room ∪ an extra paragraph: one Gap entry (author None, rev = snapshot rev, ins_ids = the extra), a Gap checkpoint, later replays include it
#[test] fn a_snapshot_written_after_its_rev_was_built_is_still_gap_checked()
    // build rows 1..N; then a snapshot at rev N holding room ∪ an extra paragraph, visible from the next pass;
    // (a) a pass with no rows → one Gap entry at rev N, Gap checkpoint at N, snapshot_checked_rev N;
    // (b) separately: one more row N+1, then a pass → the same Gap entry at rev N, the row's own entry at N+1.
    // Also as a build_all case: build_all(rows, [(1, late snapshot)], 3, 256) has the Gap entry.
    // (c) two late snapshots at revs N−1 and N with different extra paragraphs, both visible from the next pass →
    // two Gap entries, both at rev N, and exactly one Gap checkpoint at N, whose state holds both paragraphs
#[test] fn a_gap_merge_credits_the_rows_it_drains()
    // the snapshot holds paragraph P that no row carries; B's row types inside P and parks (its origin is in P);
    // the gap merge drains it → B's entry holds B's text at the gap's rev, the Gap entry holds only P, claims.len() == 0;
    // B's entry ends at the gap's rev R with meta["gap_credit_rev"] == R, and replay_window(KeepDeleted, .., end_rev R,
    // merge_gap_at_end: true, as that rule gives) holds B's text, while merge_gap_at_end false would not
#[test] fn a_kept_gap_state_is_merged_at_its_rev()
    // a first build (build_all) of rows 1..6 with a snapshot at rev 3 holding an extra paragraph X → Gap checkpoint K at 3;
    // a rebuild: genesis again, then one run_pass over rows 1..6 with kept_gaps [K (rev 3, its created_at)] and no snapshots
    // → exactly one Gap entry, at rev 3, started_at = K's created_at, ins_ids = X's insertions; a Gap checkpoint at 3 whose
    // visible content equals K's; every later checkpoint holds X; entries equal the first build's (by stable key)
    // with the snapshot still given as well → still exactly one Gap entry (the probe at 3 finds nothing new)
    // with rows 1..2 only (the pass ends below 3) → no Gap entry, no checkpoint at 3
#[test] fn a_kept_gap_that_does_not_merge_fails_the_pass_with_its_rev()
    // kept_gaps [(3, [0xff, 0xff])] → Err(ReplayError::KeptGap{rev: 3}); hooks.corrupt_kept_gap_at = Some(3) with a valid
    // state → the same
#[test] fn genesis_pending_content_is_never_credited()
    // a legacy row (rev ≤ G) builds on content no row carries and parks in genesis (GenesisOutput.claims holds that row's claim, author None);
    // a later row by A delivers that content → A's entry holds only A's row's own content; no entry holds the legacy content
#[test] fn an_empty_first_copy_row_still_creates_its_entry()
    // rev 1 copy row with an empty update (meta {source_doc_id, source_title}) → one sealed Copy entry with empty sets;
    // the same bytes again at rev 2 → no second entry; an empty restore row at rev 3 → no entry;
    // an empty template row at rev 1 of another page → no entry (rooms never write one, Rulings; the pass treats it like any empty row)
#[test] fn a_skipped_rev_is_not_applied_and_is_counted()                 // skip_revs {2}: built_rev passes 2, unapplied["skipped"] == 1, row 2's content in no entry and not in the replay
#[test] fn many_small_passes_equal_one_pass()                           // 200 seeds: build_all(.., usize::MAX, 256) == build_all(.., 3, 256), entries compared on (kind, author, start/end rev, times, sealed, ins/del encode_v1, meta)
#[test] fn checkpoints_equal_a_fresh_replay_to_their_rev()
    // 100 seeds, build_all(.., 3, 256): for every checkpoint at rev r, Replayer::from_state(Gc, state) and a fresh
    // replay from genesis through r (replay_window with the gap checkpoints as gap_states) have the same state vector, delete set and
    // canonical::visible_content; then both apply every remaining row (and gap state) and still agree on all three,
    // and on has_pending() (the bytes of pending content are not compared: the same encoder writes both sides)
#[test] fn checkpoints_follow_the_policy()
    // with stats.encodes: for each checkpoint, the row bytes since the previous one are ≥ threshold(its state size)
    // and ≤ threshold(size at the encode before the one that wrote it, or the previous checkpoint's size) + that row's bytes;
    // and the encodes between two checkpoints are at most (row bytes between them / CHECKPOINT_FLOOR_BYTES) + 2 (no per-row encoding)
#[test] fn a_rev_gap_stops_the_pass()                                   // rows 1,2,4 → built_rev 2, stats.rev_gap
#[test] fn genesis_applies_rows_the_baseline_lacks_below_its_seq()      // baseline at seq 10 lacks row b (seq 8, the 0.9.x late-seq skip); genesis holds b; a room snapshot after genesis reports no gap
#[test] fn genesis_writes_baseline_entries_and_the_rollout_state_only_when_it_differs()
    // differing: exactly one entry with meta {"rollout": true}, and it has kind == EntryKind::Baseline, baseline_id None,
    // end_rev Some(G), started_at == ended_at == history_since, author None, sealed; equal content: no such entry
#[test] fn genesis_of_a_new_page_is_an_empty_checkpoint_at_rev_zero()
#[test] fn a_legacy_row_that_corrupts_genesis_is_skipped_and_no_checkpoint_is_written()
    // genesis_rev 3, hooks.corrupt_at_rev = Some(2) → Err(GenesisError::Corrupt{rev: 2}); again with skip_revs {2}
    // (hook still set) → Ok, stats.unapplied["skipped"] == 1, the genesis state lacks row 2's content
#[test] fn genesis_records_a_claim_per_parking_legacy_row()
    // legacy rows 1..3 where rows 1 and 3 park → claims revs [1, 3], authors None
#[test] fn genesis_reports_a_baseline_that_does_not_apply()
    // newest baseline crafted not to apply → Err(BaselineRejected{id}); called again with the older baseline's bytes
    // → Ok, and the genesis state equals a fresh replay of the older baseline plus the rows (nothing of the rejected one)
```

- [ ] **Step 3: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --test pass`
Expected: FAIL to compile.

- [ ] **Step 4: Implement `canonical.rs`, `pass.rs`, `genesis.rs`** per Interfaces and the algorithm above.

- [ ] **Step 5: Run them to verify they pass**

Run: `cargo nextest run -p knot-history`
Expected: PASS. A failing seed in `many_small_passes_equal_one_pass` means resume state is lost between passes: find what (claims, open session sets, gap states, past-gap set, `snapshot_checked_rev`, pending content in the checkpoint) before touching the assertion; if it is the pending content, take the pending fallback (Rulings) even though Task 1's test passed, and record why.

- [ ] **Step 6: Commit**

```bash
git add crates/knot-history
git commit -m "feat(history): canonical attribution, gap check, checkpoints and genesis"
```

---

### Task 5: Spike — replay throughput, checkpoint policy and the replay cap

**Files:**
- Create: `crates/knot-history/examples/replay_throughput.rs`
- Modify: `crates/knot-config/src/lib.rs` (fields, defaults, validation, tests); `crates/knot-history/src/pass.rs` only if Step 3 changes `CHECKPOINT_STATE_FRACTION`
- Test: `crates/knot-config/src/lib.rs` (`mod tests`)

**Interfaces:**
- Consumes: `Replayer`, `run_pass` (Tasks 1, 4).
- Produces: `knot_config::DEFAULT_HISTORY_REPLAY_MAX_BYTES: u64`; `Config::history_workers: u32` (default 1, `0` = builder off), `Config::history_replay_max_bytes: u64` (default the const), `Config::history_cpu_share: f64` (default `0.5`); `validate()` errors `"KNOT_HISTORY_REPLAY_MAX_BYTES must be >= 1048576"` and `"KNOT_HISTORY_CPU_SHARE must be > 0 and <= 1"`; the measured doc-memory factor F (a number in the commit message and the deviation notes, used by Tasks 10-11).

- [ ] **Step 1: Write the example**

The example installs a counting `#[global_allocator]` (a `System` wrapper keeping current and peak allocated bytes in atomics, with `reset_peak()`), so every workload reports peak heap above its starting level, not encoded size. `cargo run --release -p knot-history --example replay_throughput` prints, for each workload, rows/s, MiB/s, peak replay-doc bytes (`encode_state().len()`), peak heap and the factor peak heap ÷ replay input bytes:
1. typing: 3 clients, 100,000 rows of one to three characters each (≈ 150 B rows);
2. pastes: 300 rows of 100 KiB paragraphs, a third of them later deleted;
3. policy: a page grown to 1 MiB of state through `run_pass` (so the real checkpoint policy places the checkpoints), then a replay from the latest checkpoint at or before each of 20 heads to that head — prints the worst-case replay time, the number of checkpoints, their total bytes over the log's bytes, and `stats.encodes.len()`; it asserts that the checkpoint count is within ±1 of (log bytes ÷ the mean `threshold` over the run) and that `encodes.len() ≤ 2 × checkpoints + passes`;
4. memory, on two 4 MiB states, one grown by interleaved three-client one-character typing (unsquashed items), one by pastes: (a) one `run_pass` from a checkpoint with a 1 MiB prefix, 1 MiB of new rows, one late snapshot whose probe finds a gap, one snapshot whose probe is refused so `union` runs (`skip_underflow_case()`'s shape, or a hand-made refused probe if Task 1 found none), and a checkpoint emit; (b) one word count: `replay_window` with `KeepDeleted` over the same input plus `entry_word_counts` on a session holding a third of the rows. Peak heap ÷ the input bytes each read is the factor; F = the largest of the four (rounded up to an integer).
Use `ReplayMode::Gc` for 1-2 and `KeepDeleted` for 3 (the word-count replay).

- [ ] **Step 2: Run it and record the numbers**

Run: `cargo run --release -p knot-history --example replay_throughput`
Expected: four blocks of numbers. Paste them into the commit message and into Task 11's deviation notes, F included.

- [ ] **Step 3: Set the defaults by these rules**
  - `DEFAULT_HISTORY_REPLAY_MAX_BYTES` = the largest power of two ≤ min(the slower of workloads 1-2 in MiB/s × 2 s, (512 MiB − 256 MiB `WEB_ALLOWANCE`) ÷ (2 workers × F)), clamped to [8 MiB, 32 MiB] (so two workers fit the chart's 512 Mi limit next to the web server; since this one cap bounds build replays too (Rulings), it is also the largest page state plus rows a pass reads). If F > 16 the 8 MiB floor does not fit two workers: record it, and Task 11's README says that only one worker fits the default memory limit. Without numbers (example fails to run), stop and report: the cap must not be set without F.
  - The policy holds when workload 3's worst replay is under 500 ms. If it is not, set `CHECKPOINT_STATE_FRACTION = (1, 2)` (it governs both the encode trigger and the write condition, Rulings), rerun, check that the checkpoint count roughly doubled and that `checkpoints_follow_the_policy` still passes, and record the deviation (more checkpoint storage, about 2× the log instead of 1×).

- [ ] **Step 4: Write the failing config tests**

```rust
#[test] fn history_defaults() { let c = Config::default(); assert_eq!((c.history_workers, c.history_replay_max_bytes), (1, DEFAULT_HISTORY_REPLAY_MAX_BYTES)); }
#[test] fn history_workers_zero_disables_the_builder() { /* history_workers: 0 + a valid session key → validate() is Ok */ }
#[test] fn replay_cap_below_one_mib_is_invalid() { /* 1048575 → errs */ }
#[test] fn cpu_share_must_be_in_zero_one() { /* default 0.5; 0.0 and 1.5 → "KNOT_HISTORY_CPU_SHARE must be > 0 and <= 1"; 1.0 → Ok */ }
```

- [ ] **Step 5: Run, implement the fields, run again**

Run: `cargo nextest run -p knot-config`
Expected: FAIL, then PASS after adding the fields to `Config` (`crates/knot-config/src/lib.rs:57-141`), `Default` (`:143-179`) and `validate` (message in Global Constraints).

- [ ] **Step 6: Commit**

```bash
git add crates/knot-history/examples crates/knot-history/src crates/knot-config
git commit -m "feat(history): replay throughput spike sets the replay cap"
```

---

### Task 6: Schema — state, checkpoints and entries tables

**Files:**
- Create: `migrations/20261108120000_history_builder.sql`
- Modify: `crates/knot-storage/src/updates_store.rs` (new `rows_in_range` after `append_in_tx` at :368), `crates/knot-storage/src/lib.rs:60-63` (re-export)
- Modify: `crates/knot-storage/tests/migrations_apply.rs` (`expected` at :58-90; the helpers at :1816-1857 move out; new tests at the end), `crates/knot-storage/tests/updates.rs`
- Create: `crates/knot-test-support/src/migrate.rs` (moved helpers); Modify: `crates/knot-test-support/src/lib.rs` (`pub mod migrate;`)
- Modify: `e2e/support/reset.ts:45-77` (`TABLES`, `isLockTimeout`)

**Interfaces:**
- Produces: the three tables (Global Constraints; with the pending fallback from Task 1, `doc_history_checkpoints` also gets `pending_revs bigint[] NOT NULL DEFAULT '{}'`); `pub async fn rows_in_range(conn: &mut PgConnection, doc_id: Uuid, after_rev: i64, through_rev: Option<i64>, max_rows: i64, max_bytes: i64) -> Result<Vec<DocUpdate>, UpdatesStoreError>` — rows with `after_rev < rev ≤ through_rev` in rev order, at most `max_rows`, while the cumulative `octet_length(update_bytes)` stays ≤ `max_bytes` (stop before the row that would cross it; the first row alone when it exceeds `max_bytes` by itself, so a pass always gets a row; Rulings), mapped by the existing `doc_update`.

- [ ] **Step 1: Write the failing tests**

First move `migrator_up_to`, `migrate_up_to`, `migrate_all` and `bare_db` (`migrations_apply.rs:1816-1857`) unchanged into `knot_test_support::migrate` as `pub` functions (`sqlx::migrate!("../../migrations")` resolves the same from `crates/knot-test-support`), and import them in `migrations_apply.rs`; Task 7's builder tests use them too.

In `migrations_apply.rs`: make `expected` read, in this order, `"doc_history_baselines"`, `"doc_history_checkpoints"`, `"doc_history_entries"`, `"doc_history_heads"`, `"doc_history_state"` (the assert compares sorted names), and:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn history_builder_tables_cascade_with_their_page() {
    // page with a state row, a checkpoint, an entry (author = user, baseline_id = one of its baselines)
    // archive + purge the page (DocStore::archive, begin_purge, purge) → all three tables empty for it;
    // pg_indexes lists doc_history_entries_baseline_idx, and with 1,000 seeded entries of other pages, after
    // `ANALYZE doc_history_entries`, EXPLAIN of `DELETE FROM doc_history_entries WHERE baseline_id = <a literal id>`
    // names the index (run it inside a transaction with `SET LOCAL enable_seqscan = off` if the planner still
    // prefers a scan of so small a table; the assertion is that the index exists and is usable)
}

#[tokio::test(flavor = "multi_thread")]
async fn entries_need_an_end_rev_unless_baseline() {
    // INSERT kind 'session' with end_rev NULL → 23514; kind 'baseline' with end_rev NULL → ok
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn history_builder_migration_runs_under_live_0_9_10_traffic() {
    // bare_db(4); migrate_up_to(20261107120004); seed ws, user, 4 pages with rows (PgUpdatesStore::append)
    // loops (spawn_loop, as migrations_one_to_five_run_under_live_0_9_9_traffic at :2233 does):
    //   (i) one PgUpdatesStore::append loop per page; (ii) a page write (FOR UPDATE + UPDATE title) on page A;
    //   (iii) a users write: UPDATE users SET display_name = .. WHERE id = user; (iv) create + archive + purge a fresh page;
    //   (v) a stamped snapshot write (insert_stamped) on page B
    // run migrate_all while they run; stop; assert: every loop made progress after the migration,
    // no loop saw an error, pg_stat_database.deadlocks unchanged, the three tables exist
}
```

In `tests/updates.rs`: `rows_in_range_caps_rows_and_bytes` (5 rows of 10 bytes: `max_rows` 3 → revs 1-3; `max_bytes` 25 → revs 1-2 (the third would cross 25); `max_bytes` 30 → revs 1-3 (exactly 30); `max_bytes` 1 → rev 1 alone (a first row over the limit); `through_rev` Some(2) → revs 1-2; legacy unnumbered rows are not returned).

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-storage --test migrations_apply --test updates`
Expected: FAIL (tables missing, `rows_in_range` missing).

- [ ] **Step 3: Write the migration and `rows_in_range`**

Header comment: what the tables are for (builder watermark, lease and crash marker; derived checkpoints; timeline entries, ids only, names resolved at read time), why the lock helper comes first (FKs need SHARE ROW EXCLUSIVE on `documents`, `users`, `doc_history_baselines`, held while 0.9.10 pods serve), and that nothing else waits on an app lock.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-storage --test migrations_apply --test updates`
Expected: PASS.

- [ ] **Step 5: Add the three tables to `TABLES` in `e2e/support/reset.ts`** (next to `doc_history_heads`, so the `doc_history_*` run reads `doc_history_baselines`, `doc_history_checkpoints`, `doc_history_entries`, `doc_history_heads`, `doc_history_state`), and make `isLockTimeout` (`reset.ts:75-77`) also return true for `"deadlock detected"`: `TRUNCATE` takes `ACCESS EXCLUSIVE` table by table in list order, `doc_history_*` before `documents`, while a builder write transaction holds `documents` (row `FOR KEY SHARE`, so `ROW SHARE`) and then writes the history tables; a reset that holds the history tables and waits on `documents` deadlocks with it (40P01, raised before the 4 s lock timeout), and appenders already meet `doc_history_heads` the same way.

- [ ] **Step 6: Commit**

```bash
git add migrations/20261108120000_history_builder.sql crates/knot-storage crates/knot-test-support e2e/support/reset.ts
git commit -m "feat(history): tables for the history builder"
```

---

### Task 7: The builder

**Files:**
- Create: `crates/knot-history/src/store.rs`, `crates/knot-history/src/builder.rs`
- Modify: `crates/knot-history/src/lib.rs`, `crates/knot-server/Cargo.toml`, `crates/knot-server/src/main.rs:162-195` (the bus block returns a clone of `updates`) and `:275` (spawn after `state`, with its shutdown token), `Cargo.lock`
- Test: `crates/knot-history/tests/builder.rs`, `crates/knot-history/tests/builder_metrics.rs` (current-thread, `CountingRecorder` in `tests/common/metrics.rs`), `builder.rs` (`mod tests`)

**Interfaces:**
- Consumes: Tasks 1-6; `knot_storage::{UpdatesStore, rows_in_range, begin}`; `UpdatesStore::ensure_legacy_numbered` (`updates_store.rs:230`); `knot_test_support::migrate::{migrate_up_to, migrate_all, bare_db}` (Task 6).
- Produces:
  - `pub struct BuildLimits { pub max_rows: i64, pub max_row_bytes: i64, pub max_input_bytes: i64, pub words_max_input_bytes: i64, pub rows_budget: Duration, pub words_budget: Duration, pub lease: Duration, pub replay_timeout: Duration, pub words_timeout: Duration, pub checkpoint_floor: u64, pub debounce: Duration, pub cpu_share: f64, #[doc(hidden)] pub hooks: TestHooks }` with `BuildLimits::from_config(&knot_config::Config)` (the constants: `rows_budget = PASS_ROWS_BUDGET`, `words_budget = PASS_WORDS_BUDGET`, `lease = LEASE`, `debounce = DEBOUNCE`; `max_input_bytes` and `words_max_input_bytes` both `history_replay_max_bytes`; `cpu_share = history_cpu_share`; `checkpoint_floor = CHECKPOINT_FLOOR_BYTES`; `TestHooks::default()`); tests set `cpu_share = 1.0` (no pacing) unless they test pacing; `#[doc(hidden)] #[derive(Default, Clone)] pub struct TestHooks { pub hold_compute: Duration, pub hold_compute_doc: Option<Uuid>, pub hold_words: Duration, pub hold_before_release: Duration, pub replay: ReplayHooks, pub fail_words: bool, pub input_log: Option<Arc<Mutex<Vec<i64>>>> }` (`hold_compute`: `std::thread::sleep` inside the isolated pass or genesis compute, at `Stage::Idle` before the replay starts, of every page, or only of `hold_compute_doc` when that is set; `input_log`: `build_page` appends the byte size of every replay input it loaded (genesis attempt, pass, word count: the checkpoint or baseline, reapply and prefix rows, new rows, snapshots, gap states); `hold_words`: the same inside a word-count compute; `hold_before_release`: `tokio::time::sleep` on the builder task before a release that runs in its own transaction (step 10); `replay` → `PassInput::hooks` / `GenesisInput::hooks`; `fail_words`: the words compute returns `DiffError::Encode`). Production code never sets them.
  - `pub fn failure_backoff(failures: i32) -> chrono::Duration` (Global Constraints); `pub fn pacing_delay(compute: Duration, cpu_share: f64) -> Duration` (`compute × (1 / cpu_share − 1)`, zero for `cpu_share ≥ 1`; Rulings (CPU)).
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum PassResult { Built { built_rev: i64, rows: u64 }, Genesis { genesis_rev: i64 }, Clean, Busy, Gone, Deferred, Failed }` (`Clean`: the claim's clean check found nothing to do, no lease taken; `Busy`: another builder holds the page's row lock or lease, or a claim statement hit its lock timeout; `Deferred`: `retry_after` in the future or a newer `builder_version`).
  - `pub fn history_permits(workers: usize) -> Arc<tokio::sync::Semaphore>` (`workers ≥ 1` permits; `main.rs` creates it once and keeps a clone for M1.4's `AppState`, Rulings).
  - `pub struct Builder` with `pub fn new(pool: PgPool, updates: Arc<dyn UpdatesStore>, limits: BuildLimits, permits: Arc<Semaphore>) -> Builder` (a random `id: Uuid`, the lease owner), `pub fn id(&self) -> Uuid`, `pub async fn build_page(&self, doc_id: Uuid) -> PassResult` (one pass; errors are recorded, never returned; cancellation-safe: dropping the future leaves only the committed claim, which the lease and backoff cover, and the in-flight guard's `Drop` frees the page on this replica), `pub async fn tick(self: &Arc<Self>) -> usize` (one round of `dirty_candidates`, each built by a `build_page` task in a `JoinSet` that owns an `Arc<Builder>` clone, awaited; returns how many results were `Built` or `Genesis`; for tests, the spawned loop refills instead, Rulings), `pub async fn sweep(self: &Arc<Self>) -> usize` (`sweep_candidates` the same way; also sets `knot_history_builder_lag_revs` and `knot_history_given_up_pages`), `#[doc(hidden)] pub async fn dirty_candidates(&self) -> Vec<Uuid>` and `#[doc(hidden)] pub async fn sweep_candidates(&self) -> Vec<Uuid>` (the store queries with no ids excluded, for the integration tests, which cannot reach crate-private `store`), `pub fn spawn(self: Arc<Self>, shutdown: tokio_util::sync::CancellationToken) -> tokio::task::JoinHandle<()>` (one task, `tokio::join!` of two refilling loops: `dirty_candidates` every `TICK`, `sweep_candidates` and the gauges every `SWEEP`; Rulings; on `shutdown.cancelled()` both loops stop claiming, record the in-flight ids, `abort_all` their `JoinSet`s and run `release_on_shutdown(ids)` once, the graceful-shutdown ruling), `#[doc(hidden)] pub async fn release_on_shutdown(&self, ids: &[Uuid])` (exposed for the test).
  - `store.rs` (crate-private SQL): `claim`, `renew_lease`, `give_up`, `load_genesis_inputs`, `load_baseline_bytes`, `load_pass_inputs`, `input_bytes`, `write_pass`, `write_genesis`, `reset_for_rebuild`, `record_failure(…, action: FailureAction, timed_out: bool)` with `enum FailureAction { None, SkipRev(i64), SkipSnapshot(i64), SkipBaseline(i64), DropCheckpoint(i64), DropKeptGap(i64), Rebuild { drop_gap: Option<i64> } }` (`timed_out` with `None`: the second-timeout rule, step 7), `next_words_entry`, `take_words_entry`, `load_words_inputs`, `write_words`, `release`, `release_on_shutdown`, `dirty_candidates`, `sweep_candidates`, `max_lag`, `given_up_count`.

`build_page`, in order (transactions as Global Constraints, "Builder pass"):
1. Skip if this replica already builds the page (a `Mutex<HashSet<Uuid>>`; membership is an `InFlight` guard whose `Drop` removes the id, Rulings); take an owned permit (`permits.clone().acquire_owned()`). Every `run_isolated` call below gets the permit, wrapped with the stalled-thread state (Rulings), as its guard and its timeout (`replay_timeout`, or `words_timeout` in step 9), and is preceded by `renew_lease` (Global Constraints step 4; lease lost → `Failed`, no `record_failure`). When the guard does not come back (a timeout), `build_page` marks it timed out and calls `progress.cancel()` (Rulings: the frozen stage picks the action); for a pass or genesis compute the pass then records its failure and returns, for a word count step 9 applies. Either way the permit stays with the thread until it ends (Rulings). `build_page` sums the wall time of every compute whose guard came back, and after its last transaction (release, failure record or give-up) sleeps `pacing_delay(sum, limits.cpu_share)` before dropping the permit (Rulings (CPU)); the in-flight guard is held through the sleep.
2. `claim`: `Gone` / `Busy` / `Deferred` / `Clean` as Global Constraints (`given_up`, `deferred`, `leased` read as SQL booleans, never `retry_after` itself; the version lift applied first and bound as `$failures`, `$timeouts`, `$crashes` from Rust; `$lease` from `limits.lease`); `failures >= MAX_FAILURES` (reason `failures`) or `$crashes >= MAX_CRASHES` (reason `crash`) → `give_up` in its claim form → `Failed`.
3. `ensure_legacy_numbered` (`DocGone` → `Gone`; any other error → `record_failure` → `Failed`).
4. `builder_version < BUILDER_VERSION` with `genesis_rev` set → `reset_for_rebuild` (a lease-checked write transaction: delete the page's entries except `kind = 'erase'` and its checkpoints with `kind IN ('genesis', 'periodic')`, keeping every `gap` checkpoint (Rulings, gap states); reset `built_rev` (0), `genesis_rev` (NULL), `snapshot_checked_rev` (0), `parked`, keep `skip_revs` and `skip_baselines`; then `UPDATE doc_history_heads SET dirty = true`), then continue as a genesis. Every gap checkpoint is then above the new genesis's `G` (gaps are found above `snapshot_checked_rev`, which starts at `G`), so the genesis write leaves them alone.
5. Genesis (`genesis_rev` NULL): `load_genesis_inputs` in the read transaction: `G` = `SELECT coalesce(max(rev), 0) FROM doc_updates WHERE doc_id = $1 AND created_at < $2` (`$2` = `documents.history_since`); `input_bytes` = `octet_length` of the newest baseline's `state_bytes` not in `skip_baselines` (that is "the newest baseline" throughout this step; skipped ones still get their entries) plus `sum(octet_length(update_bytes))` of rows with `rev ≤ G`, in SQL; over `max_input_bytes` → `give_up(reason "too_large")` → `Failed`; every baseline's `id`, `snapshot_seq`, `created_at`, and the newest one's bytes; `rows_in_range(.., 0, Some(G), i64::MAX, i64::MAX)`; `skip_revs`. COMMIT. `build_genesis` under `run_isolated` with a fresh `Progress`; `Err(BaselineRejected { id })` → re-run the `input_bytes` check with the next older baseline's (not in `skip_baselines`) `octet_length` in place of the rejected one's (over `max_input_bytes` → `give_up(reason "too_large")` → `Failed`), then `load_baseline_bytes` of it (none left: start from no baseline) and call again; any other failure → the table below. Write transaction (lease-checked as Global Constraints step 5): `DELETE FROM doc_history_entries WHERE doc_id = $1 AND kind = 'baseline' AND baseline_id IS NOT NULL` (what a give-up at genesis inserted, Rulings), genesis checkpoint, entries, state (`built_rev = snapshot_checked_rev = genesis_rev = G`, `parked` = the genesis claims, `skip_revs` extended by `refused_revs`, `builder_version = BUILDER_VERSION`), release → `Genesis`.
6. Pass: `load_pass_inputs` in the read transaction, sizes first: the latest checkpoint with `rev ≤ built_rev` (`rev`, `kind`, `octet_length`; with the pending fallback also `pending_revs`); `base` = that length + the reapply rows' + the replay prefix's (rows after the checkpoint through `built_rev`), all `octet_length` sums (no gap checkpoint lies after the start, `PassInput`); `first` = the first new row's `octet_length` (0 when none); `base + first` over `max_input_bytes` → `give_up(reason "too_large")` → `Failed` (the rows themselves cannot fit; snapshots never cause a give-up, Rulings); the snapshot metadata (`snapshot_rev`, `created_at`, `octet_length(state_bytes)`) with `snapshot_checked_rev < snapshot_rev`, ascending, the newest per rev, at most `PASS_MAX_SNAPSHOTS`, each with `rows_to` = the `octet_length` sum of the rows with `built_rev < rev ≤ snapshot_rev` (0 for a late snapshot; one query, a running sum over the rows joined to the snapshot revs); and the kept gaps (Rulings, gap states: every checkpoint with `rev > built_rev`, all `kind = 'gap'`), at most `PASS_MAX_SNAPSHOTS`, ascending, with `rows_to` the same way. Walk snapshots and kept gaps merged in rev order (a kept gap before a snapshot at the same rev; `cap` = `max_input_bytes`, `taken` starts at 0):

   ```text
   snapshot:  if snapshot_rev ≤ built_rev + 1 and base + rows_to + size > cap → oversized: state_bytes None, no bytes fetched; go on
              else if base + rows_to + taken + size ≤ cap                    → take it: taken += size
              else                                                            → left out: the walk ends; through = snapshot_rev − 1
   kept gap:  if base + rows_to + taken + size ≤ cap                          → take it: taken += size
              else if rev = built_rev + 1 and nothing was taken               → give_up(reason "too_large") → Failed (its
                                                                                content cannot be dropped and cannot fit)
              else                                                            → left out: the walk ends; through = rev − 1
   after the walk: if cap − base − taken < first                              → through = built_rev (no rows this pass; only late
                                                                                snapshots were taken, and they are its progress)
   ```

   (A left-out snapshot or kept gap always has a taken one before it or lies above `built_rev + 1`, so every pass loads a snapshot or a row; the walk's `through` guarantees that the pass never builds row R without the kept gap at R.) Then the bytes: the checkpoint, reapply rows, replay rows, the taken snapshots and kept gaps (`rev`, `created_at`, `state_bytes`; `PassInput::kept_gaps` keeps those at or below the last row loaded); unless `through = built_rev`, `rows_in_range(.., built_rev, through, max_rows, max(1, min(max_row_bytes, cap − base − taken)))` (`through` `None` when nothing was left out; stop-before semantics, Task 6; every taken snapshot's rows were counted in the walk, so the loaded input never exceeds `cap`); `rows_exhausted` = the last row loaded (or `built_rev` when none) is the page's highest numbered rev, read in the same transaction, and no snapshot was left out; open sessions (`WHERE doc_id = $1 AND NOT sealed`); `parked`, `skip_revs`, `genesis_rev`, `snapshot_checked_rev`; `SELECT now()`. COMMIT.
7. `run_pass` under `run_isolated` with `rows_budget` from the limits, a fresh `Progress`, `checkpoint_floor` from the limits. Failures (passes and genesis alike) go to `record_failure` with the action of the table below. For a timeout the row is chosen by the stage `Progress::cancel()` froze, and only when that stage had been current for at least half of `replay_timeout`; otherwise the action is `None` (Rulings):

   | What failed | `FailureAction` |
   |---|---|
   | `ReplayError::Corrupt { rev }`, `GenesisError::Corrupt { rev }`; a panic or timeout at `Stage::Row(rev)` | `SkipRev(rev)`: `skip_revs = array_append(skip_revs, rev)` |
   | `ReplayError::CorruptSnapshot { snapshot_rev }`; a panic or timeout at `Stage::Probe(snapshot_rev)` | `SkipSnapshot(snapshot_rev)`: `snapshot_checked_rev = greatest(snapshot_checked_rev, snapshot_rev)`; counts `snapshots_unchecked_total{reason}` (`corrupt` / `panic` / `timeout`) |
   | `ReplayError::KeptGap { rev }`; a panic or timeout at `Stage::KeptGap(rev)` | `DropKeptGap(rev)`: `DELETE … WHERE doc_id = $1 AND rev = $rev AND kind = 'gap' AND rev > built_rev`, an ERROR log with the doc id and rev, `snapshots_unchecked_total{reason="kept_gap"}` (Rulings, gap states) |
   | `ReplayError::State`, `ReplayError::Prefix { rev }`; a panic or timeout at `Stage::Checkpoint` or `Stage::Prefix(rev)` in a pass | by the starting checkpoint `load_pass_inputs` chose (the latest at or below `built_rev`; its rev `cp` and kind come from there, never from the error or the stage, whose `rev` is the prefix row's): `periodic` → `DropCheckpoint(cp)`: `DELETE … WHERE doc_id = $1 AND rev = $cp AND kind = 'periodic'`; `genesis` → `Rebuild { drop_gap: None }`; `gap` → `Rebuild { drop_gap: Some(cp) }`, which also deletes that one gap checkpoint (Rulings); `Rebuild` sets `builder_version = 0`; never `SkipRev` (Rulings) |
   | a panic, or a timeout, at `Stage::Checkpoint` in genesis (the merge of baseline `id`) | `SkipBaseline(id)`: `skip_baselines = array_append(skip_baselines, id)` (second-timeout ruling) |
   | a timeout at `Stage::Emit(rev)` (pass or genesis) | `give_up(reason "timeout")` at once, no `record_failure` (Rulings) |
   | `GenesisError::Encode`; a panic at `Stage::Emit`; a panic or timeout at `Stage::Idle`; a timeout whose frozen stage had been current for less than half of `replay_timeout` | `None`; for a timeout, `timeouts = timeouts + 1`, and at `MAX_TIMEOUTS` the same transaction gives the page up (`give_up` after-claim form, reason `timeout`) |

   `ReplayError::Cancelled` / `GenesisError::Cancelled` never reach this table: they only arrive after the timeout was handled. `last_error` names the stage and rev (`row 7 panicked`, `replay timed out at snapshot 12`, …); a timeout also counts `knot_history_replay_timeouts_total{phase}` and logs at ERROR (`history replay timed out`). → `Failed`.
8. Write transaction: checkpoints (`ON CONFLICT (doc_id, rev) DO UPDATE SET kind = EXCLUDED.kind, state_bytes = EXCLUDED.state_bytes, state_vector = EXCLUDED.state_vector` only when `EXCLUDED.kind = 'gap'`, never `created_at`, so a kept gap re-emitted by a rebuild keeps the time it was first found; `run_pass` emits at most one per rev, so a batched insert never touches a row twice); entries (UPDATE by id, INSERT otherwise; every UPDATE sets `updated_at = now()`, which M1.4's view cache keys on; a session sealed in this pass gets `words_pending = true`); state (`built_rev`, `snapshot_checked_rev`, `parked`, `skip_revs = skip_revs || $refused_revs`); when `stats.rev_gap`: COMMIT without releasing, skip the words phase, then `record_failure("rev gap after <built_rev>", None, false)` (which matches: the lease is still ours) with an ERROR log → `Failed`; else, when no sealed session of the page has `words_pending`: release in the same transaction → `Built`.
9. Words phase, while `words_budget` lasts (measured from the phase's start), and at least one count when the pass reached the head (`rows_exhausted && !stopped_early`, Rulings), one sealed `words_pending` session at a time, oldest `end_rev` first (`next_words_entry`): `take_words_entry` (lease-checked transaction: `UPDATE doc_history_entries SET words_pending = false, updated_at = now() WHERE id = $1 AND words_pending`, so the count is attempted once, Rulings; lease lost → `Failed`); `input_bytes` of the latest checkpoint with `rev ≤ start_rev − 1` plus the gap checkpoints after it through `end_rev` and the rows through `end_rev` over `words_max_input_bytes` → skip (`too_large`); else `load_words_inputs` (that checkpoint, those gap checkpoints and rows, `skip_revs`) in a read transaction, then under `run_isolated` (`words_timeout`): `pass::replay_window` with `ReplayMode::KeepDeleted`, `end_rev` = the entry's, `merge_gap_at_end` = (`meta.gap_credit_rev` = `end_rev`: the session holds credits from the gap merge at its end, Rulings) and a fresh `Progress` (gap checkpoints merged through `merge_gap_state`, so a union-made gap checkpoint is taken; Rulings), `entry_word_counts` (`hooks.fail_words` → `DiffError::Encode`); `Ok` → `write_words` (lease-checked; `words_added`, `words_removed`, `updated_at = now()`; lease lost → `Failed`); `Err(DiffError)` → skip (`diff_error`); `WindowError::Corrupt` or `State` → skip (`corrupt`); `WindowError::GapState` → skip (`gap_state`); `Panicked` → skip (`panic`); `TimedOut` → `knot_history_replay_timeouts_total{phase="words"}` and skip (`timeout`), and the words phase ends (the permit stayed with the thread) and goes on to step 10. A skip leaves both counts NULL, keeps no crash marker beyond the claim's (the release resets it), never calls `record_failure` and never counts `knot_history_build_failures_total` (Rulings).
10. `release` in its own lease-checked transaction (Global Constraints step 7: page `FOR KEY SHARE`, state `FOR UPDATE`, `claimed_by = $me AND claimed_until > now()` or roll back → `Failed`, "lease lost", nothing written, no `record_failure`) → `Built`. This also covers a pass whose words phase wrote nothing (every count skipped as too large).
11. Metrics from `PassStats`, on this task: `knot_history_rows_built_total`, `knot_history_rows_unapplied_total{reason}`, `knot_history_rows_parked_total`, `knot_history_gap_total`, `knot_history_snapshots_unchecked_total{reason}`, `knot_history_replay_seconds{phase}` (`genesis` around step 5's compute, `build` around step 7's, `words` around each count), `knot_history_words_skipped_total{reason}`; `knot_history_build_failures_total` on every `Failed` except a given-up page and a lost lease at release; `knot_history_pages_given_up_total{reason}` in `give_up`; `knot_history_replay_threads_stalled` from the builder's stalled-thread counter (Rulings), here and in `sweep`.

`give_up` has two forms, both `UPDATE doc_history_state SET retry_after = 'infinity', builder_version = CASE WHEN genesis_rev IS NULL THEN $3 ELSE builder_version END, last_error = $2, claimed_by = NULL, claimed_until = NULL, updated_at = now()` (`$3` = `BUILDER_VERSION`), an ERROR log with the doc id and reason, and `knot_history_pages_given_up_total{reason}` once: **from the claim** (`failures >= MAX_FAILURES`, `$crashes >= MAX_CRASHES`), inside the claim's transaction, under its page `FOR KEY SHARE` and state `FOR UPDATE`, with `WHERE doc_id = $1` only (nobody holds the lease then: `claimed_by` is NULL or a crashed holder's expired id); **after a claim** (`too_large`, `timeout`; the second-timeout case runs it inside `record_failure`'s transaction), its own transaction (page `FOR KEY SHARE`, state `FOR UPDATE`) with `WHERE doc_id = $1 AND claimed_by = $me`. In both forms, when the state row's `genesis_rev IS NULL`, the same transaction then inserts the page's metadata-only baseline entries (`INSERT INTO doc_history_entries (doc_id, kind, author_id, started_at, ended_at, sealed, baseline_id) SELECT b.doc_id, 'baseline', NULL, b.created_at, b.created_at, true, b.id FROM doc_history_baselines b WHERE b.doc_id = $1 AND NOT EXISTS (SELECT 1 FROM doc_history_entries e WHERE e.baseline_id = b.id)`; Rulings). `record_failure`: Rulings ("a failed pass") plus the action's column change; its own failure is only logged.

`dirty_candidates`: `SELECT h.doc_id FROM doc_history_heads h LEFT JOIN doc_history_state s USING (doc_id) WHERE h.dirty AND NOT h.doc_id = ANY($2) AND (s.doc_id IS NULL OR ((s.retry_after IS NULL OR s.retry_after <= now()) AND (s.claimed_until IS NULL OR s.claimed_until < now()) AND s.builder_version <= $1)) AND (h.last_write_at < now() - $4 OR h.head_rev - coalesce(s.built_rev, 0) >= $5) ORDER BY h.last_write_at LIMIT $3` (`$2` the ids in flight on this replica, `$3` the free slots up to `CLAIM_LIMIT`, `$4` `limits.debounce` as an interval, `$5` `DEBOUNCE_MAX_LAG_REVS`; Rulings, debounce). `sweep_candidates`, each part `LIMIT 20`, excluding `$2`, and filtered by `<filter>` = `(s.retry_after IS NULL OR s.retry_after <= now()) AND (s.claimed_until IS NULL OR s.claimed_until < now()) AND s.builder_version <= $1` (the version term keeps old replicas off pages a newer builder owns during a rollout): (a) `SELECT e.doc_id FROM doc_history_entries e JOIN doc_history_state s USING (doc_id) WHERE NOT e.sealed AND e.ended_at < now() - interval '21 minutes' AND <filter> GROUP BY e.doc_id ORDER BY min(e.ended_at)`; (b) `SELECT doc_id FROM doc_history_state s WHERE builder_version < $1 AND (genesis_rev IS NOT NULL OR retry_after = 'infinity') AND (s.retry_after IS NULL OR s.retry_after <= now() OR s.retry_after = 'infinity') AND (s.claimed_until IS NULL OR s.claimed_until < now())` (older builds, given-up ones included: the claim lifts their give-up); (c) `SELECT s.doc_id FROM doc_history_state s WHERE s.genesis_rev IS NOT NULL AND <filter> AND EXISTS (SELECT 1 FROM doc_snapshots n WHERE n.doc_id = s.doc_id AND n.snapshot_rev > s.snapshot_checked_rev)` (late snapshots, Rulings). `max_lag`: `SELECT coalesce(max(h.head_rev - coalesce(s.built_rev, 0)), 0) FROM doc_history_heads h LEFT JOIN doc_history_state s USING (doc_id) WHERE h.dirty AND (s.retry_after IS NULL OR s.retry_after <> 'infinity')`. `given_up_count`: `SELECT count(*) FROM doc_history_state WHERE retry_after = 'infinity'`.

`main.rs`: inside the `PgBus::connect` arm, clone `updates` before `Rooms::new` and return it from the block as `history_updates: Option<Arc<dyn UpdatesStore>>` beside `bus`, `rooms_v2` and `reindex_rx`; after `state` is built (next to the reindex spawn, `main.rs:275`), inside `if let (Some(pool), Some(history_updates)) = (state.pool.clone(), history_updates)`: when `cfg.history_workers == 0` log `tracing::warn!("history builder disabled")` and spawn nothing; else `let history_permits = knot_history::history_permits(cfg.history_workers as usize);` (kept in scope with a `// M1.4: share with AppState for the view endpoint` comment; no `AppState` field in M1.3), then `Arc::new(knot_history::Builder::new(pool, history_updates, knot_history::BuildLimits::from_config(&cfg), history_permits.clone())).spawn(state.shutdown.clone());` and `tracing::info!("history builder spawned")` (spawned after `state` because the shutdown token lives in `AppState`, `lib.rs:93`). Task 9 extends the Helm upgrade test so CI proves this wiring on a real 0.9.10 → 0.9.11 upgrade.

- [ ] **Step 1: Write the failing tests** in `crates/knot-history/tests/builder.rs` (multi-thread `fresh_db` tests; pages and rows through `PgDocStore::create` and `PgUpdatesStore::append` with yrs updates from `tests/common`; rows with an explicit `created_at` and `doc_updates` rewrites as Global Constraints says: raw INSERT without `rev`, `SET LOCAL knot.history_rewrite = 'on'` for DELETE/UPDATE)

```rust
#[tokio::test(flavor = "multi_thread")] async fn a_new_page_gets_genesis_then_sessions()
    // create page, append 3 rows by A within 5 min → build_page = Genesis{0}, then Built{3}; one open session (A, revs 1..3); heads.dirty false;
    // state failures 0, retry_after NULL, claimed_by NULL; append a 4th row by A, build → the same entry id, end_rev 4, updated_at advanced
#[tokio::test(flavor = "multi_thread")] async fn sessions_seal_and_get_word_counts()
    // A rows at t, A row at t + 30 min → first session sealed with words_added = typed words, words_pending false; second open, words NULL
#[tokio::test(flavor = "multi_thread")] async fn an_existing_page_gets_baseline_entries_and_a_genesis_with_its_late_seq_row()
    // bare_db; migrate_up_to(20261102120000) (before M1.1: 20261104120000 copies doc_snapshots into doc_history_baselines once,
    // and 20261104120001 stamps history_since only on pages that exist then); page with legacy rows (raw INSERT of doc_id, seq,
    // update_bytes, created_at) and a doc_snapshots row (snapshot_seq 10) lacking row b (seq 8), as
    // history_migration_copies_snapshots_and_stamps_history_since (migrations_apply.rs:1274-1300) seeds them; migrate_all;
    // A appends rows after history_since; then a stamped snapshot at a rev above G holding the room's state, b included;
    // build → one baseline entry per baseline, A's session, no gap entry, stats gaps 0 (the snapshot above G IS probed),
    // and exactly one rollout entry (genesis holds b, the newest baseline does not): kind 'baseline', baseline_id NULL,
    // meta.rollout = true, end_rev = G, ended_at = history_since — the entry set is asserted exactly
#[tokio::test(flavor = "multi_thread")] async fn a_stamped_snapshot_with_unlogged_content_becomes_a_gap_entry()
#[tokio::test(flavor = "multi_thread")] async fn a_late_snapshot_is_gap_checked_by_the_sweep()
    // build to the head (dirty false); insert a stamped snapshot at the head holding an extra paragraph (insert_stamped);
    // sweep() returns ≥ 1 and the page gets one gap entry at the head rev; separately, the same with one more append and a tick
    // instead (limits.debounce zero)
#[tokio::test(flavor = "multi_thread")] async fn a_rebuild_keeps_gap_content_whose_snapshot_is_gone()
    // rows 1..4 by A; a stamped snapshot at rev 2 holding an extra paragraph X; build until Clean → one gap entry at rev 2
    // (record its started_at); DELETE that doc_snapshots row (thinning); UPDATE builder_version = 0; append row 5; build until
    // Built → exactly one gap entry, at rev 2, started_at as recorded, ins_ids = X's insertions; the gap checkpoint at 2 and
    // every later checkpoint hold X (canonical::visible_content); A's sessions equal a twin page's that never had X; no gap
    // entry at any other rev. Also: a second gap checkpoint (a snapshot at rev 3 with paragraph Y, also deleted) survives a
    // Rebuild { drop_gap: Some(2) } of the first (truncate the rev-2 checkpoint as in a_corrupt_checkpoint_is_dropped_and_rebuilt_past,
    // append, build → Failed; build until Built): Y's gap entry at rev 3 is kept, X's content is gone (its snapshot was deleted)
#[tokio::test(flavor = "multi_thread")] async fn a_rebuild_that_cannot_merge_a_kept_gap_drops_only_that_gap()
    // two gap checkpoints at revs 2 and 3 as above; builder_version = 0 and hooks.replay.corrupt_kept_gap_at = Some(2) → the
    // rebuild's genesis, then a pass → Failed; the gap checkpoint at 2 is deleted, the one at 3 kept; clear the hook, reset
    // retry_after, build until Built → one gap entry, at rev 3 (snapshots_unchecked_total{reason="kept_gap"} 1, builder_metrics)
#[tokio::test(flavor = "multi_thread")] async fn a_late_gap_credit_is_counted_in_its_session()
    // backdate history_since (and created_at) by 2 h; B's row at history_since (rev 1, types "x"); B's row at history_since +
    // 1 min (rev 2) typing "drained words here" inside a paragraph P that no row carries (it parks); A's row at now (rev 3);
    // build until Clean (B's session over rev 1 is sealed by the idle rule); a stamped snapshot at rev 3 holding P and B's text
    // → the late probe at built_rev 3 drains B's claim: a new B session with start_rev = end_rev = 3 and meta.gap_credit_rev 3;
    // backdate its ended_at by 22 min, build until Clean → words_added == 3 (not 0, not NULL).
    // Variant (the gap at the session's last row): B's own row at now (rev 4, types "own"), the snapshot at rev 4 → B's open
    // session gets the credit at rev 4 (end_rev 4, gap_credit_rev 4); backdate, build → words_added == 4
#[tokio::test(flavor = "multi_thread")] async fn late_snapshots_count_toward_the_replay_cap()
    // rows: one ~40 KiB paragraph (the shared state), then small rows so revs r1 < r2 < r3 ≤ head exist; build to the head;
    // then 3 stamped snapshots at r1, r2, r3 (distinct revs: the loader keeps one per rev), each the room state at its rev
    // plus its own small extra paragraph (≤ 200 bytes). Before each pass the test reads base (octet_length of the latest
    // checkpoint ≤ built_rev plus the replay prefix, the store's own sum) and the remaining snapshots' sizes, asserts
    // base + the two smallest remaining > base + the largest remaining + 512, and builds with a fresh Builder whose
    // max_input_bytes = base + the largest remaining snapshot + 512 → three passes, each probing exactly one snapshot:
    // snapshot_checked_rev = r1, then r2, then r3; three gap entries (all at the head rev) in the end, never given up
#[tokio::test(flavor = "multi_thread")] async fn an_oversized_snapshot_is_left_unchecked_not_given_up()
    // build to the head; a stamped snapshot at the head whose size is about 0.6 × max_input_bytes while base is about 0.6 ×
    // max_input_bytes (base + snapshot over the cap, base + any row under it); append a row; build → Built (not Failed),
    // retry_after NULL, snapshot_checked_rev ≥ the snapshot's rev, no gap entry, snapshots_unchecked_total{reason="too_large"}
    // counted (builder_metrics)
#[tokio::test(flavor = "multi_thread")] async fn small_passes_build_what_one_pass_builds()
    // twin pages with the same 40 rows; Builder with max_rows 3 and checkpoint_floor 256 vs 5_000 and the default floor;
    // compare entries as Task 4 does, word counts included
#[tokio::test(flavor = "multi_thread")] async fn appends_racing_passes_settle_clean()
    // one append loop, one stamped-snapshot loop (insert_stamped of the room state every 100 ms) and two Builders'
    // build_page loops (checkpoint_floor 256, so writes carry checkpoints) on one page for 3 s; stop the loops, build
    // until Clean; heads.dirty false, built_rev = head_rev, entries equal a single fresh build of the same rows on a twin
    // page; pg_stat_database.deadlocks unchanged, and no append took longer than APPEND_STATEMENT_TIMEOUT (8 s)
#[tokio::test(flavor = "multi_thread")] async fn two_builders_never_build_one_page_at_once()
    // two Builders on one pool, build_page concurrently 20×: every result Built/Genesis/Clean/Busy/Deferred, final entries
    // equal a single build's, pg_stat_database.deadlocks unchanged
#[tokio::test(flavor = "multi_thread")] async fn a_clean_page_is_not_rebuilt_by_a_late_claimant()
    // Builder A builds a page to the head (Genesis, then Built); record its checkpoints' created_at and the state row's
    // updated_at; Builder B build_page → Clean; nothing changed (same checkpoints, same updated_at, claimed_by NULL,
    // failures 0); then a stamped snapshot above snapshot_checked_rev → B build_page → Built (the clean check sees it);
    // and an open session idle for 22 min (backdated ended_at) → Built (sealed)
#[tokio::test(flavor = "multi_thread")] async fn a_first_build_racing_another_builder_is_busy_not_failed()
    // Builder A with hooks.hold_compute 6 s (longer than any lock timeout the builder sets) starts the page's genesis; after 200 ms Builder B
    // build_page returns Busy or Deferred within 1.5 s; after A returns Genesis: failures 0, last_error NULL
#[tokio::test(flavor = "multi_thread")] async fn a_pass_that_lost_its_lease_writes_nothing()
    // genesis and a first pass by B; append 2 rows; Builder A with hold_compute 3 s starts build_page; during the hold:
    // UPDATE doc_history_state SET claimed_until = now() - interval '1 s', retry_after = NULL; Builder B build_page → Built and releases;
    // A returns Failed; B's entries (ids, updated_at), checkpoints, built_rev and parked are unchanged, and last_error, failures and
    // claimed_by are what B's release left (A's record_failure matched no row: WHERE claimed_by = A).
    // Variant: rows that seal a session; A with hold_words 3 s; expire A's lease while it counts, B builds → A's write_words writes
    // nothing (that session's counts stay NULL: attempted once), A returns Failed, B's state unchanged.
    // Release variant: rows that seal a session; A with words_max_input_bytes = 1 (its count is skipped as too large, so its release
    // runs in its own transaction) and hooks.hold_before_release 3 s; during that hold: expire A's lease, and Builder B (hold_compute
    // 5 s) claims the page after `UPDATE doc_history_state SET claimed_until = now() - interval '1 s', retry_after = NULL` (without clearing `retry_after`, A's crash marker would make B's claim `Deferred`); A's release rolls back → A returns Failed; while B still computes, the state row shows B's claim
    // (claimed_by = B.id(), claimed_until, failures and retry_after as B's claim wrote them) and heads.dirty is unchanged by A
#[tokio::test(flavor = "multi_thread")] async fn a_long_pass_holds_no_lock_on_the_page()
    // hooks.hold_compute 3 s; while the pass computes: DocStore::move_to of the page under another parent, DocStore::archive,
    // restore from archive, and an owner-grant removal attempt on the page each complete within 500 ms; the pass then writes (archived pages keep building) or returns Gone after a purge
#[tokio::test(flavor = "multi_thread")] async fn a_purge_racing_builder_passes_never_deadlocks()
    // 30 pages with rows; spawn build_page loops; archive + purge each page meanwhile;
    // no 40P01 anywhere (pg_stat_database.deadlocks unchanged), every purge succeeds, no rows left in the three tables
#[tokio::test(flavor = "multi_thread")] async fn a_pass_that_dies_mid_compute_defers_the_page()
    // Builder A (hold_compute 5 s): spawn build_page, abort the task after 500 ms (the process "dies": the claim is committed,
    // nothing else); Builder B build_page → Deferred; state failures 1, retry_after > now(), claimed_by = A.id();
    // UPDATE retry_after and claimed_until to now() - 1 s → B build_page → Genesis (its claim counted crashes 1: A's
    // 'pass interrupted'); failures 0, crashes 0, claimed_by NULL;
    // append a row → Builder A (the aborted one) build_page → Built after its hold, not skipped as in flight (the aborted
    // future's in-flight guard was dropped)
#[tokio::test(flavor = "multi_thread")] async fn a_hung_replay_times_out_and_backs_off_the_page()
    // Builder with history_permits(1), replay_timeout 1 s, hold_compute 4 s with hold_compute_doc = Some(P) (Q's compute is not
    // held): build_page(P) (its genesis) → Failed within 2 s;
    // P: last_error "replay timed out", retry_after > now(), claimed_by NULL, skip_revs empty, timeouts 1, not given up (the
    // hold is at Stage::Idle: nothing to blame); build_page(Q) on another page waits for the
    // permit and returns Genesis only after the held thread ended (≥ 3.5 s after the start), proving the permit rode the thread
#[tokio::test(flavor = "multi_thread")] async fn a_row_held_just_past_the_timeout_never_skips_the_next_row()
    // genesis built; rows 1..3; replay_timeout 1 s, hooks.replay.sleep_at_rev = Some((2, 1.3 s)) → Failed; skip_revs == {2}
    // (the frozen stage), never {3}; wait 1 s for the thread: skip_revs still {2}, built_rev unchanged (the cancelled thread
    // wrote nothing). Variant: sleep_at_rev = Some((2, 0.4 s)) after a 0.8 s hold_compute (the timeout falls at Row(2),
    // current for 0.2 s < half the timeout) → Failed with skip_revs empty, retry_after > now()
#[tokio::test(flavor = "multi_thread")] async fn a_replay_that_hangs_while_encoding_a_checkpoint_is_given_up()
    // new page; replay_timeout 500 ms, hooks.replay.sleep_at_emit = Some(1.5 s) (the genesis checkpoint's encode) → Failed;
    // SELECT retry_after = 'infinity' → true, last_error names the checkpoint encode, skip_revs empty
    // (builder_metrics: pages_given_up_total{reason="timeout"} 1)
#[tokio::test(flavor = "multi_thread")] async fn a_hang_with_nothing_to_blame_is_given_up_at_its_second_timeout()
    // two Builders A and B, each with history_permits(1), replay_timeout 500 ms, hold_compute 3 s with hold_compute_doc = P
    // (Stage::Idle, every attempt); A.build_page(P) → Failed, timeouts 1, retry_after finite; UPDATE retry_after = now() - 1 s;
    // B.build_page(P) → Failed, SELECT retry_after = 'infinity' → true, last_error names the timeout, timeouts 2
    // (builder_metrics: pages_given_up_total{reason="timeout"} 1); a third Builder build_page(P) → Deferred;
    // after A's held thread ended (sleep 3 s), A.build_page(Q) on another page → Genesis (A's replica is not stuck);
    // a release resets the count: a page with timeouts = 1 that then builds normally has timeouts 0
#[tokio::test(flavor = "multi_thread")] async fn a_baseline_whose_merge_hangs_is_skipped()
    // a fresh page with two doc_history_baselines rows (both valid states, newest id N); replay_timeout 500 ms,
    // hooks.replay.sleep_at_baseline = Some((N, 2 s)) → Failed; skip_baselines = {N}, skip_revs empty, not given up;
    // UPDATE retry_after = now() - 1 s; build (hook still set) → Genesis, the genesis state equals the older baseline plus the
    // rows; both baseline entries listed; the rollout entry compared with the older baseline
#[tokio::test(flavor = "multi_thread")] async fn a_page_that_crashes_the_process_three_times_is_given_up()
    // three times: Builder X_i (hold_compute 5 s) spawns build_page(P), the task is aborted after 300 ms (the process "dies":
    // last_error stays 'pass interrupted'), then UPDATE claimed_until = now() - 1 s, retry_after = NULL;
    // after the first and second abort a claim counts crashes 1, then 2 (it proceeds); the claim after the third abort
    // → Failed, retry_after = 'infinity', last_error names the crash (builder_metrics: pages_given_up_total{reason="crash"} 1)
#[tokio::test(flavor = "multi_thread")] async fn a_graceful_shutdown_is_not_counted_as_a_crash()
    // Builder A (hold_compute 5 s) spawned with spawn(token) over a page P made dirty; once P's state row shows
    // claimed_by = A.id(), cancel the token and await the JoinHandle (≤ 2 s) → P: claimed_by NULL, last_error 'shutdown',
    // failures 0 (the claim's +1 taken back), retry_after NULL; Builder B build_page(P) → Genesis with crashes 0
#[tokio::test(flavor = "multi_thread")] async fn a_build_is_paced_by_the_cpu_share()
    // cpu_share 0.5, hold_compute 1 s: build_page of a new page returns Genesis after ≥ 2 s (1 s compute + 1 s pacing);
    // a second Builder sharing the same history_permits(1) and calling build_page(Q) 100 ms in returns only after ≥ 2 s
    // (the pacing held the permit); with cpu_share 1.0 the same build returns in < 1.8 s
#[tokio::test(flavor = "multi_thread")] async fn a_page_built_at_an_older_version_with_maxed_failures_is_rebuilt_not_given_up()
    // build a page (Genesis, Built); UPDATE builder_version = 0, failures = MAX_FAILURES, retry_after = now() - 1 s (not given up)
    // → build_page = Genesis (the claim lifted the counts and rebuilt the page), then Built; retry_after NULL, failures 0,
    // builder_version 1, entries equal a twin page's. Same with failures = MAX_FAILURES - 1 and a Rebuild request
    // (builder_version 0) → Genesis
#[tokio::test(flavor = "multi_thread")] async fn a_build_that_outlasts_one_lease_renews_it()
    // lease 3 s, replay_timeout 2 s, words_timeout 2 s.
    // (a) a fresh page with two doc_history_baselines rows, the newest with state_bytes '\xffffff' (rejected), the older a
    //     valid state; hold_compute 1.6 s → two genesis computes (> 3 s) → Genesis, not Failed; failures 0;
    // (b) rows that seal a session; hold_compute 1.6 s and hold_words 1.6 s → one pass plus its forced count (> 3 s) →
    //     Built, the session's words_added not NULL;
    // in both, 2.5 s in: UPDATE doc_history_state SET retry_after = NULL (A's crash marker would otherwise make any claim
    // Deferred, renewed or not), then a second Builder's build_page returns Busy (the lease was renewed: claimed_until > now())
#[tokio::test(flavor = "multi_thread")] async fn a_large_row_after_small_rows_waits_for_its_own_pass()
    // input_log set; cap C = max_input_bytes = 64 KiB, checkpoint_floor 256; a first row of ≈ 0.3 C (built: genesis, pass);
    // then 3 small rows totalling ≈ 0.3 C and one row of ≈ 0.5 C; build_page → Built with built_rev at the last small row
    // (the large row would cross C); every input_log entry ≤ C; the next build_page → Failed, given up as too_large (base is
    // now ≈ 0.6 C, as checkpoint or prefix, and base + the large row > C: SELECT retry_after = 'infinity'), input_log ≤ C throughout
#[tokio::test(flavor = "multi_thread")] async fn an_unreachable_snapshot_ahead_is_not_loaded()
    // input_log set; C = 64 KiB; built to rev r with base ≈ 0.5 C; append rows r+1..r+10 of ≈ 0.03 C each; a stamped
    // snapshot at r+10 of ≈ 0.4 C (base + its rows + it > C); build_page → Built, rows through r+9 at most, the snapshot's
    // bytes not loaded (input_log entry ≈ base + the rows, < base + 0.4 C); every input_log entry ≤ C; build until Clean →
    // the snapshot is probed or left unchecked as too_large, never given up, and input_log ≤ C throughout
#[tokio::test(flavor = "multi_thread")] async fn a_legacy_page_given_up_at_genesis_still_lists_its_baselines()
    // a fresh page with two doc_history_baselines rows; max_input_bytes = 1 → build_page = Failed (too_large at genesis);
    // the page has exactly two entries, kind 'baseline', baseline_id set, author_id NULL, ended_at = the baselines'
    // created_at; then a normal Builder and UPDATE failures = 0, timeouts = 0, crashes = 0, retry_after = NULL → Genesis; still exactly one entry per baseline
#[tokio::test(flavor = "multi_thread")] async fn a_page_that_keeps_failing_is_given_up()
    // UPDATE failures = MAX_FAILURES (claimed_by NULL, retry_after NULL: as record_failure leaves it) → build_page = Failed;
    // retry_after = 'infinity', builder_version 1, last_error names it, claimed_by NULL (the claim form matched without a
    // claimed_by predicate); a second build_page → Deferred; Builder::dirty_candidates() skips it;
    // UPDATE failures = 0, timeouts = 0, crashes = 0, retry_after = NULL (the README's re-enable) → Built
    // (builder_metrics: pages_given_up_total{reason="failures"} is 1 after both build_page calls)
#[tokio::test(flavor = "multi_thread")] async fn a_given_up_page_is_rebuilt_after_a_version_bump()
    // build a page; give it up (UPDATE failures = MAX_FAILURES, build → Failed); UPDATE builder_version = 0 (an older build);
    // Builder::sweep_candidates() returns it; build_page → Genesis (the claim lifted the give-up, the page rebuilt), then Built;
    // failures 0, retry_after NULL, entries equal a twin page's.
    // Failure right after the lift: give it up again (failures = MAX_FAILURES) and UPDATE builder_version = 0; a Builder with
    // replay_timeout 200 ms and hold_compute 1 s: build_page → Failed (the rebuild's genesis timed out) with failures = 1, not
    // MAX_FAILURES + 1, and retry_after finite; UPDATE retry_after = now() - 1 s; a normal Builder build_page → Genesis
    // (retried, not given up again)
#[tokio::test(flavor = "multi_thread")] async fn a_page_over_the_input_cap_is_given_up()
    // limits.max_input_bytes = 1 on a new page with rows: build_page → Genesis{0} (an empty genesis reads 0 bytes);
    // build_page → Failed; retry_after 'infinity', last_error "too large", only the genesis checkpoint exists, builder_version 1
#[tokio::test(flavor = "multi_thread")] async fn a_row_that_always_panics_is_skipped()
    // build genesis; hooks.replay.panic_at_rev = Some(2) over rows 1..3 → Failed, skip_revs = {2}, failures 1;
    // reset retry_after → Built{built_rev: 3}; row 2's content in no entry; the panic hook stays set and is never hit again
#[tokio::test(flavor = "multi_thread")] async fn a_prefix_row_that_panics_is_never_skipped()
    // checkpoint_floor 256; build rows 1..8 so a periodic checkpoint sits at c with built_rev > c (assert it); record entries;
    // append row 9; a Builder with hooks.replay.panic_at_rev = Some(c + 1) (a prefix row now) → Failed; skip_revs empty, the
    // periodic checkpoint at c deleted (DropCheckpoint), entries unchanged; reset retry_after, clear the hook → Built{9};
    // entries equal a twin page's (row c + 1's content still in its author's entry); same with corrupt_at_rev = Some(c + 1)
#[tokio::test(flavor = "multi_thread")] async fn a_legacy_row_that_always_panics_in_genesis_is_skipped()
    // a legacy page (rows before history_since, G = 3); hooks.replay.panic_at_rev = Some(2) → Failed, skip_revs = {2},
    // no genesis checkpoint, genesis_rev NULL; reset retry_after → Genesis{3}, then Built; the genesis state lacks row 2
#[tokio::test(flavor = "multi_thread")] async fn a_snapshot_whose_gap_merge_corrupts_is_skipped_not_a_row()
    // rows 1..3, a stamped snapshot at rev 2 with an extra paragraph; hooks.replay.corrupt_merge_at = Some(2) → Failed,
    // skip_revs empty, snapshot_checked_rev ≥ 2; reset retry_after → Built{3}; every row's content is in its author's entry,
    // no gap entry, snapshots_unchecked_total{reason="corrupt"} counted (builder_metrics)
#[tokio::test(flavor = "multi_thread")] async fn a_snapshot_whose_probe_panics_is_skipped()
    // the same with hooks.replay.panic_probe_at = Some(2): Failed, skip_revs empty, snapshot_checked_rev ≥ 2, then Built{3}
#[tokio::test(flavor = "multi_thread")] async fn a_corrupt_checkpoint_is_dropped_and_rebuilt_past()
    // checkpoint_floor 256 and 3 passes so a periodic checkpoint exists; truncate its state_bytes to half their length
    // (`substring(state_bytes from 1 for octet_length(state_bytes) / 2)`: decoding fails cleanly; random garbage can carry
    // varint counts that make yrs 0.27.3 allocate gigabytes, id_set.rs:91, an abort catch_unwind cannot catch);
    // append a row; build → Failed, that checkpoint gone; reset retry_after → Built, entries equal a twin page's.
    // Gap case: a stamped snapshot with an extra paragraph makes a gap checkpoint; build past it; truncate it the same way;
    // append, build → Failed, builder_version 0, that gap checkpoint deleted (Rebuild { drop_gap }); build until Built → exactly
    // one gap entry holding the paragraph (its snapshot still exists and is probed again), entries equal a twin's
    // Choosing the checkpoint: with periodic checkpoints at c1 < c2 ≤ built_rev, truncating c1 → no failure (the pass starts
    // at c2); truncating c2 → DropCheckpoint deletes c2, not a row at the failing prefix row's rev
#[tokio::test(flavor = "multi_thread")] async fn a_rev_gap_fails_the_page_with_backoff()
    // rows 1..3; DELETE rev 2 (in a transaction with SET LOCAL knot.history_rewrite = 'on') → Failed; built_rev 1, failures 1,
    // last_error "rev gap after 1", retry_after in the future, claimed_by NULL (the write did not release, so record_failure matched);
    // Builder::dirty_candidates() skips it
#[tokio::test(flavor = "multi_thread")] async fn a_legacy_count_mismatch_fails_the_page_with_backoff()
    // a page with legacy_rows > 0 (seeded as legacy_page in crates/knot-storage/tests/updates.rs:518); number its legacy rows
    // by hand to other revs (UPDATE under knot.history_rewrite = 'on') → Failed; failures = 1, retry_after in the future;
    // Builder::dirty_candidates() skips it
#[tokio::test(flavor = "multi_thread")] async fn the_dirty_flag_stays_while_rows_or_word_counts_remain()
    // max_rows 2 over 5 rows: dirty true until built_rev = 5
#[tokio::test(flavor = "multi_thread")] async fn word_counts_wait_for_the_budget()
    // rows by A at t, by B at t, then rows by A and B at t + 30 min: one pass of a Builder with words_budget = 0 (rows_budget
    // the default) seals two sessions; it reached the head, so it counts exactly one (the older end_rev) anyway: that one has
    // counts, the other keeps words_pending true, heads.dirty true; a second pass of the same Builder (no new rows: run_pass
    // decodes nothing) counts the other → both counted, dirty false. A words_budget-0 Builder with max_rows 1 over 3 new rows
    // (not at the head) counts none in that pass. A Builder with rows_budget = 0 over 3 new rows that end at the head
    // processes one row per pass, and the pass that consumes the last row seals and counts (it is not stopped early)
#[tokio::test(flavor = "multi_thread")] async fn a_session_spanning_a_union_gap_gets_its_word_count()
    // (doc_state, update, room_state) = skip_underflow_case(): row 1 = doc_state by A at t (a full state is an update), row 2 =
    // update by A at t + 1 min (refused: in skip_revs after the pass), a stamped snapshot at rev 2 = room_state (union gap
    // checkpoint at 2), row 3 by A at t + 2 min typing "four new words here", row 4 by A at t + 30 min (seals);
    // build until Clean → A's first session (revs 1..3) has words_added not NULL and ≥ 4 (Task 4's
    // a_window_over_a_union_gap_checkpoint_merges_it shows the plain merge_state is refused there)
#[tokio::test(flavor = "multi_thread")] async fn a_word_count_over_the_cap_is_skipped()
    // words_max_input_bytes = 1 seals a session → words_added/removed NULL, words_pending false, heads.dirty false, built_rev = head
#[tokio::test(flavor = "multi_thread")] async fn a_word_count_that_fails_is_skipped_not_retried()
    // hooks.fail_words = true → counts NULL, words_pending false, PassResult Built, failures 0, built_rev = head, dirty false
#[tokio::test(flavor = "multi_thread")] async fn a_newer_builder_version_is_left_alone_and_an_older_one_rebuilt()
    // set builder_version 2 → Deferred, entries untouched; set 0 and skip_revs {2} → reset_for_rebuild: heads.dirty true,
    // genesis_rev NULL, skip_revs still {2}, the genesis and periodic checkpoints gone and the page's gap checkpoint kept (a
    // snapshot with an extra paragraph was probed before); rebuild: entries rebuilt equal (the gap entry once, at its rev),
    // an 'erase' entry kept
#[tokio::test(flavor = "multi_thread")] async fn the_sweep_seals_idle_sessions_on_clean_pages()
    // also: a page in backoff (retry_after future) and a page with builder_version 2 are not returned by Builder::sweep_candidates()
#[tokio::test(flavor = "multi_thread")] async fn ticks_visit_seeded_pages_oldest_first()
    // limits.debounce zero; three dirty pages with heads.last_write_at set to now() - 3 h, - 1 h, - 2 h (UPDATE) →
    // Builder::dirty_candidates() == [3 h, 2 h, 1 h] in that order (the assertion is on the query, not on build order: tick()
    // spawns every candidate and they race for permits); a page in backoff (retry_after future) is not listed
#[tokio::test(flavor = "multi_thread")] async fn a_page_edited_seconds_ago_waits_unless_far_behind()
    // limits.debounce 10 s: a page appended to just now is not in dirty_candidates(); with heads.last_write_at = now() - 11 s
    // it is; a page with last_write_at = now() and head_rev − built_rev = 200 (DEBOUNCE_MAX_LAG_REVS) is; sweep_candidates()
    // still returns a just-written page with a late snapshot
```

In `builder.rs` (`mod tests`): `pacing_delay_scales_with_the_share` (`pacing_delay(1 s, 0.5)` = 1 s, `(1 s, 0.25)` = 3 s, `(1 s, 1.0)` = 0, `(0, 0.5)` = 0).

In `builder.rs` (`mod tests`): `failure_backoff_is_capped` (`failure_backoff(0)` = 30 s, `(1)` = 60 s, `(7)` = 1 h, `(100)` = 1 h, `(i32::MAX)` = 1 h).

In `tests/builder_metrics.rs` (`#[tokio::test]`, current-thread, the local `CountingRecorder` with gauges):

```rust
#[tokio::test] async fn a_pass_records_its_metrics()
    // page P: 3 good rows by A at t, one undecodable row, one row whose dependency never arrives (parks), then a row by A at
    // t + 30 min (seals A's first session); genesis → replay_seconds{phase="genesis"} recorded; before the next build the lag
    // gauge is read by sweep() while P has no stamped snapshot yet (so the sweep's late-snapshot part does not build it):
    // builder_lag_revs == head_rev − built_rev; then insert a stamped snapshot with an extra paragraph at the head;
    // one build_page → rows_built_total 6, rows_unapplied_total{reason="undecodable"} 1, rows_parked_total 1, gap_total 1,
    // replay_seconds{phase="build"} and {phase="words"} recorded (the session counted under the normal cap); sweep() → lag 0;
    // page Q (a second page, its own Builder with words_max_input_bytes = 1): rows sealing one session, build until Clean →
    // words_skipped_total{reason="too_large"} 1;
    // page R: a rev gap → build_failures_total 1; then UPDATE failures = MAX_FAILURES, retry_after = NULL (the rev gap's marker
    // would otherwise defer the claim) → pages_given_up_total{reason="failures"} 1, build_failures_total still 1; a second
    // build_page of R → Deferred, pages_given_up_total still 1;
    // sweep() → given_up_pages 1 and builder_lag_revs 0 although R is dirty and behind (given-up pages are not in the lag)
#[tokio::test] async fn an_oversized_page_counts_as_too_large()
    // max_input_bytes = 1: Genesis{0}, then Failed → pages_given_up_total{reason="too_large"} 1
#[tokio::test] async fn snapshot_failures_and_timeouts_are_counted()
    // hooks.replay.corrupt_merge_at → snapshots_unchecked_total{reason="corrupt"} 1;
    // an oversized snapshot (an_oversized_snapshot_is_left_unchecked_not_given_up's setup) → {reason="too_large"} 1;
    // timeout: build the page's genesis with a normal Builder first, append a row, then a Builder with replay_timeout 200 ms
    // and hold_compute 1 s → replay_timeouts_total{phase="build"} 1, replay_threads_stalled 1 right after build_page returns,
    // and 0 after the held thread ended (sleep 1.2 s, then sweep(), which sets the gauge from the builder's counter);
    // a word count that times out (hold_words 1 s, words_timeout 200 ms) → replay_timeouts_total{phase="words"} 1,
    // words_skipped_total{reason="timeout"} 1, build_failures_total unchanged, PassResult Built;
    // a checkpoint encode that hangs (a_replay_that_hangs_while_encoding_a_checkpoint_is_given_up's setup) →
    // pages_given_up_total{reason="timeout"} 1 (a second page with a_hang_with_nothing_to_blame_is_given_up_at_its_second_timeout's
    // setup → 2); a kept gap that does not merge (a_rebuild_that_cannot_merge_a_kept_gap_drops_only_that_gap's setup) →
    // snapshots_unchecked_total{reason="kept_gap"} 1; a page given up for crashes (UPDATE crashes = MAX_CRASHES - 1,
    // last_error = 'pass interrupted', then build_page) → pages_given_up_total{reason="crash"} 1
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --test builder --test builder_metrics && cargo nextest run -p knot-history --lib builder`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `store.rs`, `builder.rs` and the `main.rs` wiring** per Interfaces.

- [ ] **Step 4: Run them to verify they pass**

Run: `cargo nextest run -p knot-history`
Expected: PASS.

- [ ] **Step 5: Full Rust suite**

Run: `make lint && make test.rust`
Expected: PASS, including `crates/knot-server/tests/history_integration.rs` (the old drawer) unchanged.

- [ ] **Step 6: Commit**

```bash
git add crates/knot-history crates/knot-server/Cargo.toml crates/knot-server/src/main.rs Cargo.lock
git commit -m "feat(history): background builder of timeline entries"
```

---

### Task 8: `history_author_view` and the contributors API

**Files:**
- Create: `crates/knot-history/src/people.rs`
- Modify: `crates/knot-history/src/lib.rs`, `crates/knot-server/src/routes/api/contributors.rs` (whole handler, 48 lines)
- Test: `people.rs` (`mod tests`), `crates/knot-server/tests/contributors_integration.rs`

**Interfaces:**
- Produces: `pub const ANONYMIZED_SQL: &str = "false"`; `#[derive(Debug, Clone)] pub struct HistoryPerson { pub id: Uuid, pub display_name: String, pub anonymized: bool }`; `#[derive(Debug, Clone, PartialEq, Serialize)] pub struct AuthorView { pub key: Option<Uuid>, pub name: Option<String>, pub former: bool }`; `pub fn history_author_view(person: &HistoryPerson) -> AuthorView` (anonymized → `{key: null, name: null, former: true}`; else `{key: id, name, former: false}`; there is no "unknown" `AuthorView`: an absent author is JSON `null`, Task 9); `pub async fn load_people(pool: &PgPool, ids: &[Uuid]) -> Result<HashMap<Uuid, HistoryPerson>, sqlx::Error>` (`SELECT u.id, u.display_name, {ANONYMIZED_SQL} AS anonymized FROM users u WHERE u.id = ANY($1)`).
- Contributors response: `{"created_by": {"id", "display_name", "former"}, "created_at", "contributors_since", "contributors": [{"user_id", "display_name", "former", "first_edited_at", "last_edited_at"}]}`, id and name fields from `history_author_view` (null when former), built by `pub fn byline_view(b: &knot_storage::DocByline, people: &HashMap<Uuid, HistoryPerson>) -> serde_json::Value` in `people.rs` (a person missing from `people`, only possible when a user row is deleted between the two reads, keeps the byline's own id and name with `former: false`). The route never returns a former member's user id, email or avatar (architecture, READ PATH).

- [ ] **Step 1: Write the failing tests**

```rust
// people.rs
#[test] fn a_former_member_has_no_id_or_name() {
    let p = HistoryPerson { id, display_name: "Ann".into(), anonymized: true };
    assert_eq!(history_author_view(&p), AuthorView { key: None, name: None, former: true });
}
#[test] fn a_member_is_shown_by_id_and_name() { /* history_author_view(&p) with anonymized false → key Some(id), name Some("Ann") */ }
#[test] fn the_byline_hides_former_members() {
    // DocByline with creator c and contributors [c, d], both anonymized in `people`, plus member m (not anonymized)
    let v = byline_view(&byline, &people);
    assert_eq!(v["created_by"], json!({"id": null, "display_name": null, "former": true}));
    assert_eq!(v["contributors"][0]["user_id"], json!(null));
    assert_eq!(v["contributors"][0]["display_name"], json!(null));
    assert_eq!(v["contributors"][0]["former"], json!(true));
    assert_eq!(v["contributors"][2], json!({"user_id": m, "display_name": "Max", "former": false, "first_edited_at": .., "last_edited_at": ..}));
    let s = v.to_string();
    assert!(!s.contains(&c.to_string()) && !s.contains(&d.to_string()) && !s.contains("Ann"));
    // a person missing from `people` keeps the byline's own id and name with former false
}
```

In `contributors_integration.rs`, extend the existing byline test: the JSON has `"former": false` on `created_by` and on every contributor, and otherwise equals today's. Also update `byline_still_names_a_former_member` (`:961-985`, an exact `json!` comparison that breaks otherwise): add `"former": false` to its expected contributor and rename it `byline_still_names_someone_who_left_the_workspace`, with a comment that leaving the workspace keeps `former: false`; `former` means anonymized (M4).

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --lib people && cargo nextest run -p knot-server --test contributors_integration`
Expected: FAIL.

- [ ] **Step 3: Implement `people.rs` and route the byline through it** (the route keeps `ContributorStore::byline` for order and times, loads the people for the creator and contributors with `load_people` on `state.pool`, and returns `Json(byline_view(&b, &people))`; the handler builds no JSON of its own).

- [ ] **Step 4: Run them to verify they pass**

Run: same as Step 2.
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-history/src crates/knot-server/src/routes/api/contributors.rs crates/knot-server/tests/contributors_integration.rs
git commit -m "feat(history): resolve history authors in one place, also for the byline"
```

---

### Task 9: `GET /api/docs/{id}/history/entries`

**Files:**
- Create: `crates/knot-history/src/list.rs`
- Modify: `crates/knot-history/src/lib.rs`, `crates/knot-server/src/routes/api/history.rs` (new handler `entries`), `crates/knot-server/src/routes/api/docs.rs:92-103` (route)
- Modify: `crates/knot-server/src/routes/api/markdown.rs:535` and `:555` (`meta: Some(json!({"mode": "replace"}))` / `Some(json!({"mode": "append"}))`, Rulings)
- Modify: `.github/workflows/helm-upgrade.yaml` ("Verify the upgraded release" step, after assertion 4c at :386-403)
- Modify: `crates/knot-server/tests/history_labels_integration.rs:501` and `:513` (`markdown_import_is_labelled_import` expects the new meta: `assert_labelled(&appended, UpdateSource::Import, fx.bob.id, Some(json!({"mode": "append"})))` and `assert_labelled(&replaced, UpdateSource::Import, fx.alice.id, Some(json!({"mode": "replace"})))`; `assert_labelled` compares `row.meta` exactly, `:295`; the workspace import at `:575` stays `None`)
- Test: `list.rs` (`mod tests`, cursor), `history.rs` (`mod tests`, `readable_sources`), `crates/knot-server/tests/history_entries_integration.rs` (new), `crates/knot-server/tests/markdown_import_integration.rs` (`replace_records_the_importing_user` at :330 and `append_into_non_empty_doc_keeps_both` at :256 also assert the row's `meta`: `SELECT meta FROM doc_updates WHERE doc_id = $1 AND source = 'import' ORDER BY rev DESC LIMIT 1` is `{"mode": "replace"}` / `{"mode": "append"}`)

**Interfaces:**
- Consumes: `people::{load_people, history_author_view}` (Task 8); `knot_docs::AclCache::access` (`crates/knot-docs/src/cache.rs:156`) and `DocAccess::can_read`.
- Produces:
  - `#[derive(Debug, Clone, PartialEq, Eq)] pub enum Cursor { Log { end_rev: i64, id: i64 }, BaselineStart, Baseline { ended_at: DateTime<Utc>, id: i64 } }` with `pub fn encode(&self) -> String` (URL-safe base64 without padding of `L.<end_rev>.<id>`, `B.start` or `B.<unix micros>.<id>`) and `pub fn decode(s: &str) -> Result<Cursor, CursorError>`.
  - `#[derive(Debug, Clone, PartialEq)] pub struct EntryRow { pub id: i64, pub kind: EntryKind, pub author_id: Option<Uuid>, pub started_at: DateTime<Utc>, pub ended_at: DateTime<Utc>, pub end_rev: Option<i64>, pub sealed: bool, pub words_added: Option<i32>, pub words_removed: Option<i32>, pub meta: serde_json::Value }` (`end_rev` builds the `Log` cursor and is not rendered).
  - `pub struct HistoryPage { pub history_since: DateTime<Utc>, pub history_complete: bool, pub catching_up: bool, pub unavailable: bool, pub entries: Vec<EntryRow>, pub next_cursor: Option<Cursor> }`.
  - `pub async fn list_entries(pool: &PgPool, doc_id: Uuid, cursor: Option<Cursor>, limit: i64) -> Result<Option<HistoryPage>, sqlx::Error>` (`None` when the page row is gone). Log section (skipped entirely when the cursor is `BaselineStart` or `Baseline`): `kind <> 'baseline' AND (end_rev, id) < ($c_rev, $c_id) ORDER BY end_rev DESC, id DESC LIMIT limit + 1`; when it yields `limit + 1` rows, the page is its first `limit` and `next_cursor` is a `Log` cursor of the last. Otherwise the log section is exhausted and the baseline section (`kind = 'baseline' ORDER BY ended_at DESC, id DESC`, after the cursor when it is a `Baseline` one, from the top otherwise) fills `need = limit − log rows` with `LIMIT need + 1`; when it returns more than `need` rows, `next_cursor` is `BaselineStart` if `need = 0` (the log section ended exactly at the page boundary: the extra row is not on this page) and else a `Baseline` cursor of the last row on the page; otherwise `None`. `history_complete = documents.created_at >= history_since`; `unavailable` and `catching_up` come from one query, never decoding `retry_after` (Global Constraints): `SELECT d.created_at >= d.history_since AS history_complete, coalesce(s.retry_after = 'infinity', false) AS unavailable, (NOT coalesce(s.retry_after = 'infinity', false) AND ((s.doc_id IS NULL AND coalesce(h.dirty, false)) OR (s.doc_id IS NOT NULL AND s.genesis_rev IS NULL) OR coalesce(s.built_rev, 0) < coalesce(h.head_rev, 0) - $2)) AS catching_up FROM documents d LEFT JOIN doc_history_state s ON s.doc_id = d.id LEFT JOIN doc_history_heads h ON h.doc_id = d.id WHERE d.id = $1` (`$2` = `CATCHING_UP_REVS`; genesis not built yet, also during a rebuild: an empty or partial list is not final; a page with neither a heads nor a state row is not catching up).
  - `pub fn referenced_docs(entries: &[EntryRow]) -> Vec<Uuid>` (`meta.source_doc_id` of copy entries, `meta.template_doc_id` of template entries, deduplicated).
  - `pub fn render_page(page: &HistoryPage, people: &HashMap<Uuid, HistoryPerson>, readable: &HashSet<Uuid>) -> serde_json::Value` — the response JSON (Global Constraints) with `meta` per kind: `session` → `{"accepted_by": [AuthorView]}` when any, else `{}`; `restore` → `{"target_ended_at", "target_entry_id", "fallback"}` (null when absent; both M1.2's `{target_snapshot_seq, target_ended_at}` and M2's `{target_entry_id, target_rev, target_ended_at}` shapes); `copy` → `{"source_doc_id", "source_title", "version_at"}` if the source is in `readable` (`version_at` null when absent: today's copy rows carry only `source_doc_id`, `source_title`, `knot-storage/src/duplicate.rs:142-145`; M2 adds it), else `{"source_hidden": true}`; `template` → `{"template_doc_id", "template_title"}` if readable, else `{"template_hidden": true}`; `baseline` → `{"rollout": bool}`; `import` → `{"mode"}` (`"append"` or `"replace"`, any other value or absent → null, Rulings); `gap`, `erase` → `{}`. `author` is JSON `null` when `author_id` is NULL or the id is missing from `people` (`history_author_view` is never called without a person); an `accepted_by` id missing from `people` renders as `null` in that array.
- Handler: `pub async fn entries(State, Path(doc_id), Query<EntriesQuery { cursor: Option<String>, limit: Option<String> }>, req) -> Response`: `require_editor`; a bad cursor, or a `limit` that does not parse as `i64` or is outside 1..=`MAX_LIMIT` → 400 `history.bad_request` (`limit` is parsed in the handler: axum's `Query` would answer a non-number with a plain-text 400); `list_entries` (`None` → 404 `doc.not_found`); `load_people` for authors and `accepted_by`; `readable = readable_sources(&referenced_docs(..), state.acl.as_ref().map(|acl| move |id| acl.access(ctx.workspace_id, id, ctx.user_id)))`, with `pub(crate) async fn readable_sources<F, Fut>(ids: &[Uuid], access: Option<F>) -> HashSet<Uuid> where F: Fn(Uuid) -> Fut, Fut: Future<Output = Result<Option<DocAccess>, ResolveError>>` in `history.rs`: an id is readable only for `Ok(Some(a)) if a.can_read()`; `access` being `None` (no ACL cache), `Ok(None)` and `Err(_)` all mean not readable, `None` and `Err` also logged at warn (fail closed, Rulings); 200 with `Cache-Control: private, no-store`. Route: `.route("/api/docs/{id}/history/entries", get(crate::routes::api::history::entries))` next to the old history routes.

- [ ] **Step 1: Write the failing tests**

`list.rs`: `cursor_round_trips_and_rejects_garbage` (all three variants round-trip; `"!!"` (not base64) and the URL-safe base64 of `"L.1"`, `"Q.1.2"`, `"L.a.1"`, `"B.x.1"`, `"B.start.1"`, and `"B.9223372036854775807.1"` / `"B.-9223372036854775808.1"` (micros outside chrono's range: `DateTime::from_timestamp_micros` returns `None`, mapped to an error, never unwrapped) → errors), and:

```rust
#[test]
fn render_page_rebuilds_meta_per_kind() {
    // inputs carry every allowlisted key plus a stray "secret": "x" key and, for sessions, "suggestion_ids"
    // session with accepted_by [u1, u2] (u2 anonymized in `people`) →
    //   meta == {"accepted_by": [{"key": u1, "name": "Ann", "former": false}, {"key": null, "name": null, "former": true}]}
    // session with no accepted_by → {}
    // restore, M1.2 shape {target_snapshot_seq: 7, target_ended_at: t} → {"target_ended_at": t, "target_entry_id": null, "fallback": null}
    // restore, M2 shape {target_entry_id: 5, target_rev: 9, target_ended_at: t} → {"target_ended_at": t, "target_entry_id": 5, "fallback": null}
    // restore_fallback entry (meta.fallback = true) → "fallback": true
    // baseline {rollout: true} → {"rollout": true}; baseline {} → {"rollout": false}
    // import {mode: "append", secret: "x"} → {"mode": "append"}; import {} → {"mode": null}; import {mode: "drop"} → {"mode": null}
    // gap, erase with stray keys → {}
    // a gap entry and a baseline entry (author_id None) → "author": null (JSON null, not an object); a session whose author_id
    // is missing from `people` → "author": null
    // and for every rendered entry: no key outside its kind's allowlist, and the serialized page never contains
    // "secret", "suggestion_ids", "gap_credit_rev", "target_snapshot_seq" or u2's id
}
```

`history.rs` (`mod tests`): `readable_sources_fails_closed` — ids a, b, c, d with a closure returning `Ok(Some(editor access))` for a, `Ok(Some(viewer access, archived))` for b (`can_read` false), `Ok(None)` for c and `Err(ResolveError::…)` (any variant) for d → `{a}`; with `access: None` → `{}`.

`history_entries_integration.rs` (seeding as `history_integration.rs:36-80`, entries built with `knot_history::Builder::build_page` after appends):

```rust
#[tokio::test(flavor = "multi_thread")] async fn editors_list_entries_and_viewers_are_refused()
    // Editor 200, Owner 200, Viewer 403 acl.editor_required, no session 401; response has Cache-Control "private, no-store";
    // archive the page: Editor 200, Owner 200, Viewer 404 (the archived-page Ruling);
    // a restricted page R (owner O only) with a built session: workspace Editor E without a grant on R → 403 acl.no_grant,
    // and the body holds no entry data (no "entries" key, neither O's id nor name)
#[tokio::test(flavor = "multi_thread")] async fn entries_page_newest_first_across_both_sections()
    // fresh_db pages are created after history_since, so backdate: UPDATE documents SET created_at = history_since - interval '1 day';
    // insert 2 doc_history_baselines rows (yrs states) and legacy rows created before history_since (raw INSERT), then build;
    // 3 sessions after that; limit 2 → [s3, s2], next; → [s1, rollout]; → [baseline2, baseline1], next_cursor null; history_complete false.
    // A twin page with 2 sessions: limit 2 → [s2, s1] with a non-null next_cursor (BaselineStart); → [rollout, baseline2]; → [baseline1], null
#[tokio::test(flavor = "multi_thread")] async fn a_bad_limit_is_a_400()
    // limit=abc, limit=0, limit=201 → 400 history.bad_request (JSON error body)
#[tokio::test(flavor = "multi_thread")] async fn an_open_session_is_listed_in_progress_without_counts()
#[tokio::test(flavor = "multi_thread")] async fn a_bad_cursor_is_a_400()
#[tokio::test(flavor = "multi_thread")] async fn catching_up_is_set_while_the_builder_lags()
    // new page, 600 rows appended, no build → catching_up true, entries [];
    // a page with 3 rows and no build (no state row, heads dirty) → catching_up true; after its build → false;
    // a page never edited (no heads row, no state row) → catching_up false, entries [];
    // rebuild: build a page, UPDATE builder_version = 0, start build_page with hooks.hold_compute 2 s in the background; while it
    // holds (after reset_for_rebuild, before genesis is written) → catching_up true; after it returns Genesis → false (a page of
    // a few rows: head_rev − built_rev is far below 500);
    // the 500-rev rule alone: a page with genesis built and one pass to the head (state row, genesis_rev not NULL); append 501
    // rows (no build) → head_rev − built_rev = 501 → catching_up true, unavailable false; then UPDATE doc_history_state SET
    // built_rev = head_rev − 500 → catching_up false (the boundary, and that the rule compares state.built_rev with heads.head_rev);
    // given up: UPDATE retry_after = 'infinity' on a dirty page → unavailable true, catching_up false
#[tokio::test(flavor = "multi_thread")] async fn copy_and_template_sources_are_hidden_from_readers_who_cannot_see_them()
    // restricted template T (only owner can read); page P created from T (POST /api/docs/from-template/{T}) by the owner;
    // editor E of P (workspace editor, not granted on T) lists P: template entry meta == {"template_hidden": true}, body contains neither T's id nor title;
    // the owner sees template_doc_id and template_title. Same for a duplicate C of a restricted page S: the copy is restricted
    // too with only its actor as owner (knot-server/src/duplicate.rs:10), so first grant E editor on C (not on S); E lists C:
    // copy meta == {"source_hidden": true}, the owner sees source_doc_id and source_title; and after purging the source,
    // hidden for everyone. The template must have content (an empty template writes no row, Rulings). (The no-ACL-cache arm
    // is unit-tested on readable_sources: require_doc_role_mw answers 500 before the handler when AppState::acl is None.)
#[tokio::test(flavor = "multi_thread")] async fn restore_and_accepted_suggestion_entries_carry_their_meta()
    // drive the real routes as history_labels_integration.rs does (snapshot_restore_is_labelled_restore,
    // accepted_suggestion_credits_the_suggester): owner O, editor B; B types (a snapshot exists), O restores it through
    // POST /api/docs/{id}/history/{seq}/restore; B creates a suggestion and O accepts it (POST …/suggestions/{sid}/accept);
    // build_page until Clean; GET …/history/entries as O →
    // a "restore" entry with author.key = O and meta.target_ended_at == that snapshot's created_at (RFC 3339, as the
    // route writes it), meta.fallback null; a "session" entry with author.key = B and meta.accepted_by ==
    // [{"key": O, "name": O's display name, "former": false}]
#[tokio::test(flavor = "multi_thread")] async fn the_old_snapshot_list_still_answers()
    // GET /api/docs/{id}/history still returns the SnapshotMeta array
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --lib list && cargo nextest run -p knot-server --lib routes::api::history && cargo nextest run -p knot-server --test history_entries_integration --test markdown_import_integration --test history_labels_integration`
Expected: FAIL.

- [ ] **Step 3: Implement `list.rs`, the handler and the route** per Interfaces.

- [ ] **Step 4: Run them to verify they pass**

Run: same as Step 2, then `cargo nextest run -p knot-server --test history_integration`
Expected: PASS.

- [ ] **Step 5: Extend the Helm upgrade test**

In `.github/workflows/helm-upgrade.yaml`, after assertion 4c (the task toggle) and before the success `echo`, add assertion 5: poll `api GET "/api/docs/$DOC/history/entries"` into `/tmp/entries.json` every 2 s for up to 60 s until `jq -e '.catching_up == false and ([.entries[] | select(.kind == "session")] | length) >= 1'` holds (the toggle's `task` row builds a session; the builder ticks every 2 s); on timeout `::error::the history builder built no session for the canary page within 60 s after the upgrade` plus `cat /tmp/entries.json`. Then assert with `jq -e`: `.history_complete == true` (the canary was created by 0.9.10, after `history_since`), `.unavailable == false`, at least one entry with `.kind == "import"` (the Markdown replace through 0.9.10 and the append after the upgrade are `import` rows) and one with `.kind == "import" and .meta.mode == "append"` (the append after the upgrade, tagged by 0.9.11), and every `session` entry has a non-null `.author.key` (rows by the setup session's user). Each failure gets its own `::error::` line naming what was missing, in the style of assertions 1-4. This is CI's proof that `main.rs` spawns the builder and that the migration plus the builder work on real 0.9.10 data.

Run: `actionlint .github/workflows/helm-upgrade.yaml` if installed, else `python3 -c "import yaml,sys; yaml.safe_load(open(sys.argv[1]))" .github/workflows/helm-upgrade.yaml`
Expected: no errors (the job itself runs in CI, triggered by `migrations/**`).

- [ ] **Step 6: Commit**

```bash
git add crates/knot-history/src crates/knot-server/src/routes/api crates/knot-server/tests/history_entries_integration.rs crates/knot-server/tests/markdown_import_integration.rs crates/knot-server/tests/history_labels_integration.rs .github/workflows/helm-upgrade.yaml
git commit -m "feat(history): list a page's history entries"
```

---

### Task 10: Metrics, `history-stats` and the Helm values

**Files:**
- Create: `crates/knot-history/src/sizes.rs`
- Modify: `crates/knot-history/src/lib.rs`, `crates/knot-server/src/main.rs` (spawn the sampler next to the builder, also skipped when `history_workers == 0`), `crates/knot-obs/src/metrics.rs` (`fn describe_history_metrics()` called from `init` after the CRDT block, :54-82, and a unit test), `crates/knot-server/src/history_stats.rs` (fields), `deploy/helm/knot/values.yaml` (after `archive:` at :133), `deploy/helm/knot/values.schema.json` (after `"archive"` at :138), `deploy/helm/knot/templates/configmap.yaml` (after :19)
- Test: `crates/knot-history/tests/sizes.rs`, `crates/knot-server/tests/history_stats_integration.rs`

**Interfaces:**
- Produces: `pub const HISTORY_TABLES: [&str; 7] = ["doc_updates", "doc_snapshots", "doc_history_baselines", "doc_history_heads", "doc_history_state", "doc_history_checkpoints", "doc_history_entries"]`; `pub async fn sample_table_sizes(pool: &PgPool) -> Result<bool, sqlx::Error>` (false when another replica holds `SIZE_LOCK_KEY`; else sets `knot_history_table_bytes{table}` from `pg_total_relation_size` for each and returns true); `pub fn spawn_size_sampler(pool: PgPool)` (every `SIZE_SAMPLE_EVERY`, first run after one minute). `HistoryStats` gains `checkpoint_rows: i64`, `checkpoint_bytes_total: i64` (`sum(octet_length(state_bytes) + octet_length(state_vector))`), `entry_rows: i64`, all 0 when `to_regclass('doc_history_checkpoints')` is NULL (a database before 0.9.11; checked in its own query first).

- [ ] **Step 1: Write the failing tests**

```rust
// tests/sizes.rs (current-thread, CountingRecorder-style local recorder that also records gauges)
#[tokio::test] async fn table_sizes_are_sampled_once_per_lock_holder()
    // hold SIZE_LOCK_KEY on a detached connection → Ok(false), no gauge; release → Ok(true), a gauge per table > 0
// history_stats_integration.rs
// extend history_stats_reports_rows_bytes_and_daily_rates: one checkpoint of 100 + 7 bytes and two entries → checkpoint_rows 1, checkpoint_bytes_total 107, entry_rows 2
// crates/knot-obs/src/metrics.rs (mod tests)
#[test] fn history_metrics_are_described()
    // a local recorder (metrics::with_local_recorder) that collects describe_* names: describe_history_metrics() registers
    // exactly the names in Global Constraints (4 gauges, 10 counters including M1.2's knot_history_legacy_mismatch_total,
    // 1 histogram), each with its kind
#[test] fn replay_seconds_has_buckets_up_to_the_replay_timeout()
    // REPLAY_BUCKETS_SECONDS is ascending and its last edge is ≥ 270 (REPLAY_TIMEOUT), unlike LATENCY_BUCKETS_SECONDS
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo nextest run -p knot-history --test sizes && cargo nextest run -p knot-server --test history_stats_integration && cargo nextest run -p knot-obs`
Expected: FAIL.

- [ ] **Step 3: Implement** `sizes.rs`, the stats fields (printed like the others), the sampler spawn, and one `describe_*` per metric in Global Constraints. Helm: `history: { workers: 1, replayMaxBytes: <DEFAULT_HISTORY_REPLAY_MAX_BYTES>, cpuShare: 0.5 }` with a comment ("Page history is built in the background on every replica. workers bounds concurrent replays per replica; cpuShare is the share of one CPU core each worker may use on average (it sleeps after each page in proportion to its compute time): unpaced, one worker can use a whole core, the default resources.limits.cpu, which it shares with the web server; raise either only with CPU headroom; workers: 0 turns the builder off on that replica (an incident switch; history then waits). replayMaxBytes caps the input of every replay, builds and word counts alike (the page's state plus the edits and snapshots it reads); a replay holds about <F> times its input in memory (measured), so peak builder memory is about workers × <F> × replayMaxBytes, which must fit resources.limits.memory next to the web server. A page whose input exceeds the cap gets no further history until the cap is raised"; `<F>` is Task 5's measured factor), schema `workers` integer ≥ 0, `replayMaxBytes` integer ≥ 1048576 and `cpuShare` number with `exclusiveMinimum: 0`, `maximum: 1`, configmap `KNOT_HISTORY_WORKERS: {{ .Values.history.workers | quote }}`, `KNOT_HISTORY_REPLAY_MAX_BYTES: {{ .Values.history.replayMaxBytes | int64 | quote }}` and `KNOT_HISTORY_CPU_SHARE: {{ .Values.history.cpuShare | quote }}`. In `metrics.rs`, `init` adds `.set_buckets_for_metric(Matcher::Full("knot_history_replay_seconds".into()), &REPLAY_BUCKETS_SECONDS)` (Global Constraints).

- [ ] **Step 4: Run them to verify they pass, and render the chart**

Run: same as Step 2, then `helm template t deploy/helm/knot --set database.url=x --set session.key=y | grep KNOT_HISTORY_`
Expected: PASS; three lines with the defaults (the replay cap as a plain integer, not `1.6e+07`; `"0.5"`).

- [ ] **Step 5: Commit**

```bash
git add crates/knot-history crates/knot-obs crates/knot-server deploy/helm/knot
git commit -m "feat(history): history metrics, stats and chart values"
```

---

### Task 11: Docs, deviation notes and the release section

**Files:**
- Modify: `CHANGELOG.md` (new section under `## [Unreleased]` at :9; compare links at :1227-1228), `deploy/helm/knot/README.md` (new `### Upgrading from 0.9.10` after the 0.9.9 subsection ending before `## Operational notes` at :257; an "Operational notes" bullet; a values note), `docs/superpowers/specs/2026-11-04-page-history-architecture.md` (a new paragraph after line 9)

- [ ] **Step 1: CHANGELOG** — `## [0.9.11] - <the date the PR is opened>`, lines at most 76 characters, operator-facing:
  - `### Added`: knot now builds every page's history in the background: who edited, when, and how much, per editing session, back to the page's creation (or to the 0.9.9 upgrade for older pages). The History drawer is unchanged in this release; the new timeline arrives in the next. New metrics (list them by name) and the `history.workers`, `history.replayMaxBytes` and `history.cpuShare` chart values (`KNOT_HISTORY_WORKERS`, `KNOT_HISTORY_REPLAY_MAX_BYTES`, `KNOT_HISTORY_CPU_SHARE`). `knot-server history-stats` also reports checkpoint and entry sizes.
  - `### Changed`: `GET /api/docs/{id}/contributors` adds `former` to each person (always `false` for now).
  - `### Upgrading`: don't skip 0.9.10. One migration, `20261108120000_history_builder`, creates three empty tables; its lock attempts pause page writes and user updates for at most 100 ms each, then it commits at once. After the upgrade every page's history is built once in the background, pages edited longest ago first; expect extra CPU and database reads for a while (about one read of `doc_updates`), and watch `knot_history_builder_lag_revs` fall to 0 (pages given up are left out of it and counted by `knot_history_given_up_pages`). The builder runs one replay at a time per replica by default (`history.workers: 1`), paced to half a CPU core on average (`history.cpuShare: 0.5`; unpaced, one worker can take a whole core, the chart's default CPU limit for the whole pod); `history.workers: 0` turns it off on a replica. The database grows by checkpoints (about as much again as `doc_updates` with Task 5's `CHECKPOINT_STATE_FRACTION` of `(1, 1)`, about twice as much with `(1, 2)`: write the one Task 5 chose) and entries (small). A page whose history cannot be built is given up after ten failed attempts, two replay timeouts or three crashes in a row (see the chart README). 0.9.10 pods that restart after the migration commits crash-loop until new pods replace them; rolling back needs a database restore. The old snapshot thinning is unchanged.
  - Links: `[Unreleased]: …/compare/v0.9.11...HEAD`, `[0.9.11]: …/compare/v0.9.10...v0.9.11`.

- [ ] **Step 2: Helm README** — `### Upgrading from 0.9.10` (the migration and its lock attempts, as for 0.9.9; the background first build, what it costs, how to watch it; the `--timeout` needs nothing extra beyond the lock loop's 45 s per attempt); an "Operational notes" bullet "Page history builder": what it does, `history.workers` and `history.replayMaxBytes` (CPU: each worker is a replay thread that can use a whole core of the pod's CPU limit, shared with the web server, hence the default of 1 and its pacing to `cpuShare` (0.5) of a core; memory: a replay holds about F times its input (F as Task 5 measured it, written as a number), the page's state plus the edits and snapshots it reads, which `replayMaxBytes` caps for builds and word counts alike, so peak builder memory is about `workers × F × replayMaxBytes`, and the default fits two workers next to 256 MiB for the web server in the default 512Mi (or, if Task 5 found F > 16, only one); pages whose input exceeds the cap are given up as too large: raise `replayMaxBytes` together with `resources.limits.memory`, or lower `history.workers`; `history.workers: 0` turns the builder off on a replica during an incident, and history waits until it is back), pages given up (`knot_history_pages_given_up_total`, log `history page given up` with the doc id and reason; their history so far stays listed, and a page given up before its first build still lists its earlier versions; a page whose replay hangs while writing a checkpoint is given up at once, one whose replay times out twice in a row with nothing to blame at the second (`reason="timeout"`), one that killed the process three times at the third (`reason="crash"`); re-enable one with `UPDATE doc_history_state SET failures = 0, timeouts = 0, crashes = 0, retry_after = NULL WHERE doc_id = '…'`; a later knot release whose builder changed retries them by itself), sizing (the M1.1 bullet's rows/day × average bytes × ~1.6, plus checkpoints at about 1× that with `CHECKPOINT_STATE_FRACTION = (1, 1)` or about 2× with `(1, 2)`, whichever Task 5 chose, plus entries; backups grow the same), and metrics worth an alert: `knot_history_build_failures_total` (log `history build failed` with the doc id; report it), `knot_history_pages_given_up_total`, `knot_history_rows_unapplied_total`, `knot_history_gap_total` (edits that reached the page without being stored, credited to nobody), `knot_history_snapshots_unchecked_total` (snapshots the gap check could not use), `knot_history_replay_timeouts_total` and `knot_history_replay_threads_stalled` (a timed-out replay keeps a builder worker and a CPU core busy until it ends or the pod restarts: restart the pod when the gauge stays above 0), and a lag that does not fall. Ask operators to send `history-stats` output a few weeks after upgrading (the real-instance measurement).

- [ ] **Step 3: Architecture deviation notes** — a paragraph "**Deviations recorded in the M1.3 plan** ([link](../plans/2026-11-08-page-history-m1-3-builder-and-read-api.md), Rulings):" stating, one sentence each: the endpoint path `/history/entries`; thinning to 3 deferred to M1.4; genesis at `G = max(rev)` of rows with `created_at < history_since`, every page with a genesis checkpoint, snapshots at or below `G` not gap-checked; the rollout entry compared by `canonical::visible_content`; parked content credited at the draining rev; claims stored in `doc_history_state.parked`, pending content inside checkpoints; resends over blocks held past a gap applied (yrs trims them), pre-validation narrowed to the reproduced `block_store.rs:318` shape (or, if Task 1 found none, only the post-apply check), and the release-build post-apply check; the panic isolation and replay timeout, the worker permit riding the replay thread (one semaphore created in `main.rs` for M1.4 to share, `history.workers` defaulting to 1 for CPU, a gauge of stalled replay threads), `skip_revs` for rows that panic, hang, corrupt the replay or are refused (in genesis too, kept across rebuilds), prefix rows that fail blaming the starting checkpoint, never the row, snapshots whose probe fails, or that are too large to probe, left unchecked instead of blaming a row or giving the page up, the `union` fallback for snapshots whose probe is refused (and `merge_gap_state` for replays that pass a union-made gap checkpoint), and the claim's crash marker with give-up after `MAX_FAILURES` or past the replay cap (lifted by a newer `BUILDER_VERSION`, shown as `unavailable` in the list, left out of the lag gauge); one cap, `history_replay_max_bytes`, for build and word-count replays, snapshots counted toward it; `KNOT_HISTORY_WORKERS=0` as an off switch; the builder's lease (no transaction across compute, so page archive, restore, move, purge and grant removal never wait on a pass; every write, the release included, re-checks it), the claim's clean check (`Clean`: no pass for a page another replica already built), refilling tick and sweep loops, and `ensure_legacy_numbered` outside the pass; late snapshots probed at `built_rev` and found by the sweep; gap merges crediting parked claims and genesis's pending content never credited; an empty first copy row still creating its entry, and no entry for an empty template (the room writes no row for it; a follow-up appends the template row directly); rev gaps and rejected checkpoints as failed passes (checkpoints verified when written; a rejected periodic one dropped, a rejected genesis or gap one rebuilt past); gap claims for content a gap merge parks; the keyset cursor's `BaselineStart`; pending content in checkpoints (or, if Task 1 took the pending fallback, `pending_revs` and the re-applied rows); the extra state columns (`genesis_rev`, `parked`, `skip_revs`, `failures`, `retry_after`, `last_error`, `claimed_by`, `claimed_until`), `words_pending` and the `baseline_id` index; word counts attempted once, NULL on error, panic or over the cap; word counts for sessions only, and the diff core moved from M1.4 to M1.3 (M1.4 builds `pm_json` on `diff::entry_diff` and `diff::text_chunks`); `AuthorView` without `color`, `ANONYMIZED_SQL` as the one place M4 changes, `former` on the contributors API; no `knot_history_view_seconds` before M1.4; the size gauge's lock and the lag gauge's definition; the lease renewed before every compute; a timeout freezing the replay's stage (blame only when that stage held for half the timeout; a hang while encoding a checkpoint gives the page up); separate rows and words budgets; the replay cap as a hard bound (`rows_in_range` stops before the crossing row, snapshots counted with the rows they need); baseline entries written when a page is given up before genesis; `retry_after` never decoded into chrono (the `'infinity'` sentinel panics chrono); word counts over maximal runs within one text segment, not per yrs chunk; `replay_window`'s `merge_gap_at_end`; Markdown import rows tagged `meta.mode`; Task 5's measured numbers, the measured doc-memory factor F and the chosen `DEFAULT_HISTORY_REPLAY_MAX_BYTES` (and `CHECKPOINT_STATE_FRACTION`, if changed); gap checkpoints kept as source data across every rebuild and merged at their revs (ATOMICITY AND REBUILD's "the log is complete" does not hold for gap content), a rejected gap checkpoint dropped alone; `meta.gap_credit_rev` and `merge_gap_at_end` for sessions credited by the gap at their end; the version lift before the `MAX_FAILURES` check and `give_up` keeping an older build's `builder_version`; `timeouts`, `crashes` and `skip_baselines` (a second timeout with nothing to blame, a third crash, a baseline whose merge panics or hangs), and the builder releasing its leases on a graceful shutdown; pacing to `history.cpuShare`; the 10 s debounce of `dirty_candidates`; `knot_history_replay_seconds`'s own buckets up to 300 s; Task 3's spike outcome (and the fallback, if taken).

- [ ] **Step 4: Verify**

Run: `make lint && make test`
Expected: PASS. Then `awk 'length > 76' CHANGELOG.md | head` shows no line of the new section; `grep -n CHECKPOINT_STATE_FRACTION crates/knot-history/src/pass.rs` shows the fraction the CHANGELOG's and README's checkpoint factor (1× or 2×) and the README's F match.

- [ ] **Step 5: Commit**

```bash
git add CHANGELOG.md deploy/helm/knot/README.md docs/superpowers/specs/2026-11-04-page-history-architecture.md
git commit -m "docs(history): 0.9.11 upgrade notes and M1.3 deviations"
```

---

## After M1.3

Merge gates: CI's Helm upgrade test (`.github/workflows/helm-upgrade.yaml`, triggered by `migrations/**`) upgrades from the newest `v*` tag merged into HEAD (`helm-upgrade.yaml:77`), so it runs 0.9.10 → this branch only once `v0.9.10` is tagged and this branch is rebased onto the `main` that contains it; before that it runs 0.9.9 → 0.9.11, the path the CHANGELOG forbids, and its assertion 5 (an `import` row from 0.9.10) is not meaningful: rebase after the v0.9.10 tag and re-run the job before trusting the gate. Since Task 9 it checks that the upgraded release builds the canary page's history entries; the `ci` workflow's `playwright` job (`.github/workflows/ci.yml:104`) must pass; the reset list now truncates the three new tables. The release is the merge plus an annotated `v0.9.11` tag (the user's step).

M1.4 inherits: the list endpoint at `/api/docs/{id}/history/entries` and its JSON (`unavailable` for given-up pages, `catching_up` while genesis is missing); `diff::entry_diff` and `diff::text_chunks` (render `pm_json` from them, merging adjacent text, so "Show changes" matches the stored word counts, which `words::count_words` takes over maximal runs within one `TextChunk::segment`); replay for a view is `pass::replay_window` (`KeepDeleted`) from the latest checkpoint at or before `start_rev − 1` through the entry's `end_rev`, with `merge_gap_at_end` true when viewing a `gap` entry itself or a session whose internal `meta.gap_credit_rev` equals its `end_rev` (it holds text that gap merge drained; the word counts used the same rule), false otherwise (a gap entry at rev R is listed above every entry ending at R, so their "after" states exclude it; Rulings), which merges gap checkpoints at their revs through `merge_gap_state` (a union-made gap checkpoint is refused by a plain `merge_state`) and skips `doc_history_state.skip_revs`; it caps its input in SQL before fetching, passes a `Progress` it cancels on timeout, and runs inside `run_isolated` with a timeout and a permit of the builder's semaphore as its guard (`main.rs` holds the `Arc<Semaphore>` from `history_permits`; M1.4 adds it to `AppState` and must not create a second one, or `workers × F × replayMaxBytes` (F measured in Task 5) no longer bounds memory; with `history.workers: 0` it decides whether views get their own small semaphore) (with the pending fallback, it also re-applies the checkpoint's `pending_revs` rows first); the list's keyset cursor orders the log section by `end_rev`, which an open session's next pass raises, so a session first listed below the cursor can move above it between page fetches and appear on neither page: M1.4's client refreshes the first page when it sees an entry's `updated_at` change (or M1.4 keys pages by a stable column); entries' `updated_at` moves on every change, so the view cache can key on it; baseline entries view their `doc_history_baselines.state_bytes`, the rollout entry (`meta.rollout`) the `genesis` checkpoint; snapshot thinning to the 3 newest per page ships with M1.4, when the old drawer and its `/history` list retire; it is safe only because gap checkpoints are source data (Rulings): no thinning, rebuild or later `BUILDER_VERSION` may delete a `gap` checkpoint, and a rebuild merges each one at its rev. Views wait for permits that pacing (`history.cpuShare`) holds after each build; M1.4 decides whether views are paced too. Import entries carry `meta.mode` (`append` | `replace` | null): M1.4 words `replace` as "replaced the content (import)", `append` as an append, and null (workspace import, 0.9.10 rows) neutrally as "imported content". M2 writes restore meta `{target_entry_id, target_rev, target_ended_at}` (the list already reads it) and copy meta with `version_entry_id`, `version_at`, and appends `from_template`'s row directly (as Duplicate does) so an empty template also gets its "Created from template" entry (Rulings). M3's erase takes the page row's KEY SHARE, then `doc_history_state … FOR UPDATE`, and takes the builder lease the way `claim` does (waiting while `claimed_until > now()`; a builder whose lease is gone fails its write), rewrites checkpoints (pending content included; `gap` checkpoints rewritten, never deleted, since they hold content no row carries) and entries' word counts, and must keep `parked` consistent with the rewritten rows. M4 changes `people::ANONYMIZED_SQL` and the web validators for `former`.
