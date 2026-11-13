# Page Audit Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> **Superseded in part (2026-11-13).** The owner decided exports **fail closed** ("an export without an audit shouldn't work"): an export whose `doc.export` rows can't be written, or whose insert takes longer than `audit::EXPORT_AUDIT_WRITE_TIMEOUT` (5 s), is refused with `503 audit.unavailable` and no file. Wherever this plan says exports are recorded "best effort", that "an audit insert never fails an export", or describes a detached `spawn_record_for_targets`, it is out of date: that function was removed and the export handlers await `record_for_targets`. The spec ("What counts as an export", "Recording points") and the outcome doc ("Owner decision: fail closed") are current.

> **Merged with 0.9.12 (2026-11-13).** The branch was brought up to date with `main` after 0.9.12 (scoped API keys and bot accounts, #81). Exports made with an API key also store `data.api_key_id` and show `details.via_api_key`; bots are marked `"bot": true` on the log; the migration now runs on top of 0.9.12's five, and the CHANGELOG entries moved to `[Unreleased]`. See the spec's "API keys and bots" and the outcome doc's "Merged with 0.9.12".

**Goal:** Each page gets an audit log at `/doc/:id/audit` for its editors and owners (by effective role on the page, archived pages included): export stats first (how often, by whom, when last, in which formats), then a paged, filterable list of the page's audited events in plain sentences. Every successful file export that contains the page is recorded with the person who exported it.

**Architecture:** No new table. Exports become `doc.export` rows in `audit_events`, one per page in the file, written after the file was built and before it is sent, fail closed (docx/odt in `export_response`, Markdown zips of a page, a subtree or the workspace after `write_export_zip` answered 200 to a `GET`). One migration adds `audit_events_target_idx (target_id, id)`. One read route, `GET /api/docs/{id}/audit`, behind `require_doc_role_mw` plus the moved `require_editor`, renders people through `knot_history::load_people` / `history_author_view` and sends whitelisted `details` only. The web adds a lazy `DocAuditPage` route and two entry points (page header, sidebar menu), and tells exporters that exports are recorded.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9 runtime queries), Postgres 18, React 18 + TanStack Query + valibot, Vitest, Playwright.

**Spec:** [docs/superpowers/specs/2026-11-13-doc-audit-page-design.md](../specs/2026-11-13-doc-audit-page-design.md), binding. Where this plan and the spec disagree, the spec wins; where the spec's file:line references disagree with the code, follow the code and keep the spec's intent, and say so in the task report.

**Base branch:** `feat/doc-audit-page` off `main` (ab8c9b5), worked in place in `/home/chris/odc/knot`.

## Global Constraints

- Error envelope via `http_error.rs` `json_err`; new code `audit.bad_request` (messages `bad category`, `bad cursor`, `bad limit`).
- Raw `audit_events.data` never reaches the client. `doc_count`, `export_id`, `fallbacks` and `source_id` are for operators (SQL) only.
- Never an email, never a page id or title the reader can't open, never a count of pages the reader can't open. Anonymized and missing people merge into one `former` row in `by_person`.
- Gate by the effective doc role (`EffectiveDocRole`), never by `ctx.role`.
- Superseded: exports fail closed (see the banner). The other pool writers (member changes, `quota.exceeded`) still log, count `knot_audit_write_failures_total{action}` and go on.
- Migrations forward-only, lock taken with `knot_lock_tables`. After adding one, `touch crates/knot-test-support/src/lib.rs`.
- DB tests: `knot_test_support::fresh_db()` with `KNOT_TEST_DATABASE_URL`. Run targeted test targets, not the whole workspace at once.
- Web tests: `cd web && NODE_OPTIONS=--no-experimental-webstorage ./node_modules/.bin/vitest run <files>`; pnpm via `npx --yes pnpm@11.22.0`.
- Test first: each test below is written red, then made green.
- Descriptive variable names (user rule). Agents don't commit or push; the user does.

## Review Focus

1. **No restricted page leaks through a subtree export** → Task 2, `a_subtree_export_records_every_included_page_and_no_restricted_one` (rows from `subset`, never `subtree_ids`) and `a_subtree_row_hides_a_root_the_reader_cannot_open`.
2. **Operator fields never reach the client** → Task 2, `operator_fields_never_reach_the_client`.
3. **Only released files count** → Task 2, `refused_and_failed_exports_record_nothing`, `head_on_a_zip_export_records_nothing`, `the_diagrams_preflight_and_markdown_read_record_nothing`.
4. **Effective role, not workspace role** → Task 2, `editors_read_the_audit_log_and_viewers_are_refused` (the workspace owner without a grant on a restricted page gets `403 acl.no_grant`).
5. **No re-identification** → Task 2, `system_and_former_actors`.
6. **The moved `require_editor` changes nothing** → `history_entries_integration`, `history_integration` stay green.

---

### Task 1: Backend storage (migration, writers, reads, metric)

**Files:** Create `migrations/20261113120000_audit_doc_index.sql`, `crates/knot-storage/tests/audit_read.rs`; modify `crates/knot-storage/src/audit.rs` (module header, `record_for_targets`, `DocAuditRow`, `list_doc_events`, `ActionCount`, `ExportGroup`, `ExporterCount`, `DocAuditStats`, `doc_stats`, failure counter), `crates/knot-storage/src/grant_store.rs` (`put` and `delete` write `principal`, `role`, `inherit` / `principal`), `crates/knot-storage/tests/grants.rs`, `crates/knot-obs/src/metrics.rs` (`describe_audit_metrics()` called from `init`).

- [x] Failing tests (`audit_read.rs`): `list_doc_events_is_newest_first_and_only_this_page`, `list_doc_events_pages_below_the_cursor_and_filters_by_action`, `record_for_targets_writes_one_row_per_page_and_nothing_for_none`, `doc_stats_counts_actions_formats_scopes_and_exporters` (NULL actors not among exporters; 30-day window), `audit_target_index_exists`.
- [x] Failing tests (`grants.rs`): `grant_rows_carry_principal_role_and_inherit`, `grant_delete_rows_carry_principal`.
- [x] Failing unit test (`metrics.rs`): `audit_metrics_are_described`.
- [x] Implement; `cargo test -p knot-storage --test audit_read --test grants`, `cargo test -p knot-obs`; clippy.

### Task 2: Backend server (export recording, read route)

**Files:** Create `crates/knot-server/src/routes/api/audit.rs` (`//!` header, category table, query parsing, `doc_audit`, details whitelist, actor kinds, stats rendering), `crates/knot-server/tests/doc_audit_integration.rs`; modify `routes/api/mod.rs` (`pub mod audit;`), `routes/api/docs.rs` (route in `doc_id_routes` after the contributors route; header list), `auth/require_doc_role.rs` and `auth/mod.rs` (`require_editor` moved from `history.rs`, `pub`, re-exported), `routes/api/history.rs` (imports it), `routes/api/export.rs` (`AuthContext` cloned before `read_pictures`, `diagram_picture_count`, `record_with_data` after the build succeeded and outside the spawned task; header line), `routes/api/export_import.rs` (`is_get`, ids from `subset` / `readable_docs`, `record_zip_export` after a 200; header line), `rate_limit/http.rs` (pin the route in `reads_and_unlisted_routes_are_not_limited`).

- [x] Failing integration tests (`doc_audit_integration.rs`, fixture from `history_entries_integration.rs`, export helpers from `export_integration.rs`): `editors_read_the_audit_log_and_viewers_are_refused`, `a_viewers_docx_export_is_recorded_with_who_and_format` (and odt), `the_diagrams_preflight_and_markdown_read_record_nothing`, `refused_and_failed_exports_record_nothing` (400, 409 archived, 413, 503 busy), `a_zip_page_export_is_recorded_once`, `a_subtree_export_records_every_included_page_and_no_restricted_one`, `a_subtree_row_hides_a_root_the_reader_cannot_open`, `a_workspace_export_records_each_readable_page`, `head_on_a_zip_export_records_nothing`, `operator_fields_never_reach_the_client`, `stats_on_the_first_page_only_and_independent_of_category`, `paging_and_bad_input`, `category_filter_keeps_paging_correct`, `system_and_former_actors`, `grant_rows_name_the_principal`.
- [x] Failing unit tests (in crate): the details whitelist drops unknown keys; the `actor_kind` mapping; every action string a writer uses is in exactly one category; the rate-limit pin.
- [x] Implement; `cargo test -p knot-server --test doc_audit_integration`, then `--test history_entries_integration --test history_integration --test export_integration --test restricted_grants_integration --test grants_membership` and `--lib`; fmt; clippy.

### Task 3: Frontend

**Files:** Create `web/src/features/docs/audit.api.ts`, `useDocAudit.ts`, `auditText.ts`, `DocAuditPage.tsx`, `AuditStats.tsx`, `AuditEventList.tsx` and their tests `audit.api.test.ts`, `auditText.test.ts`, `DocAuditPage.test.tsx`, `DocPage.audit.test.tsx`; modify `web/src/lib/validators.ts` (`auditAuthorSchema`, `docAuditEventSchema`, `docAuditStatsSchema`, `docAuditPageSchema`), `web/src/routes.tsx` (`doc/:id/audit`, lazy, before the `:code` catch-all), `features/docs/DocPage.tsx` (`open-audit` link after History, on `claimsEditShortcut`), `features/docs/TreeRow.tsx` (`ctx-audit` in the first `canEdit` block), `features/export/ExportMenu.tsx` (`export-audit-note`), `features/workspace/SettingsPage.tsx` (`workspace-export-audit-note`), and the TreeRow and ExportMenu tests.

- [x] Failing vitest: `audit.api.test.ts` (URL, encoding, unset params omitted, error envelope passed through); `auditText.test.ts` (every action and scope, old rows without data, hidden root, null and former actors, each `actor_kind`); `DocAuditPage.test.tsx` (tiles, format line, exporters with "Former member", sentences, the Exports filter sends `category=exports`, Load more sends `cursor`, `audit-forbidden` on 403 and with no request for a viewer, `audit-no-access`, generic error with `audit-retry`, empty states, scope note); `DocPage.audit.test.tsx` (`open-audit` for owner and editor, archived too, not for a viewer); TreeRow `ctx-audit` for editors only; the ExportMenu note.
- [x] Implement; vitest on those files and the touched ones; `npx --yes pnpm@11.22.0 tsc`; `npx --yes pnpm@11.22.0 lint`.

### Task 4: Docs

**Files:** `CHANGELOG.md`, `deploy/helm/knot/README.md`, `.claude/skills/knot-api/SKILL.md`, `docs/superpowers/README.md`, this plan; later `docs/superpowers/research/2026-11-13-doc-audit-page-outcome.md`.

- [x] `CHANGELOG.md` `[Unreleased]`: Upgrading (the index migration's `SHARE` lock in bounded attempts, four lock loops in the `--timeout`, `audit_events` growth per exported page, no retention); Added (the page audit log, the route, `doc.export`, `knot_audit_write_failures_total`); Changed (grant rows carry the principal).
- [x] Helm README "Upgrading from 0.9.10 or 0.9.11": the fourth migration, its lock, the halt query, `6 × 45 s` and at least 6 minutes, the optional `CREATE INDEX CONCURRENTLY`; Operational notes **Audit log** bullet (recorded and not, growth, no retention, the metric worth an alert), the `quota.exceeded` mention pointing to it.
- [x] knot-api skill: the endpoint row, the export gotcha, "page audit log" in the description.
- [x] `docs/superpowers/README.md` row after the rate-limits row.
- [x] Outcome doc once the gates ran.

### Task 5: e2e

**Files:** Create `e2e/flows/doc-audit.spec.ts`; modify `e2e/flows/mobile.spec.ts`.

- [x] `doc-audit.spec.ts` (`test.beforeAll(reset)`): the owner seeds a page with a subpage and invites an editor and a viewer through /members; the viewer sees `export-audit-note`, exports Word, has no `open-audit` or `ctx-audit`, and gets `audit-forbidden` at `/doc/{id}/audit`; the editor opens the log with `open-audit` (one export, one person, the viewer's name and Word on the `doc.export` event); the owner exports Markdown with subpages; the editor reopens the log with `ctx-audit` (two exports, two people, the Exports filter lists only exports) and the subpage's log shows "as part of" with a link to the parent.
- [x] `mobile.spec.ts`: `open-audit` in the in-viewport list, the action-row comment updated, and no horizontal overflow on `/doc/{id}/audit` at 375 px.
- [x] Run on a throwaway compose Postgres (own project, tmpfs) with a per-run `KNOT_E2E_DB`, since the branch adds a migration.

### Gates

`cargo fmt --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; the DB-backed test targets above; web tsc, lint, vitest; `make helm.check`; e2e as in Task 5. If Postgres is unavailable: `cargo test --workspace --no-run`, and list in the outcome doc the DB tests that did not run.
