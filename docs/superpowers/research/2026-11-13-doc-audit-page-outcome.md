# Outcome: page audit log with export recording

**Date:** 2026-11-13
**Issue:** none yet (the spec says `#TBD`; the CHANGELOG entries carry no number)
**Spec:** [specs/2026-11-13-doc-audit-page-design.md](../specs/2026-11-13-doc-audit-page-design.md)
**Plan:** [plans/2026-11-13-doc-audit-page.md](../plans/2026-11-13-doc-audit-page.md)
**Goal:** The owner asked: "build me an audit page per document so i can see the audit stats for that document as an editor and upwards. also ensure it includes exports including who exported it." Each page now has an audit log at `/doc/:id/audit` for the people who are editor or owner on that page. Every file export is recorded with the person who exported it, and an export that can't be recorded is refused (fail closed, the owner's decision below).

## What landed

- **Migration** `20261113120000_audit_doc_index`. It adds `audit_events_target_idx (target_id, id)` under `knot_lock_tables('audit_doc_index', …, 'SHARE')`, with a 5 s lock timeout and bounded attempts. No table is added, so `migrations_apply`, `e2e/support/reset.ts` and `db.reset` are unchanged. The comment lists every audit write the build blocks (see Review findings).
- **Storage** (`knot-storage/src/audit.rs`).
  - `record_for_targets` writes one row per page in a single `INSERT … SELECT … UNNEST($5::uuid[])` (all or none), and does nothing for an empty slice. It returns `Result<(), AuditWriteError>` (`Database(sqlx::Error)` or `TimedOut`) and gives up after `EXPORT_AUDIT_WRITE_TIMEOUT` (5 s); the export handlers await it before releasing the file.
  - `list_doc_events` takes an action filter and the cursor bound `id < COALESCE($4::bigint, i64::MAX)`.
  - `doc_stats` runs its four grouped queries with `tokio::try_join!`: by action, export format × scope with a 30-day count, exporters, and the last export.
  - `record_with_data` (best effort, unchanged) and `record_for_targets` increment `knot_audit_write_failures_total{action}` next to their `warn!`. The metric is described in `knot-obs` `describe_audit_metrics()`.
- **Grant rows** (`grant_store.rs`). `doc.grant` now stores `{principal, role, inherit}` and `doc.grant.delete` stores `{principal}`. Older rows keep `{}` and read "changed access" or "removed access".
- **Export recording, fail closed.** Each page in the file gets one `doc.export` row, attributed to the session user, written before the file is returned. If the insert fails (or the server has no database), the export answers `503 audit.unavailable` (`http_error::audit_unavailable`) and no file.
  - docx/odt (`export.rs` `export_response`): `{format, scope:"page", diagram_pictures, fallbacks}`. The row is written after the build succeeded, outside the build task. In `knot_export_total` the 503 `audit.unavailable` counts as `server_error`, not `busy` (an `AuditUnavailable` marker extension on the response).
  - Markdown zip of a page or a subtree (`export_import.rs` `export_doc`): the ids come from `subset`, never `subtree_ids`.
  - Workspace zip: the ids come from `readable_docs`.
  - Zip rows carry `export_id`, `doc_count` and, for a subtree, `root_id`.
  - A zip row is written only for a `GET` whose zip was built (`write_export_zip` builds it in memory and answers 200), and the zip is returned only after `record_zip_export` succeeded. `HEAD` (no body) is neither recorded nor refused. The diagrams preflight, `…/markdown` and every refusal or failure write nothing.
- **Read route** `GET /api/docs/{id}/audit?category=&cursor=&limit=` (`routes/api/audit.rs`, split into `audit/render.rs` and `audit/event_token.rs`).
  - It runs behind `require_doc_role_mw` plus `require_editor`. That function moved from `history.rs` to `auth/require_doc_role.rs`, is `pub` and is re-exported.
  - Categories: `all`, `exports`, `access` and `changes`. An action in none of them shows as `other`.
  - `details` are whitelisted per action. People are rendered through `knot_history::load_people` / `history_author_view`, and former or missing people merge into one `by_person` row. A subtree root the reader can't open is `root: null, root_hidden: true`.
  - `stats` comes only on the first page. Responses carry `Cache-Control: private, no-store`. Bad input is 400 `audit.bad_request`.
  - Event ids and cursors are opaque tokens (see Review findings).
  - The route is pinned in `reads_and_unlisted_routes_are_not_limited`.
- **Web.**
  - A lazy `DocAuditPage` route at `doc/:id/audit`, with `AuditStats` (tiles, format line, scope line, exporters) and `AuditEventList` (filter chips, sentences, Load more).
  - `audit.api.ts`, `useDocAudit.ts` (`useInfiniteQuery`) and `auditText.ts` (sentences and actor labels), plus valibot schemas in `validators.ts`.
  - Entry points: `open-audit` in the page header, shown when `claimsEditShortcut` is true (so also on archived pages), and `ctx-audit` in the sidebar menu for editors.
  - Viewers get `audit-forbidden` without any request being sent. `ExportMenu` and the workspace export in Settings say that exports are recorded.
  - `audit.unavailable` shows the error toast "Export blocked: it couldn't be recorded in the audit log. Try again later." (`features/export/auditRefusal.ts`), from `useExport.ts` (docx/odt, no retry without pictures on a 503) and from `downloadFile.ts` (the page and workspace zips, which are fetched, not navigated).
- **Docs.** The CHANGELOG `[Unreleased]` has Upgrading, Added and Changed entries. The Helm README has a fourth migration in "Upgrading from 0.9.10 or 0.9.11" (halt query, `6 × 45 s`, optional `CREATE INDEX CONCURRENTLY`) and a new **Audit log** operational note. The knot-api skill has the endpoint row and an export gotcha. There is also a row in `docs/superpowers/README.md`.

## Deviations from the spec and plan

The spec was updated to match each of these.

- **Opaque event ids and cursors.** In the first design, `id` and `next_cursor` were the stored `audit_events.id`. Review showed that consecutive ids from one export reveal how many unreadable pages it contained. Ids are now sealed with `EventTokens`, and `cursor` is a 16-hex-digit string.
- **Awaited audit insert, fail closed.** The first design awaited the insert but was best effort. A review fix then detached it (`spawn_record_for_targets`) so a slow insert couldn't push a built export past the 30 s request timeout. The owner then decided exports must fail closed ("an export without an audit shouldn't work"): the insert is awaited again, `spawn_record_for_targets` is gone, and a failed insert refuses the export with `503 audit.unavailable`. See "Owner decision: fail closed".
- **Cursor SQL.** `($4 IS NULL OR id < $4)` became `id < COALESCE($4, i64::MAX)`, and the SQL is exposed as `#[doc(hidden)] list_doc_events_sql()` so a test can EXPLAIN it.
- **Failed filter switch.** Before, a failed filter switch fell back to the generic error. Now it is a list-level state of its own (`audit-events-failed`, `audit-events-retry`), and the stats and chips stay.
- **ExportMenu note.** The menu now names the note as its accessible description (`aria-describedby`).
- **Builder's choices the spec left open.**
  - `doc_stats` runs its queries with `tokio::try_join!`.
  - `routes/api/audit.rs` (390 lines) is split into a handler, `render.rs` (637 lines with tests) and `event_token.rs`.
  - TreeRow's audit test is its own file, `TreeRow.audit.test.tsx`.
- **e2e beyond the spec's list.** `doc-audit.spec.ts` also covers `ctx-audit`, the count of two exports and two people after the owner's subtree export, the Exports filter, and the link to the parent on the subpage's log.
- **Gates.** The plan ran targeted DB test targets instead of the spec's `cargo test --workspace`, as its Global Constraints ask.

## Review findings and what was done

Seven findings were confirmed, and all seven are fixed. Each fix has a test that was red before it, except the docs fix and the effective-role coverage.

1. **Consecutive ids leaked hidden page counts.** One subtree or workspace export inserts its rows in one statement, so their ids are consecutive. Gaps between the ids a reader could see counted the pages they couldn't open, which is exactly what withholding `doc_count` was meant to prevent.
   - Fix: `EventTokens` in `audit/event_token.rs` is a keyed 8-round Feistel permutation over 64-bit values, with HMAC-SHA256 rounds. Its key is derived from `session_key` under its own label and tweaked by the page id. The result is a 16-hex-digit token, bound to the page and the same on every replica.
   - `cursor` is opened back to the stored id. Malformed text, a raw number, or a value that opens to no positive id is 400 `bad cursor`.
   - Tests: `event_ids_and_cursors_do_not_reveal_the_stored_ids` (was red), and 4 unit tests (round trip, page and key binding, unrelated tokens for consecutive ids, malformed text refused). The paging tests compare against `EventTokens::seal`.
   - The web schemas, the API client and the hook now treat both as strings. The spec and the knot-api skill were updated.
2. **Audit insert awaited inside the request timeout.** *Superseded by the owner's fail-closed decision, see below.*
   - Fix at the time: `spawn_record_for_targets` was called right before the response returned, so a slow insert couldn't turn a built export into a 408. Recording was best effort.
   - Test at the time: `a_stalled_audit_write_never_holds_up_an_export`. It is replaced by `an_export_waits_for_a_stalled_audit_write_then_goes_out`, which pins the opposite: the export waits for the insert.
3. **Cursor predicate not an index condition under the generic plan.** The `OR` form became a filter over every newer row of the page once Postgres switched to the generic plan.
   - Fix: the `COALESCE` bound.
   - Test: `the_cursor_bound_is_an_index_condition_in_the_generic_plan`. It seeds 4 000 rows over 400 pages, runs `ANALYZE`, then runs `EXPLAIN (GENERIC_PLAN)` over the simple protocol and checks that `target_id = $2` and `id <` share one Index Cond. It was red with the `OR` form.
4. **A failed filter switch replaced the whole page with the generic error.**
   - Fix: `DocAuditPage` keeps the last loaded stats for the page, using React's adjust-state-during-render pattern. The failed chip stays pressed and the list shows its own error and retry. No other filter's events stand in, and another chip switches back from the cache. The generic `audit-retry` error is now only for a failed first load.
   - Test: "keeps the log and its filters when switching the filter fails" (was red).
5. **Screen readers never announced "Exports are recorded".**
   - Fix: the note has a `useId` id, and `role="menu"` points to it with `aria-describedby`. The note stays out of the arrow-key order.
   - Test: `toHaveAccessibleDescription` in `ExportMenu.test.tsx` (was red).
6. **Nothing tested that the page role, not the workspace role, decides.** This added coverage only, because the code was already correct.
   - `editors_read_the_audit_log_and_viewers_are_refused` now also checks two cases. A workspace viewer who owns the restricted `secret` page gets 200. A workspace editor with only a `viewer` grant on the restricted `vault` gets 403 `acl.editor_required`, while the owner of `vault` gets 200.
   - Either check fails if the gate is switched to `ctx.role`.
7. **Docs on what the index build blocks were incomplete.** The migration comment, the CHANGELOG Upgrading entry, the Helm README and the spec now give the same list: create, rename, move, archive, restore, duplicate, purge (the retention job's too), template mark and unmark, restrict, grant changes, member invites, role changes and removals, and quota refusals. Each also says the table grew only with those actions "before this release".
   - Editing the comment changes the migration's sqlx checksum. That is fine while the migration is unreleased, but a dev DB that already applied it needs a reset.

**Refuted on verification, no change:**

- "`doc_stats` holds 4 pool connections at once on every first-page audit read, and the route has no rate limit."
- "On touch devices and for screen readers the audit log only says 'Nd ago'; the exact time is in a `title` attribute." The spec sets this pattern: `<time dateTime title>`, with `dateTime` machine-readable.
- "The best-effort guarantee and the failure metric are untested." *Superseded by the owner's fail-closed decision:* there is no best-effort export guarantee any more, and the refusal and `knot_audit_write_failures_total` are now tested by `record_for_targets_returns_a_failed_insert_and_counts_it`, `record_for_targets_gives_up_on_a_stalled_insert_and_counts_it`, `an_export_that_cannot_be_recorded_is_refused` and `a_stalled_audit_write_refuses_the_export_before_the_request_timeout`.

## Owner decision: fail closed (2026-11-13)

The owner picked option 2 of "Decisions the owner may want to revisit": "an export without and audit shouldnt work."

- **Server.** `audit::record_for_targets` returns `Result` and the export handlers await it after the file is built and before it is returned: `export.rs` `export_response` (docx/odt) and `export_import.rs` `record_zip_export` (page, subtree and workspace zips). On an error, or with no `state.pool`, they return `503 audit.unavailable` (JSON envelope, message "the export couldn't be recorded in the audit log") and no file. The failure is logged and counted in `knot_audit_write_failures_total{action="doc.export"}`. `spawn_record_for_targets` is removed. The other best-effort writers (member invite, role change, removal, `quota.exceeded`) are unchanged.
- **Timeout interplay.** First version: the insert ran unbounded inside the request's 30 s `TimeoutLayer`, so an insert that hung ended in 408 and no file. Review of that version showed this was wrong (see "Review of the fail-closed change"). Now the insert is bounded at 5 s and a stalled one is refused with a counted 503. Only a build that already took nearly 30 s can still hit the timeout during the insert. A row that commits just as it fires is for a file the client never got. Accepted: over-recording is the safe direction.
- **HEAD** sends no body, so it is still neither recorded nor refused, and answers as before.
- **Metrics.** `knot_export_total` counts the 503 `audit.unavailable` as `outcome="server_error"`, so `busy` stays the export semaphore's alone. Both metric descriptions in `knot-obs` say so.
- **Web.** An error toast for `audit.unavailable` on all three export paths; the docx/odt retry without pictures never fires on a 503.
- **Tests (each red before the change).**
  - `knot-storage` `audit_read`: `record_for_targets_returns_a_failed_insert_and_counts_it` (a trigger raises on `doc.export` inserts; `Err`, the counter at 1, no row of the two-page insert).
  - `doc_audit_integration`: `an_export_that_cannot_be_recorded_is_refused` (the same trigger; docx, odt, page, subtree and workspace zips each 503 `audit.unavailable` with no `Content-Disposition` and no zip bytes, no row, the counter up by 5 through a test-binary recorder; HEAD 200; exports go out again once the trigger is dropped). `an_export_waits_for_a_stalled_audit_write_then_goes_out` (SHARE lock; the docx and zip exports have not answered after 500 ms, then answer 200 with the row there once the lock goes). The `export_file` and `export_zip` helpers now assert the row exists as soon as a 200 arrives. A mutation run that ignored the insert error made the refusal test fail.
  - `export.rs` unit: `an_export_refused_for_its_audit_row_is_a_server_error_not_busy`.
  - Vitest: `useExport.test.ts` (toast, one request, no retry), `ExportMenu.test.tsx` (subtree zip), `SettingsPage.test.tsx` (workspace zip).
- **No-pool exports.** No existing export test ran without a pool, so none needed adapting.
- **Gates after the change, before its review** (same throwaway Postgres): `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean; knot-storage `audit_read` 7 and `grants` 10; knot-server `doc_audit_integration` 18 (run 4 times, all green), `export_integration` 18, `export_check_integration` 1, `workspace_import_integration` 13, `quota_integration` 12, `rate_limit_integration` 20, `metrics_integration` 3; `cargo test -p knot-server --lib` 228; `cargo test -p knot-obs` 7; web `tsc`, `lint` and `vitest run` (127 files, 1 327 tests) pass. e2e and `make helm.check` were not re-run.

## Review of the fail-closed change

Seven findings were confirmed. Each fix that changes behaviour has a test that was red first.

1. **A stalled audit insert was refused uncounted, as a 408 labelled `client_error`** (medium). Postgres can hang an insert instead of failing it: another session's lock on `audit_events`, or a lost synchronous standby. knot sets no `statement_timeout`, and the pool's 10 s `acquire_timeout` covers only getting a connection. So every export sat in the insert until the 30 s `TimeoutLayer` dropped the handler. The dropped future never returned `Err`, so `knot_audit_write_failures_total` stayed at 0, the alert the README recommends stayed silent, and docx/odt counted as `client_error`.
   - Fix: `record_for_targets` runs the insert in its own transaction under `SET LOCAL statement_timeout` (via `set_config`, 5 s), all inside `tokio::time::timeout(EXPORT_AUDIT_WRITE_TIMEOUT)`, connection included. When the time runs out it returns `AuditWriteError::TimedOut` and logs and counts it like any failure, and the handlers answer `503 audit.unavailable` (`server_error`). Abandoned before its COMMIT, the insert never lands later.
   - Tests (both red before: the storage test hung, the integration test got 408 after 30 s): `record_for_targets_gives_up_on_a_stalled_insert_and_counts_it` (`audit_read`) and `a_stalled_audit_write_refuses_the_export_before_the_request_timeout` (`doc_audit_integration`). With a SHARE lock held, a docx and a zip export each get 503 with no file within the bound, the counter rises by 2, and no row appears once the lock goes. That test and `an_export_that_cannot_be_recorded_is_refused` both read the binary-wide counter, so they now hold one test lock (`failing_export_writes`) between them.
2. **The web retried a stalled docx/odt export after the 408** (low). That doubled the wait to about 60 s and pointed the user at the Markdown zip, which sits behind the same gate. Fixed by finding 1: a stalled insert is now a 503, and `useExport` never retries a 503 (its existing test pins that). A 408 now means only a slow build, which is what the retry without pictures is for. No web code change.
3. **`knot_audit_write_failures_total` was described as rows** (low). It counts failed inserts: one per refused export, however many pages it covered. The metric description, the CHANGELOG and the Helm README now say "failed audit writes, one per failed insert however many rows it held", and the spec adds that a refused 300-page workspace export adds 1.
4. **The Gates and e2e sections below called the pre-decision tree "the final tree"** (medium). They are now headed as the gates before the fail-closed change. The gates after it are in the section after this list.
5. **Dangling "See Known gaps" reference** (low). It is now marked superseded and names the tests that cover the refusal and the metric.
6. **The spec's test lists missed the fail-closed tests** (low). The spec now lists storage 3a/3b, server 9c, the `outcome` unit tests and the three `audit.unavailable` Vitest cases.
7. **The plan still said "best effort"** (low). It now has a "Superseded in part" banner, and its Architecture line and Global Constraint no longer say that.

Also updated: the comments in `export.rs` and `export_import.rs`, the `knot_export_total` description, and the spec's "What counts as an export", "Not recorded", "Recording points" item 6 and decision 2.

## Gates after the review fixes

Run against the throwaway Postgres `knot-claude-audit-postgres` (`:55432`).

- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean.
- knot-storage: `audit_read` 8 and `grants` 10 pass.
- knot-server integration: `doc_audit_integration` 19 (run 5 times, all green), `export_integration` 18, `export_check_integration` 1, `workspace_import_integration` 13, `quota_integration` 12, `rate_limit_integration` 20 and `metrics_integration` 3 pass.
- `cargo test -p knot-server --lib`: 228 pass. `cargo test -p knot-obs`: 7 pass.
- Web: `tsc` and `lint` pass. `vitest run`: 127 files, 1 327 tests, all pass.
- Helm: `make helm.check` passes (`helm lint` and `check-render.sh`); the only chart change is `README.md`.
- Not re-run: e2e, and the knot-storage `documents`, `grants_membership` and `migrations_apply` suites. The knot-server history, restricted-grants and admin-CLI suites were not re-run either.

## Gates before the fail-closed change

These ran on the tree before the owner's fail-closed decision, with exports recorded best effort, against the throwaway Postgres `knot-claude-audit-postgres` (`:55432`). The counts are historical. The gates on the current tree are in "Gates after the review fixes" above.

- **Rust:**
  - `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`: clean.
  - knot-storage: `audit_read` 6, `documents` 36, `grants` 10, `grants_membership` 6, `migrations_apply` 47. All pass, and the new migration applies.
  - knot-server integration: `doc_audit_integration` 17 (run 3 more times, all green), `export_integration` 18, `history_integration` 6, `history_entries_integration` 9, `restricted_grants_integration` 4, `admin_cli_integration` 4, `quota_integration` 12, `rate_limit_integration` 20. All pass. The history suites passing shows the moved `require_editor` changed nothing.
  - `cargo test -p knot-server --lib`: 227 pass, including the 4 `event_token` tests and the category, query, details, actor-kind and `by_person` unit tests.
  - `cargo test -p knot-obs`: 7 pass, including `audit_metrics_are_described`.
  - Not run: `cargo test --workspace` as a whole. Only the targets above ran.
- **Web** (from `web/`): `tsc` and `lint` (max-warnings 0) pass. `vitest run`: 127 files, 1 324 tests, all pass. After the rate-limits work it was 122 files and 1 261 tests.
- **Helm:** `make helm.check` passes (`helm lint` and `check-render.sh`). CI's `ct lint` couldn't run locally because `ct` and `yamllint` are not installed. `ct.yaml` has `check-version-increment: false`, and the only chart change is `README.md`.

## e2e (before the fail-closed change)

This run predates the fail-closed change and was not repeated after it (see Known gaps).

- **Stack.** Docker only, as the compose project `knot-claude-audit-e2e`:
  - knot built from the working tree (`docker build -t knot-claude-audit:dev .`)
  - `postgres:18-alpine` on tmpfs, with database `knot_e2e_audit`; `migrate` reported max version `20261113120000`
  - Dex v2.45.1
  - Vite (`node:24-bookworm`)
  - all on one `net` service; no toxiproxy, so collab goes straight to `:3000`
  - Torn down with `down -v` afterwards. No `knot-dev-*` container was touched.
- **Targeted.** `doc-audit.spec.ts` 1/1 and `mobile.spec.ts` 3/3 pass, including the header test with `open-audit` and the audit log fitting a 375 px viewport.
- **Full suite** (every spec except `ws-reconnect`, which needs toxiproxy): 144 tests, 143 passed, 1 failed, 0 flaky.
  - The failure was `docs.spec.ts` "docs CRUD + grant flow". `docs.spec.ts:98` hard-codes `psql -d knot` and ignores `KNOT_E2E_DB`, so it found no database named `knot`. That file and `support/reset.ts` are unchanged on this branch.
  - Re-run against a migrated database named `knot` on the same stack: `docs.spec.ts` and `doc-audit.spec.ts` both pass. That covers the changed `grant_store` writes too.
  - So all 144 tests in scope passed on the branch as it was then. Specs that have been flaky on main (history, excalidraw, tree-reorder nest) passed on the first try.
- No code, test or image changes came out of the e2e run, and no assertion was weakened.

## Merged with 0.9.12 (API keys and bots)

`main` after 0.9.12 (#81, scoped API keys and bot accounts) was merged into the branch. What the merge needed beyond the conflict resolution:

- **Docs.** The CHANGELOG entries moved to `[Unreleased]` (0.9.12 is released and untouched); the Upgrading note now covers one migration on top of 0.9.12. The Helm README has an "Upgrading from 0.9.12" section for `20261113120000_audit_doc_index` (lock, the one-loop `--timeout` budget, halt query, optional `CREATE INDEX CONCURRENTLY` with the invalid-index caveat) and the **Audit log** operational note again, now naming the API key actions and `data->>'api_key_id'`. The knot-api skill has the audit log row and gotchas back, next to main's API-key text.
- **Scope.** The audit route needs `read` by the method default and is not session-only, as main does for the grants and access listings: reading who exported a page grants nothing and outlives nothing. The key's user's page role still decides, so a viewer's key or a viewer bot gets `403 acl.editor_required`. The `OVERRIDES` comment on the export POST now says it writes its audit row.
- **Exports with a key.** All five export paths store `data.api_key_id` when an `ApiPrincipal` authenticated the request (read before the multipart body is consumed); `actor_id` stays the key's user. The log shows `details.via_api_key: true` only, and the web says ", with an API key". `operator_fields_never_reach_the_client` now exports with a key too and checks neither `api_key_id` nor the key's id reach the client.
- **Bots.** A bot is marked `"bot": true` wherever the log names someone, via `render::ActorView` (audit module only; history's `AuthorView` is unchanged) and `knot_storage::audit::bots_among`, one query next to `load_people` instead of a new `HistoryPerson` field. The web shows "CI bot (bot)"; a removed grant on a bot reads "removed the access of CI bot (bot)".
- **Checked, no change needed.** main's new audit actions (`api-key.create`, `api-key.revoke`, `api-key.revoke-cascade`, and `kind` on `workspace.member.invite`) all target `user`, so no page shows them. main's bot guards on import, create and restrict sit before this branch's code and don't touch export recording. `AuthContext` is unchanged (main added `SessionHash` and `ApiPrincipal` as separate extensions).
- **Tests added.** `an_export_made_with_an_api_key_names_its_user_and_flags_the_key`, `a_bot_is_marked_as_a_bot_on_the_audit_log`, `api_keys_read_the_audit_log_by_their_users_role_on_the_page` (integration); `the_page_audit_log_is_a_read_open_to_keys` (scope), two render unit tests, `bots_among_finds_only_the_bots` (storage); Vitest for the bot label, the API-key sentence, the validator default and the page.
- **Gates after the merge:** fmt and clippy (`-D warnings`) clean. knot-storage: `audit_read` 9, `grants` 10, `grants_membership` 6, `documents` 36, `migrations_apply` 68, `api_keys` 13, `users` 9. knot-server: `doc_audit_integration` 22, `export_integration` 18, `export_check_integration` 1, `workspace_import_integration` 13, `history_integration` 6, `history_entries_integration` 9, `restricted_grants_integration` 4, `admin_cli_integration` 4, `quota_integration` 12, `rate_limit_integration` 20, `metrics_integration` 3, `api_keys_integration` 51, lib 283. knot-obs 7, knot-history 192 (1 ignored). Web: tsc and lint clean, Vitest 129 files, 1379 tests. `make helm.check` passes. e2e was not run.

## Known gaps

- **Post-merge doc review (fixed).** The Helm README, CHANGELOG and ARCHITECTURE.md now list the 0.9.12 writes that wait for the index build: the API-key revocation triggers on password changes, member removals and owner demotions, API key creation and revocation, and exports. The migration file itself was left as merged in #97 (its comment predates API keys), because editing it would change its sqlx checksum for databases that already applied it. The Helm README and CHANGELOG lists now include quota refusals. Both docs and ARCHITECTURE.md now say that member removals, owner demotions and password changes fail, uncounted by `knot_audit_write_failures_total`, when `audit_events` refuses inserts, because their trigger writes in the change's transaction. ARCHITECTURE.md also says that `doc.export` rows keep `data.api_key_id`, and describes the page audit log route and the fail-closed exports. The 2026-11-06 spec has forward notes.

- **An audit outage blocks every export.** That is the intended fail-closed behaviour; operators should alert on `knot_audit_write_failures_total{action="doc.export"}`. An insert that hangs is given up on after 5 s and is counted there too. Only a build that already took nearly 30 s can still run into the request timeout (408, no file) during the insert.
- **e2e not re-run after the fail-closed change or its review fixes.** `mobile.spec.ts` and `doc-audit.spec.ts` export and then open the audit page; with the awaited insert the row is there before the file, so the earlier race is gone, but the e2e suite was not run again.
- **Rows from old pods.** Exports served by old pods during a rollout are not recorded.
- **`docs.spec.ts:98` ignores `KNOT_E2E_DB`.** It should read it the way `support/reset.ts` does. This was already the case on main and isn't part of this feature.
- **No issue number.** The CHANGELOG entries and the spec still have none.
- **Migration checksum.** The migration file's sqlx checksum changed during review, before #97 was merged. A dev database that applied an earlier branch version needs a reset; the file is unchanged since #97.

## Non-obvious

- Cursors and event ids are bound to the page and to `session_key`. Rotating the session key only invalidates open cursors, and every replica derives the same key.
- The generic-plan test needs a realistic spread of pages. With 20 pages of 200 rows each, the planner picked the primary key for the `COALESCE` form. With 400 pages it uses `audit_events_target_idx`.
- Viewers never call the route. The client checks `useEffectiveRole`, but the server is still the guard.

## Decisions the owner may want to revisit

These are carried over from the spec. Number 2 is now decided.

1. **The Markdown view and API reads are not exports.** `GET …/markdown`, which is also how API scripts "read as markdown", leaves no trace. The alternative is an opt-in `?download=1` recorded as `format:"markdown"`.
2. **Fail closed (decided by the owner 2026-11-13).** An export whose audit rows can't be written is refused with 503 `audit.unavailable` and no file. See "Owner decision: fail closed".
3. **One row per included page.** A workspace export of N pages writes N rows, and the stats count it once per page. The alternative is one event plus a link table.
4. **Editors see everyone's exports**, including viewers' exports and break-glass admin grants. Viewers are told by the ExportMenu note.
5. **No retention.** The table grows forever, and deleting rows is up to the operator. A `KNOT_AUDIT_RETENTION_DAYS` job (archive.rs pattern) is deferred.
6. **Plain `CREATE INDEX` under SHARE** rather than the two-file `CONCURRENTLY` pattern. That is fine unless the production `audit_events` table is large, and the Helm README describes the manual `CONCURRENTLY` pre-build.
7. **The scope boundary is static copy** ("before this version"), not a date read from `_sqlx_migrations`.
8. **Not shown on this page:** copies made from it, an ancestor's archive, restore or purge, and a purged page's log.
9. **No rate-limit bucket** for the read route.

## Deferred

- Recording reads (page views, the Markdown view, attachments, public links), history restores, share links, suggestion decisions and markdown imports.
- A workspace-wide audit view, a CSV download, date-range and actor filters, and retention.
- Moving zip exports to POST. A cross-site GET can still trigger an export, which is recorded against the victim and limited by the Export bucket.
