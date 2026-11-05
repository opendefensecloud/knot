# Page history M1.1: stop the loss (implementation plan)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** From this release on, knot never deletes a stored edit (`doc_updates` row) again, except when a page is purged, and it keeps every snapshot that survives today's thinning as a permanent "earlier version".

**Architecture:** One migration makes `doc_updates` append-only with a trigger that raises SQLSTATE `KH001` on any delete or rewrite that is not a purge cascade (or an explicitly flagged history rewrite, used from M3 on). The same migration stamps `documents.history_since` and copies every `doc_snapshots` row into a new `doc_history_baselines` table. The hourly GC stops deleting updates, keeps thinning snapshots (they are only a boot cache), and runs single-flight under an advisory lock. A `history-stats` CLI measures log growth so M1.3 can be tuned on real numbers. No UI change: today's History drawer keeps working unchanged.

**Tech Stack:** Rust (sqlx 0.9, tokio, clap), Postgres (PL/pgSQL trigger), Playwright reset list (TypeScript).

**Spec:** [docs/superpowers/specs/2026-11-04-page-history-design.md](../specs/2026-11-04-page-history-design.md), technical architecture [docs/superpowers/specs/2026-11-04-page-history-architecture.md](../specs/2026-11-04-page-history-architecture.md) (sections `storage_model`, `baseline`, `milestones` → M1.1).

## Global Constraints

- Migration file: `migrations/20261104120000_history_keep_updates.sql`. sqlx runs it inside one transaction, as the Helm `pre-upgrade` Job, while old pods still serve, so every lock it takes is held until COMMIT.
- Statement order inside the migration, chosen to keep the server usable while it runs:
  1. `SET LOCAL lock_timeout = '5s';` (a blocked lock request fails and the Job retries, instead of queueing the whole server behind it).
  2. Trigger function `doc_updates_append_only()` and trigger `doc_updates_append_only` on `doc_updates` (both names exactly this; M1.2 replaces this function with `CREATE OR REPLACE FUNCTION doc_updates_append_only()`). It must come before the copy, so no delete can land between copy and commit. It blocks `doc_updates` inserts (edit saves) until COMMIT; that pause is the copy time and goes in the upgrade note.
  3. `CREATE TABLE doc_history_baselines` **without** the FK, then the `INSERT ... SELECT FROM doc_snapshots` copy, then `ALTER TABLE doc_history_baselines ADD CONSTRAINT doc_history_baselines_doc_fk FOREIGN KEY (doc_id) REFERENCES documents(id) ON DELETE CASCADE`. Adding the FK after the copy keeps `documents` writable during the copy.
  4. Last: `ALTER TABLE documents ADD COLUMN history_since timestamptz NOT NULL DEFAULT now()`. It takes ACCESS EXCLUSIVE on `documents`, so it goes last and holds that lock only until COMMIT. `now()` is the transaction start time wherever the statement sits.
- Refusal error: `RAISE EXCEPTION 'doc_updates is append-only (page history)' USING ERRCODE = 'KH001'`.
- Trigger body, exactly this logic (every FK cascade runs inside `RI_FKey_cascade_del`, so cascades arrive with `pg_trigger_depth() > 1`, and a top-level statement has depth 1):

  ```sql
  IF TG_OP = 'DELETE' AND pg_trigger_depth() > 1 THEN RETURN OLD; END IF;
  IF current_setting('knot.history_rewrite', true) = 'on' THEN RETURN COALESCE(NEW, OLD); END IF;
  RAISE EXCEPTION 'doc_updates is append-only (page history)' USING ERRCODE = 'KH001';
  ```

  Trigger: `BEFORE DELETE OR UPDATE OF update_bytes, doc_id ON doc_updates FOR EACH ROW`.
- `documents.history_since timestamptz NOT NULL DEFAULT now()`. A page created after rollout satisfies `created_at >= history_since`.
- `doc_history_baselines(id bigserial PRIMARY KEY, doc_id uuid NOT NULL, snapshot_seq bigint NOT NULL, state_bytes bytea NOT NULL, state_vector bytea NOT NULL, created_at timestamptz NOT NULL, redacted_at timestamptz NULL, UNIQUE (doc_id, snapshot_seq))`, index `(doc_id, created_at DESC)`, FK added after the copy as above.
- Snapshot thinning stays exactly as today: keep 5 newest + newest per day for 30 days (`snapshots.gc(doc, 5, 30)`).
- GC advisory lock key: `const GC_LOCK_KEY: i64 = 0x6b6e_6f74_6763_0001;` ("knotgc" + 1) in `crates/knot-crdt/src/gc.rs`, taken with `pg_try_advisory_lock` on a **detached** connection, exactly like `crates/knot-server/src/archive.rs:224-239`.
- Deviations from the architecture, decided here: (a) the `knot_replicas` heartbeat is **not** in M1.1. Only erase (M3) reads it, and replicas older than M3 would be invisible to it either way, so it moves to M3. (b) The `knot_history_table_bytes` gauge moves to M1.3 with the other history metrics; `history-stats` covers measurement until then.

## Review Focus

1. **Purging a page with subpages.** `DELETE FROM documents` on a parent cascades to children and to their `doc_updates`; the trigger must allow every level of that cascade. Pinned in Task 1 (`purge_cascade_through_subpages_still_deletes_updates`).
2. **An old replica's GC during the rolling deploy.** Its exact statement `DELETE FROM doc_updates WHERE doc_id = $1 AND seq <= $2` must fail with `KH001` and not abort anything else; `gc.rs:38-48` in the old binary already logs and continues. Pinned in Task 1 (`old_gc_delete_statement_is_refused`).
3. **e2e reset.** `TRUNCATE ... CASCADE` fires no row triggers, so `e2e/support/reset.ts` keeps working with the append-only trigger in place. Pinned in Task 1 Step 6 (e2e smoke against a freshly built server).
4. **Existing pages keep loading.** Room hydration reads the latest `doc_snapshots` row plus newer `doc_updates`; nothing in that path deletes or updates rows. Pinned by the existing `history_integration.rs` and room tests staying green (Task 2 Step 6).
5. **Workspace deletion.** `workspaces → documents → doc_updates` is a two-level cascade (depth > 1); it must still work. Pinned in Task 1 (`workspace_delete_cascade_still_deletes_updates`).

---

### Task 1: Append-only `doc_updates`, `history_since` and permanent baselines

**Files:**
- Create: `migrations/20261104120000_history_keep_updates.sql`
- Modify: `crates/knot-storage/tests/migrations_apply.rs` (table list at :30-57, new tests at the end)
- Modify: `crates/knot-storage/tests/updates.rs` (`setup()` at :5-25 also returns the `PgPool`, i.e. `(PgUpdatesStore, PgPool, Uuid, Uuid)`, with its callers updated; replace `delete_up_to_removes_inclusive` at :74-92)
- Modify: `e2e/support/reset.ts:35-59` (add `"doc_history_baselines"` before `"doc_markdown_cache"`)

**Interfaces:**
- Consumes: nothing new.
- Produces: table `doc_history_baselines` (columns in Global Constraints); column `documents.history_since`; SQLSTATE `KH001`; session setting `knot.history_rewrite` (only M3 sets it).

- [ ] **Step 1: Write the failing tests** in `crates/knot-storage/tests/migrations_apply.rs`

Add `"doc_history_baselines"` to the `expected` table list (alphabetical: after `"doc_contributors"`). Then add, using `knot_test_support::fresh_db().await.pool` and raw SQL seeding as the file already does:

```rust
fn kh001(e: sqlx::Error) -> bool {
    e.as_database_error().and_then(|d| d.code()).as_deref() == Some("KH001")
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_delete_of_doc_updates_is_refused() {
    // seed ws, user, doc, two doc_updates rows
    let e = sqlx::query("DELETE FROM doc_updates WHERE doc_id = $1").bind(doc).execute(&pool).await.unwrap_err();
    assert!(kh001(e));
    assert_eq!(count_updates(&pool, doc).await, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn old_gc_delete_statement_is_refused() {
    // the exact statement of knot <= 0.9.8's UpdatesStore::delete_up_to
    let e = sqlx::query("DELETE FROM doc_updates WHERE doc_id = $1 AND seq <= $2")
        .bind(doc).bind(i64::MAX).execute(&pool).await.unwrap_err();
    assert!(kh001(e));
}

#[tokio::test(flavor = "multi_thread")]
async fn rewriting_update_bytes_is_refused() {
    let e = sqlx::query("UPDATE doc_updates SET update_bytes = '\\x00' WHERE doc_id = $1")
        .bind(doc).execute(&pool).await.unwrap_err();
    assert!(kh001(e));
}

#[tokio::test(flavor = "multi_thread")]
async fn flagged_history_rewrite_is_allowed() {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL knot.history_rewrite = 'on'").execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE doc_updates SET update_bytes = '\\x00' WHERE doc_id = $1").bind(doc).execute(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_cascade_through_subpages_still_deletes_updates() {
    // parent P with child C and grandchild G, each with doc_updates rows
    sqlx::query("DELETE FROM documents WHERE id = $1").bind(p).execute(&pool).await.unwrap();
    assert_eq!(count_updates_any(&pool, &[p, c, g]).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn workspace_delete_cascade_still_deletes_updates() {
    sqlx::query("DELETE FROM workspaces WHERE id = $1").bind(ws).execute(&pool).await.unwrap();
    assert_eq!(count_updates(&pool, doc).await, 0);
}
```

And a migration-boundary test modelled on `doc_contributors_migration_backfills_from_doc_updates` (migrate to `VERSION - 1` with a filtered `Migrator`, seed, then run the full migrator), with `const VERSION: i64 = 20261104120000;`:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn history_migration_copies_snapshots_and_stamps_history_since() {
    // before: doc D created 2026-01-01 with 3 doc_snapshots rows (seq 10, 20, 30)
    // run full migrator; let t_mig = SELECT now() right after
    // after:
    //   doc_history_baselines has exactly 3 rows for D, same snapshot_seq, state_bytes,
    //   state_vector and created_at as the doc_snapshots rows
    //   D.history_since is within 60 s of t_mig (not D.created_at), so D.created_at < D.history_since
    //   a doc created after the migration has created_at >= history_since (the "new page" predicate)
    //   DELETE FROM documents WHERE id = D removes its baselines (ON DELETE CASCADE)
}
```

In `crates/knot-storage/tests/updates.rs` replace `delete_up_to_removes_inclusive` with `rows_survive_and_direct_delete_is_refused`: insert 3 rows via `insert_batch`, assert a raw `DELETE FROM doc_updates WHERE doc_id = $1 AND seq <= $2` fails with code `KH001`, and `since(doc, 0)` still returns 3 rows. (`delete_up_to` itself is removed in Task 2; leave its trait method in place for now.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p knot-storage --test migrations_apply --test updates`
Expected: FAIL — table list mismatch, deletes succeed, `doc_history_baselines` does not exist.

- [ ] **Step 3: Write `migrations/20261104120000_history_keep_updates.sql`**

Header comment: why (page history keeps every edit; see the design doc) and why the statement order is what it is (locks are held until COMMIT). Statements in exactly the order of Global Constraints; the copy is `INSERT INTO doc_history_baselines (doc_id, snapshot_seq, state_bytes, state_vector, created_at) SELECT doc_id, snapshot_seq, state_bytes, state_vector, created_at FROM doc_snapshots`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-storage --test migrations_apply --test updates`
Expected: PASS. If a cascade test fails, print `pg_trigger_depth()` from inside the trigger with `RAISE NOTICE` and fix the condition; never loosen it to "always allow DELETE".

- [ ] **Step 5: Add `"doc_history_baselines"` to `TABLES` in `e2e/support/reset.ts`**

- [ ] **Step 6: e2e smoke**

Stop any knot-server already listening on :3000 first (Playwright reuses a running server outside CI, which would skip the new migration), then: `make compose.up`, `cd e2e && CI=1 npx playwright test flows/history.spec.ts`
Expected: PASS; `SELECT to_regclass('doc_history_baselines')` in the e2e database is not NULL; the server log shows no `KH001` during resets.

- [ ] **Step 7: Commit**

```bash
git add migrations/20261104120000_history_keep_updates.sql crates/knot-storage/tests/migrations_apply.rs crates/knot-storage/tests/updates.rs e2e/support/reset.ts
git commit -m "feat(history): keep every doc update and every surviving snapshot"
```

---

### Task 2: GC stops deleting updates and runs single-flight

**Files:**
- Modify: `crates/knot-crdt/src/gc.rs` (whole file, 53 lines)
- Modify: `crates/knot-crdt/src/lib.rs:27` (re-exports)
- Modify: `crates/knot-server/src/main.rs:179` (call site)
- Modify: `crates/knot-storage/src/updates_store.rs:50-51, 176-183` (remove `delete_up_to`)
- Modify: test doubles that implement `UpdatesStore`: `crates/knot-crdt/src/room.rs:1038, 1074, 1109, 2412, 2562`, `crates/knot-crdt/tests/nesting.rs:352, 1112`, `crates/knot-server/tests/suggestions_integration.rs:1638-1644` (`SlowUpdates`). Before editing, re-check with `grep -rn 'UpdatesStore for' crates` that there are no others.
- Test: `crates/knot-crdt/tests/gc.rs` (new)

**Interfaces:**
- Consumes: `SnapshotStore::gc(&self, doc_id, keep_recent: i64, retain_days: i32)` (unchanged).
- Produces:
  - `pub fn spawn(pool: PgPool, snapshots: Arc<dyn SnapshotStore>)` (the `updates` and `snapshot_every_n` parameters are gone).
  - `pub async fn run_once(pool: &PgPool, snapshots: &dyn SnapshotStore) -> Result<GcRun, sqlx::Error>`
  - `pub enum GcRun { Skipped, Ran { docs: usize, snapshots_deleted: u64 } }` (derive `Debug, PartialEq, Eq`)
  - `knot_crdt::{spawn_gc, gc_run_once, GcRun}` re-exported from `lib.rs`.

- [ ] **Step 1: Write the failing tests** in `crates/knot-crdt/tests/gc.rs`

```rust
#[tokio::test(flavor = "multi_thread")]
async fn gc_thins_snapshots_but_keeps_every_update() {
    // fresh_db; doc with 20 doc_updates rows and 8 snapshots, all created_at = now() - 40 days
    let run = knot_crdt::gc_run_once(&pool, &snapshots).await.unwrap();
    assert_eq!(run, GcRun::Ran { docs: 1, snapshots_deleted: 3 });   // 8 - keep 5
    assert_eq!(count_updates(&pool, doc).await, 20);
    assert_eq!(count_baselines(&pool, doc).await, 0);               // baselines are only the migration's copy
}

#[tokio::test(flavor = "multi_thread")]
async fn gc_is_single_flight_across_replicas() {
    // hold the lock on a separate detached connection
    let mut other = pool.acquire().await.unwrap().detach();
    let held: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(GC_LOCK_KEY_FOR_TEST).fetch_one(&mut other).await.unwrap();
    assert!(held);
    assert_eq!(knot_crdt::gc_run_once(&pool, &snapshots).await.unwrap(), GcRun::Skipped);
    // release, and the next run goes ahead
    sqlx::query("SELECT pg_advisory_unlock($1)").bind(GC_LOCK_KEY_FOR_TEST).execute(&mut other).await.unwrap();
    assert!(matches!(knot_crdt::gc_run_once(&pool, &snapshots).await.unwrap(), GcRun::Ran { .. }));
    // and run_once released its own lock: another session can take it now
    let mut probe = pool.acquire().await.unwrap().detach();
    let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)").bind(GC_LOCK_KEY_FOR_TEST).fetch_one(&mut probe).await.unwrap();
    assert!(free);
}
```

`GC_LOCK_KEY_FOR_TEST` is the literal from Global Constraints (`0x6b6e_6f74_6763_0001`); also export the const as `knot_crdt::gc::GC_LOCK_KEY` and assert equality in the test so the two cannot drift.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo nextest run -p knot-crdt --test gc`
Expected: FAIL to compile (`gc_run_once`, `GcRun` not defined).

- [ ] **Step 3: Implement**

`gc.rs`: rewrite the module doc (snapshots are a boot cache; updates are kept forever for page history and are never deleted here). `run_once` takes the advisory lock on a detached connection (`archive.rs:224-239` pattern), returns `Skipped` when not obtained, otherwise enumerates `SELECT DISTINCT doc_id FROM doc_snapshots` and calls `snapshots.gc(doc, 5, 30)` per doc, logging and continuing on per-doc errors as today; drops the lock connection before returning. `spawn` keeps the existing loop shape (sleep 1 h first) and calls `run_once`, logging a warning on `Err`. Remove `UpdatesStore::delete_up_to` from the trait, the Pg impl and every test double listed above; update `main.rs:179` to `knot_crdt::spawn_gc(pool.clone(), snapshots);` (`cfg.snapshot_every_n` stays used by `SnapshotPolicy`).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo nextest run -p knot-crdt --test gc`
Expected: PASS.

- [ ] **Step 5: Lint**

Run: `make lint`
Expected: no warnings.

- [ ] **Step 6: Full Rust suite**

Run: `make test.rust`
Expected: PASS, including `crates/knot-server/tests/history_integration.rs` (today's drawer API) and the room tests.

- [ ] **Step 7: Commit**

```bash
git add crates/knot-crdt crates/knot-storage/src/updates_store.rs crates/knot-server/src/main.rs crates/knot-server/tests/suggestions_integration.rs
git commit -m "feat(history): GC thins snapshots only and never deletes updates"
```

---

### Task 3: `knot-server history-stats`

**Files:**
- Create: `crates/knot-server/src/history_stats.rs`
- Modify: `crates/knot-server/src/main.rs:25-34` (new `Cmd::HistoryStats`), `crates/knot-server/src/lib.rs` (`pub mod history_stats;`)
- Test: `crates/knot-server/tests/history_stats_integration.rs` (new)

**Interfaces:**
- Produces:
  - `pub struct HistoryStats { pub update_rows: i64, pub update_bytes_total: i64, pub update_bytes_avg: f64, pub rows_per_page_day_p50: f64, pub rows_per_page_day_p95: f64, pub rows_per_page_day_max: i64, pub pages_with_updates: i64, pub baseline_rows: i64, pub baseline_bytes_total: i64, pub doc_updates_relation_bytes: i64 }` (derive `Debug`)
  - `pub async fn collect(pool: &PgPool, days: i32) -> Result<HistoryStats, sqlx::Error>`
  - `pub async fn run(cfg: knot_config::Config, days: i32) -> anyhow::Result<()>`: connects **without** migrating (as `export_check::run` does), prints one `key: value` line per field, human-readable sizes in parentheses for byte fields.
  - CLI: `knot-server history-stats [--days <N>]`, default `30`; doc comment: "Report how fast the page-history log grows. Read-only; does not migrate."

`rows_per_page_day_*` are over (doc_id, day) groups of `doc_updates.created_at` within the last `days` days (`percentile_cont` for p50/p95, `max` for max). `update_bytes_*` use `octet_length(update_bytes)` over all rows. `baseline_bytes_total` is `sum(octet_length(state_bytes) + octet_length(state_vector))`. `doc_updates_relation_bytes` is `pg_total_relation_size('doc_updates')`. Every aggregate is wrapped in `COALESCE(..., 0)`, and `avg` and the percentiles are cast `::float8` (Postgres returns `numeric` for `avg(int)` and NULL over zero rows).

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test(flavor = "multi_thread")]
async fn history_stats_reports_rows_bytes_and_daily_rates() {
    // fresh_db; page A: 4 rows of 10 bytes today, 2 rows of 10 bytes yesterday;
    // page B: 1 row of 40 bytes today;
    // 1 doc_history_baselines row with 100 bytes of state_bytes and 7 bytes of state_vector
    let s = knot_server::history_stats::collect(&pool, 30).await.unwrap();
    assert_eq!(s.update_rows, 7);
    assert_eq!(s.update_bytes_total, 100);
    assert!((s.update_bytes_avg - 100.0 / 7.0).abs() < 1e-9);
    assert_eq!(s.pages_with_updates, 2);
    assert_eq!(s.rows_per_page_day_max, 4);
    assert!((s.rows_per_page_day_p50 - 2.0).abs() < 1e-9);  // groups: 4, 2, 1
    assert!((s.rows_per_page_day_p95 - 3.8).abs() < 1e-9);  // percentile_cont, not _disc
    assert_eq!(s.baseline_rows, 1);
    assert_eq!(s.baseline_bytes_total, 107);
    assert!(s.doc_updates_relation_bytes > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn history_stats_on_empty_db_reports_zeros() {
    let s = knot_server::history_stats::collect(&knot_test_support::fresh_db().await.pool, 30).await.unwrap();
    assert_eq!((s.update_rows, s.update_bytes_total, s.pages_with_updates, s.rows_per_page_day_max, s.baseline_rows, s.baseline_bytes_total), (0, 0, 0, 0, 0, 0));
    assert_eq!((s.update_bytes_avg, s.rows_per_page_day_p50, s.rows_per_page_day_p95), (0.0, 0.0, 0.0));
}
```

Rows are inserted with raw SQL setting `created_at` explicitly (inserts are allowed by the trigger).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p knot-server --test history_stats_integration`
Expected: FAIL to compile (`history_stats` not found).

- [ ] **Step 3: Implement `collect`, `run` and the CLI wiring** as specified in Interfaces.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo nextest run -p knot-server --test history_stats_integration`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/src/history_stats.rs crates/knot-server/src/main.rs crates/knot-server/src/lib.rs crates/knot-server/tests/history_stats_integration.rs
git commit -m "feat(history): history-stats CLI to measure log growth"
```

---

### Task 4: Operator docs and changelog

**Files:**
- Modify: `CHANGELOG.md` (`## [Unreleased]`, line 9)
- Modify: `deploy/helm/knot/README.md` (new subsection under `## Upgrading` at :171; new bullet under "Operational notes" at :199)

- [ ] **Step 1: CHANGELOG `[Unreleased]`**, in the file's existing style (see `## [0.9.8]` at :18-28 for the `### Upgrading` format):
  - **Changed:** "Knot no longer deletes old edits. Every change to a page is kept for the page's whole life (purging a page still removes it). This is the groundwork for the page history timeline; the History drawer itself is unchanged in this release."
  - **Added:** "`knot-server history-stats` reports how fast the stored edit log grows (rows and bytes per page per day). Run it a few weeks after upgrading and share the output with the maintainers."
  - **`### Upgrading`:** "One database migration runs on upgrade. Rolling back to 0.9.8 needs a database restore." Then the migration bullet: "`20261104120000_history_keep_updates` makes the stored edit log append-only, adds `documents.history_since` and copies every stored snapshot into `doc_history_baselines`. While it runs, saving edits pauses for about as long as the copy takes; check `SELECT pg_size_pretty(pg_total_relation_size('doc_snapshots'))` beforehand." And: "The database grows with every edit from now on; watch the size of `doc_updates`. Older pods still running during the rolling upgrade log a `doc_updates is append-only` warning from their hourly cleanup; that is expected and harmless."

- [ ] **Step 2: Helm README**: a new `### Upgrading to the release after 0.9.8` subsection under `## Upgrading` (:171) naming `20261104120000_history_keep_updates` and the edit-save pause during its copy; and an "Operational notes" bullet (:199) "Storage growth from page history": the edit log is kept forever, size it as rows/day × average row bytes × ~1.6 (tuple, index and TOAST overhead), backups and PITR grow at the same rate, and `knot-server history-stats` gives the inputs.

- [ ] **Step 3: Verify**

Run: `make lint && make test`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add CHANGELOG.md deploy/helm/knot/README.md
git commit -m "docs(history): changelog and sizing note for keeping every edit"
```

---

## After M1.1

The release itself (version, CHANGELOG section, tag) is the user's step. M1.2 (complete, commit-ordered, labelled log), M1.3 (entry builder and list API), M1.4 (timeline UI and Show changes), M2 (restore), M3 (erase, including the `knot_replicas` heartbeat) and M4 (Former member) each get their own plan, written from the architecture document.
