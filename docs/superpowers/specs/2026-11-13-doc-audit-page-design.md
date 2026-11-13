# Page audit log (per-document audit page with export recording) — design

**Date:** 2026-11-13
**Status:** Draft, from three candidate designs and two reviews ("minimal"
won both; grafts from "ux" and "compliance" merged, review errors fixed)
**Issue:** #TBD (ask the owner; CHANGELOG entries and the plan cite it)
**Branch:** `feat/doc-audit-page`
**Effort:** medium. One index migration, one read route, export recording at
three handlers, grant writer enrichment, one web page and two entry points.

The request, verbatim: "build me an audit page per document so i can see the
audit stats for that document as an editor and upwards. also ensure it
includes exports including who exported it."

## Context

- `audit_events` (`migrations/20260602000001_v0_1_schema.sql:125-134`):
  `id bigserial PK, workspace_id, actor_id NULL, action, target_kind,
  target_id, data jsonb DEFAULT '{}', created_at DEFAULT now()`. Only the PK
  index, no foreign keys, no retention. Purge keeps the rows (archive spec
  2026-10-13 :88-89). The foundation spec said "No UI in v0.1"; nothing reads
  the table except tests.
- Writers live in `crates/knot-storage/src/audit.rs`: `record` and
  `record_with_data` on the pool (best effort: a failure is a
  `tracing::warn!`, :30-54), `record_in_tx` and `record_in_tx_with_data`
  inside a transaction (return `Result`; failure aborts the mutation).
- Rows that target a page (`target_kind = 'doc'`, `target_id = page`) today:
  `doc.create`, `doc.rename`, `doc.move`, `doc.duplicate` (on the **copy**,
  `data.source_id` as text), `doc.archive` / `doc.restore` / `doc.purge`
  (archive **root** only; purge by the retention job has `actor_id NULL`,
  `archive.rs:282`), `doc.template.mark` / `.unmark`, `doc.restrict` /
  `.unrestrict`, `doc.grant` / `doc.grant.delete` (data `{}`,
  `grant_store.rs:283`, :348), `admin.grant` (break-glass CLI, `actor_id
  NULL`, data `{break_glass, principal, role, inherit}`, `grant_store.rs:313-330`).
- **No export is recorded.** The paths that hand a file out:
  - `POST /api/docs/{id}/export/{docx|odt}`: `routes/api/export.rs`,
    `export_response` (:191-273). Behind `require_doc_role_mw` (merged into
    `doc_id_routes`, `docs.rs:110`). Viewer and up. It reads only `user_id`
    from `AuthContext` (:200); `read_pictures(req)` consumes the request
    (:225); the build runs in a spawned task and succeeds at
    `Ok(Ok(built)) => built` (:246-248); the response is built after it.
  - `GET /api/docs/{doc_id}/export?descendants=` (`routes/api/export_import.rs`,
    `export_doc`). **Not** behind `require_doc_role_mw` (param `{doc_id}`,
    own router); it checks `acl.effective_role` itself and builds `subset`
    (root plus readable live descendants; archived pages and templates are
    never in `list_alive`), then `write_export_zip`.
  - `GET /api/workspace/export` (`export_workspace`): owner only
    (`acl.owner_required`), exports `readable_docs`.
  - `write_export_zip` returns a ready `Response`: 200 only after
    `zip.finish()` succeeded; every failure is a JSON error response.
- `require_editor` is a private fn in `routes/api/history.rs:30-45`
  (401 `auth.session_required`, 403 `acl.no_grant`, 403
  `acl.editor_required`). `require_viewer` is public in
  `auth/require_doc_role.rs:88-104`. `WorkspaceRole` has no `Ord`.
- People are shown through `knot_history::load_people` and
  `history_author_view` (`crates/knot-history/src/people.rs:47-90`), the one
  place that hides anonymized people (`ANONYMIZED_SQL`, `"false"` until M4).
  The page-history architecture keeps `audit_events.actor_id` for anonymized
  people (:487, :566), so an audit page that skipped this helper would
  re-identify them.
- `history::readable_sources` (`history.rs`, `pub(crate)`) resolves which
  referenced pages the reader may read, failing closed.
- Reads are not rate limited by design (`rate_limit/http.rs` `classify`;
  Helm README "Reads (page loads, search, history) … are not limited").

## Goals

1. Each page has an audit log at `/doc/:id/audit` that its editors and owners
   (by **effective role on that page**) can open, also when the page is
   archived. Viewers and people without access can't.
2. The page shows stats first: how often the page was exported, by whom,
   when last, in which formats, and counts per kind of event.
3. Every successful file export that contains the page is recorded, with the
   person who exported it, and shows on that page's log: Word, OpenDocument,
   Markdown zip of the page, of a parent's subtree, and of the workspace.
4. Below the stats, a paged, filterable list of every audited event on the
   page, in plain sentences.
5. The page states plainly what is and isn't recorded.

## Non-goals

- Recording reads: page views, `GET /api/docs/{id}/markdown` (the in-app
  "Show markdown" view refetches on every toggle and refresh, the import
  precheck calls it, it warms the zip exports' cache), attachment downloads
  (`GET /api/blobs/{id}` runs on every `<img>` render), public `/p/{token}`
  views, copy and paste. The log covers file exports, not all content egress.
- Showing a page's events that are recorded on another target: an ancestor's
  archive/restore/purge (root only), copies made **from** this page (the row
  is on the copy), or anything after a purge (the page's route is gone).
- Enriching `doc.rename` / `doc.move` data (titles in audit data would widen
  the erase surface; old rows have none anyway).
- New audited events beyond exports (history restore, share links, suggestion
  decisions, markdown import).
- A workspace-wide audit view, CSV download of the log, date-range filter,
  actor filter, retention.

## Anti-goals (would make this a failure even if it "works")

- The audit log leaks a restricted page: an id, a title, or even a count of
  pages the reader can't open (a subtree export's `doc_count` included).
- The audit log re-identifies an anonymized person, or shows an email.
- An export goes out without its audit row. Exports fail closed (owner
  decision 2026-11-13, "an export without an audit shouldn't work"): an
  export whose `doc.export` rows can't be written is refused with
  `503 audit.unavailable` and no file. That refusal is the intended
  behaviour, not a failure of this design.
- A failed, refused, timed-out, HEAD or preflight request shows as an export.
- Gating by workspace role (`ctx.role`) instead of the page's effective role:
  a workspace owner without a grant inside a restricted subtree must be
  refused like anyone else.

## Decided

### Why this shape (minimal, reuse `audit_events`)

One row per exported page in the existing table, one index, one read route,
no new table, store trait, `AppState` field or rate-limit bucket. A per-page
read stays a single index range; a restricted descendant the exporter couldn't
read was already filtered out of `subset` / `readable_docs`, so it gets no row
and nothing can leak. The rejected alternative, one event plus a link table
(`audit_event_docs`), only pays off for workspace-wide stats, which this page
doesn't need, and costs a table, a backfill and reset-list changes.

### Migration `migrations/20261113120000_audit_doc_index.sql`

```sql
-- audit_doc_index (page audit log)
--
-- The page audit log (GET /api/docs/{id}/audit) reads one page's events
-- newest first and counts them by action and exporter. audit_events had
-- only its primary key, so every read would scan the whole table.
-- (target_id, id) serves `WHERE target_id = $doc ORDER BY id DESC` as a
-- backward index scan; workspace_id, target_kind and action stay residual
-- filters (uuids don't repeat across kinds).
--
-- CREATE INDEX takes SHARE on audit_events, which blocks every audit
-- INSERT while it builds: the ones inside the transactions that create,
-- rename, move, archive, restore, duplicate and purge pages (the archive
-- retention job's purge too), mark and unmark templates, restrict pages
-- and change their grants, which wait with them; and the best-effort
-- writes of member invites, role changes and removals and of quota
-- refusals. Edit saves never touch the table. Take the lock in bounded
-- attempts (knot_lock_tables, 20261107120000) so a long transaction makes
-- this migration give up instead of queueing page writes behind it.
-- Before this release the table grew only with those actions; from now on
-- every export adds one row per exported page as well.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('audit_doc_index',
                        ARRAY['audit_events']::regclass[],
                        'SHARE');

CREATE INDEX IF NOT EXISTS audit_events_target_idx
    ON audit_events (target_id, id);
```

After adding it, `touch crates/knot-test-support/src/lib.rs` (migrations are
embedded at compile time). No table is added, so
`crates/knot-storage/tests/migrations_apply.rs`, `e2e/support/reset.ts` and
the Makefile `db.reset` list stay as they are. An operator with an unusually
large `audit_events` can pre-build the index by hand with
`CREATE INDEX CONCURRENTLY audit_events_target_idx ON audit_events (target_id, id)`;
`IF NOT EXISTS` then skips it (Helm README note).

### Action `doc.export` and its data

`action = 'doc.export'`, `target_kind = 'doc'`, `target_id` = **each page
whose content is in the file**, one row per page, `actor_id` = the session
user, written by `audit::record_for_targets`, which the export handlers
await before releasing the file (fail closed, see "What counts as an
export").

| Path | Rows | `data` stored |
|---|---|---|
| `POST /api/docs/{id}/export/docx` | 1, on `{id}` | `{"format":"docx","scope":"page","diagram_pictures":N,"fallbacks":{"diagram":n,"image_unavailable":n,"image_over_budget":n,"unknown_element":n}}` |
| `POST /api/docs/{id}/export/odt` | 1, on `{id}` | same with `"format":"odt"` |
| `GET /api/docs/{doc_id}/export?descendants=false` (or absent) | 1, on `{doc_id}` | `{"format":"markdown_zip","scope":"page","export_id":"<uuid v4>","doc_count":1}` |
| `GET /api/docs/{doc_id}/export?descendants=true` | one per page in `subset` | `{"format":"markdown_zip","scope":"subtree","export_id":"<uuid v4>","root_id":"<doc_id>","doc_count":N}` |
| `GET /api/workspace/export` | one per page in `readable_docs` | `{"format":"markdown_zip","scope":"workspace","export_id":"<uuid v4>","doc_count":N}` |

- `N` in `diagram_pictures` is `pictures.len()`, the pictures embedded in
  **this** file. The web retries a docx/odt POST once with no pictures after
  413/408/network error (`useExport.ts:22`, :77-80), so a retried export's
  row says 0.
- `fallbacks` is `built.fallbacks` (`knot_export::Fallbacks`,
  `crates/knot-export/src/assets.rs:49-54`, four `u32`s, no `Serialize`, no
  `total()`): written field by field with `json!`.
- `export_id` is one `Uuid::new_v4()` per zip request, shared by all its
  rows, so an operator can correlate one export across pages.
- `doc_count`, `export_id` and `fallbacks` are for operators (SQL). **They
  are never sent to the client.** `doc_count` would tell a root editor how
  many restricted subpages the exporter could read.
- Nothing else is stored: no file name (carries the title), no IP, no user
  agent.

**What counts as an export:** the server released the file, and it releases
a file only once its rows are written. Recorded in the handler after the
file was built and before the response is returned: the handler awaits
`audit::record_for_targets` (one INSERT for all of an export's pages, so all
or none are recorded). Exports **fail closed**: if the insert fails, or the
server has no database to write it to, the handler returns `503
audit.unavailable` (`http_error::audit_unavailable`, JSON envelope, message
"the export couldn't be recorded in the audit log") instead of the file,
and the failure is logged and counted in
`knot_audit_write_failures_total{action="doc.export"}`. Superseded: the
first build ran the insert on its own task (`spawn_record_for_targets`) so a
slow insert couldn't delay the file; that released files the audit log
might never record, and the owner decided against it.

The insert is bounded: `record_for_targets` gives up after
`audit::EXPORT_AUDIT_WRITE_TIMEOUT` (5 s, connection included) and runs the
INSERT in its own transaction under `SET LOCAL statement_timeout` of the
same length. An insert that hangs instead of failing (another session's lock
on `audit_events`, a lost synchronous standby; knot sets no
`statement_timeout` of its own) is therefore refused like a failed one:
`503 audit.unavailable`, logged and counted, well inside the request's 30 s
`TimeoutLayer`. Abandoned before its COMMIT, it never commits, so it leaves
no row for the file that wasn't sent. Superseded: the insert first ran
unbounded under the request timeout, so a stalled insert ended in an
uncounted 408 (labelled `client_error`) that the web then retried, for
another 30 s. Only a build that already ran close to the 30 s can still hit
the request timeout during the insert; a row that commits just as it fires
is for a file the client never got: accepted, over-recording is the safe
direction. In `knot_export_total` the 503
`audit.unavailable` counts as `outcome="server_error"`, not `busy` (the
response carries an `AuditUnavailable` marker extension).

Not recorded:

- Failures and refusals: 400 `export.bad_format`, 401, 403, 404, 409
  `doc.archived`, 413 `export.too_large`, 429, 500, 503 `export.busy`, 503
  `audit.unavailable`.
- 408 during the build: `TimeoutLayer` drops the handler before the audit
  line; the spawned build's result is thrown away. (A 408 during the audit
  insert, possible only after a build that took nearly 30 s, may or may
  not leave a row, see above.)
- `HEAD` on the two GET zip routes (axum runs the GET handler and builds the
  zip, but no body is delivered): record only when `method == GET`. A HEAD
  sends no file, so it is neither recorded nor refused for want of a row.
- `GET /api/docs/{id}/export/diagrams` (`export.rs:96`): the preflight the
  browser sends before every docx/odt POST (`useExport.ts:70-73`). The
  diagrams leave inside the docx/odt, counted as `diagram_pictures`.
- `GET /api/docs/{id}/markdown`, `GET /api/blobs/{id}`, `/p/*` (Non-goals).

Accepted: a docx/odt success whose response was lost to a network error is
retried by the web and recorded twice (two files were released). A cross-site
top-level navigation can trigger a GET zip export with the SameSite=Lax
session; the row is attributed to the victim, whose browser did receive the
file; bounded by the Export bucket (20/min). Moving zip exports to POST is out
of scope.

### Grant rows carry who and what (going forward)

`crates/knot-storage/src/grant_store.rs`:

- `put` (the `audit::record_in_tx(... "doc.grant" ...)` at :283) becomes
  `record_in_tx_with_data` with
  `{"principal":"user:<uuid>"|"group:<name>","role":"owner"|"editor"|"viewer","inherit":true|false}`
  built from `grant` (values already in scope).
- `delete` (`"doc.grant.delete"` at :348) becomes `record_in_tx_with_data`
  with `{"principal":"…"}`.
- `put_break_glass` is unchanged (already has data).

Old rows keep `{}` and read "changed access" / "removed access". This leaks
nothing new: `GET /api/docs/{id}/grants` already lists principals to any
reader (`grants.rs:33-62`). Existing tests assert action and actor only
(`restricted_grants_integration.rs:194-202`, `grants_membership.rs:359-378`).

### Storage (`crates/knot-storage/src/audit.rs`, free functions, runtime sqlx)

Update the module header: "audit_events writers (best effort on the pool,
fail closed for exports, transactional with `_in_tx`) and the per-page
reads."

```rust
/// One `action` row per id in `target_ids`, all with the same `data`, in one
/// INSERT, so all or none are written; an empty slice is a no-op. Gives up
/// after `EXPORT_AUDIT_WRITE_TIMEOUT` (5 s). Returns the error (after
/// logging and counting it once): the export routes refuse the file on it.
pub async fn record_for_targets(
    pool: &PgPool, workspace_id: Uuid, actor: Option<Uuid>, action: &str,
    target_kind: &str, target_ids: &[Uuid], data: &JsonValue,
) -> Result<(), AuditWriteError>; // Database(sqlx::Error) | TimedOut(Duration)
// BEGIN; SELECT set_config('statement_timeout', '5000ms', true);
// INSERT INTO audit_events (workspace_id, actor_id, action, target_kind, target_id, data)
// SELECT $1, $2, $3, $4, t.target_id, $6 FROM UNNEST($5::uuid[]) AS t(target_id);
// COMMIT  -- all under tokio::time::timeout(EXPORT_AUDIT_WRITE_TIMEOUT)
```

(There is no detached `spawn_record_for_targets`: exports fail closed.)
`record_with_data` (best effort: member changes, `quota.exceeded`) and
`record_for_targets` increment `knot_audit_write_failures_total{action}`
(action is a code constant, never a user value) next to their `warn!`.
Described in `crates/knot-obs/src/metrics.rs` in a new
`describe_audit_metrics()` called from `init` next to
`describe_limit_metrics()` (:106-107), with a test
`audit_metrics_are_described` like `limit_metrics_are_described` (:353):
"Failed audit writes, by action: one per failed insert, however many rows it
held. For doc.export each is one refused export (503 audit.unavailable, no
file sent), however many pages it covered, including an insert given up on
after 5 s; for other actions the writer is best effort and the request went
on". It counts failed writes, not rows: a refused 300-page workspace export
adds 1.

```rust
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DocAuditRow {
    pub id: i64, pub actor_id: Option<Uuid>, pub action: String,
    pub data: JsonValue, pub created_at: DateTime<Utc>,
}

/// One page of `doc_id`'s events, newest first. `before`: only ids below it.
/// `actions`: only these actions (`None` = all). Callers pass `limit + 1`.
pub async fn list_doc_events(
    pool: &PgPool, workspace_id: Uuid, doc_id: Uuid,
    actions: Option<&[&str]>, before: Option<i64>, limit: i64,
) -> Result<Vec<DocAuditRow>, sqlx::Error>;
// SELECT id, actor_id, action, data, created_at FROM audit_events
//  WHERE target_id = $2 AND workspace_id = $1 AND target_kind = 'doc'
//    AND ($3::text[] IS NULL OR action = ANY($3))
//    AND id < COALESCE($4::bigint, 9223372036854775807)
//  ORDER BY id DESC LIMIT $5
// The cursor bound is COALESCE, not `$4 IS NULL OR id < $4`: in the generic
// plan Postgres switches a prepared statement to after five runs, the OR
// form is a filter over every newer row of the page; COALESCE stays an
// index condition on (target_id, id). Pinned by an EXPLAIN (GENERIC_PLAN)
// test.

pub struct ActionCount { pub action: String, pub count: i64, pub last_at: DateTime<Utc> }
pub struct ExportGroup { pub format: Option<String>, pub scope: Option<String>,
                         pub count: i64, pub last_30_days: i64 }
pub struct ExporterCount { pub actor_id: Uuid, pub count: i64, pub last_at: DateTime<Utc> }
pub struct DocAuditStats {
    pub by_action: Vec<ActionCount>,      // ORDER BY count DESC, action
    pub export_groups: Vec<ExportGroup>,
    pub exporters: Vec<ExporterCount>,    // every exporter, ORDER BY last_at DESC
    pub last_export: Option<DocAuditRow>,
}
pub async fn doc_stats(pool: &PgPool, workspace_id: Uuid, doc_id: Uuid)
    -> Result<DocAuditStats, sqlx::Error>;
```

`doc_stats` runs four queries, all on `audit_events_target_idx` with the same
`WHERE target_id = $2 AND workspace_id = $1 AND target_kind = 'doc'`:

1. `SELECT action, count(*), max(created_at) … GROUP BY action`
2. `SELECT data->>'format', data->>'scope', count(*),
   count(*) FILTER (WHERE created_at > now() - interval '30 days')
   … AND action = 'doc.export' GROUP BY 1, 2`
3. `SELECT actor_id, count(*), max(created_at) … AND action = 'doc.export'
   AND actor_id IS NOT NULL GROUP BY actor_id ORDER BY max(created_at) DESC`
   (unbounded: one row per person who ever exported the page; the handler
   merges and caps, so the people count is exact)
4. `SELECT id, actor_id, action, data, created_at … AND action = 'doc.export'
   ORDER BY id DESC LIMIT 1`

### Read API: `GET /api/docs/{id}/audit?category=&cursor=&limit=`

**Registration.** In `doc_id_routes` (`crates/knot-server/src/routes/api/docs.rs`),
after the contributors route:
`.route("/api/docs/{id}/audit", get(crate::routes::api::audit::doc_audit))`,
so `require_doc_role_mw` applies. New module
`crates/knot-server/src/routes/api/audit.rs` (`pub mod audit;` in
`routes/api/mod.rs`) with a `//!` header listing the route. Add the route to
the `docs.rs:1-13` list.

**Gate.** Move `require_editor` from `history.rs:30-45` to
`auth/require_doc_role.rs` next to `require_viewer`, make it `pub`, take
`&Request<Body>`, keep its doc comment, re-export it in `auth/mod.rs`, and
import it in `history.rs` (no behaviour change). Answers:

| Caller | Answer |
|---|---|
| no session | 401 `auth.session_required` |
| unknown page, other workspace, restricted without grant, purged | 403 `acl.no_grant` (middleware, uniform) |
| effective role Viewer | 403 `acl.editor_required` |
| Viewer on an archived page | 404 `doc.not_found` (middleware) |
| Editor or Owner, archived or not | 200 |

The effective doc role from `EffectiveDocRole` decides; never `ctx.role`.

**Query**, parsed by hand from `Option<String>` fields like `history.rs`
`EntriesQuery`, an empty value meaning none:

- `category`: `all` (default) | `exports` | `access` | `changes`.
- `cursor`: the opaque `id` of the last event already shown (the previous
  page's `next_cursor`); the page holds older events.

**Event ids are opaque.** The stored `audit_events.id` is never sent: one
export writes its rows in one statement, so their ids are consecutive, and a
reader who saw the raw ids on the pages they can open would count, from the
gaps, the pages of the export they can't open (exactly what `doc_count` is
withheld for). Each event's `id`, and so `next_cursor`, is the stored id run
through a keyed permutation of 64-bit numbers (an 8-round Feistel network
with HMAC-SHA256 rounds, keyed by a key derived from `session_key` and
tweaked by the page id; `routes/api/audit/event_token.rs`, `EventTokens`),
written as 16 lowercase hex digits. The server opens `cursor` back to the
stored id; text that is not 16 hex digits, or opens to no positive id, is
`bad cursor`. Every replica shares `session_key`, so cursors work across
replicas; rotating it only invalidates open cursors.
- `limit`: 1..=200, default 50.
- Bad value: 400 `audit.bad_request`, message `bad category` / `bad cursor` /
  `bad limit`, in the `json_err` envelope.

**Categories** (a `const` table in `routes/api/audit.rs`):

| category | actions |
|---|---|
| `exports` | `doc.export` |
| `access` | `doc.grant`, `doc.grant.delete`, `admin.grant`, `doc.restrict`, `doc.unrestrict` |
| `changes` | `doc.create`, `doc.rename`, `doc.move`, `doc.duplicate`, `doc.archive`, `doc.restore`, `doc.purge`, `doc.template.mark`, `doc.template.unmark` |

An action in no set (a future one) shows under `all` with `"category":"other"`.

**Handler** `doc_audit(State, Path<Uuid>, Query<AuditQuery>, req: Request) -> Response`:

1. `require_editor`; clone `AuthContext`. Parse the query. No pool: 500
   `internal`.
2. `list_doc_events(…, limit + 1)`; if `limit + 1` rows came, drop the last
   and set `next_cursor` to the sealed id of the last row kept, else `null`.
3. Only without `cursor`: `doc_stats` (always over all categories). With a
   cursor, `"stats": null`.
4. People: one `knot_history::load_people` over the actor ids of the events,
   the exporters, the last export, and the `user:` principals of grant rows.
   Each is rendered with `history_author_view`; an id with no `users` row is
   `{"key":null,"name":null,"former":true}`. Never an email.
5. Roots: the distinct `data.root_id` of subtree export rows whose target is
   not the root go through `history::readable_sources` with
   `acl.access(ctx.workspace_id, id, ctx.user_id)`; titles of readable roots
   via `state.docs` `get(id)`. A root not readable or not found is hidden.
6. Respond 200 with `Cache-Control: private, no-store`. Store errors: log and
   500 `internal`.

**Actors.** `actor` is an `AuthorView` or `null`. `actor_kind`:
`"user"` when `actor_id` is set; for `actor_id NULL`: `"admin_cli"` on
`admin.grant`, `"retention"` on `doc.purge`, `"system"` otherwise.

**`details`, whitelisted per action; raw `data` is never sent:**

| action | details |
|---|---|
| `doc.export` | `format`, `scope`; `diagram_pictures` when present (docx/odt); for `scope:"subtree"` on a row whose target is not `root_id`: `"root": {"id","title"} \| null` and `"root_hidden": bool` |
| `doc.grant` | `principal`, `role`, `inherit` when the row has them, else `{}` |
| `doc.grant.delete` | `principal` when present, else `{}` |
| `admin.grant` | `principal`, `role`, `inherit`, `"break_glass": true` |
| every other action | `{}` |

`principal` is `{"kind":"user","person": AuthorView}` for `user:<uuid>`,
`{"kind":"group","name":"<name>"}` for `group:<name>`, and omitted when
unparsable. Editors see break-glass rows: transparency to the page's
responsible people, and the actor is a label, not a person.

**Stats.** From `DocAuditStats`, in the handler:

- `total` = sum of `by_action` counts.
- `exports.total`, `.last_30_days`, `.by_format` (`{"docx","odt","markdown_zip"}`,
  zero keys omitted), `.by_scope` (`{"page","subtree","workspace"}`) from
  `export_groups` (null format/scope ignored for the maps, counted in total).
- `exports.by_person`: exporters rendered; every former/anonymized/missing
  person merged into **one** `{"key":null,"name":null,"former":true}` row
  (count summed, `last_at` max) so the number of anonymized people doesn't
  leak; sorted by `last_at` desc; first 50; `by_person_truncated` when more.
- `exports.people` = distinct named exporters + 1 if any merged former row
  (exact, not capped at 50).
- `exports.last`: `{at, actor, actor_kind, format, scope}` or `null`.

**Response 200** (first page):

```json
{
  "events": [
    {"id": "5c1e9a07d3b4f268", "at": "2026-11-13T09:12:44Z", "action": "doc.export",
     "category": "exports", "actor_kind": "user",
     "actor": {"key": "0b6c…", "name": "Ada", "former": false},
     "details": {"format": "docx", "scope": "page", "diagram_pictures": 2}},
    {"id": "e07b42c9a1d65f30", "at": "2026-11-12T16:03:10Z", "action": "doc.export",
     "category": "exports", "actor_kind": "user",
     "actor": {"key": "77a1…", "name": "Bo", "former": false},
     "details": {"format": "markdown_zip", "scope": "subtree",
                 "root": null, "root_hidden": true}},
    {"id": "9d2f6a18c4e07b35", "at": "2026-11-10T08:00:00Z", "action": "doc.grant",
     "category": "access", "actor_kind": "user",
     "actor": {"key": "77a1…", "name": "Bo", "former": false},
     "details": {"principal": {"kind": "user",
                 "person": {"key": "c3d4…", "name": "Cy", "former": false}},
                 "role": "editor", "inherit": true}},
    {"id": "26b8e0f4a9c13d57", "at": "2026-10-01T10:00:00Z", "action": "admin.grant",
     "category": "access", "actor_kind": "admin_cli", "actor": null,
     "details": {"principal": {"kind": "group", "name": "ops"},
                 "role": "owner", "inherit": false, "break_glass": true}}
  ],
  "next_cursor": "26b8e0f4a9c13d57",
  "stats": {
    "total": 37,
    "by_action": [{"action": "doc.export", "count": 9, "last_at": "2026-11-13T09:12:44Z"}],
    "exports": {
      "total": 9, "people": 3, "last_30_days": 4,
      "last": {"at": "2026-11-13T09:12:44Z", "actor_kind": "user",
               "actor": {"key": "0b6c…", "name": "Ada", "former": false},
               "format": "docx", "scope": "page"},
      "by_format": {"docx": 5, "odt": 1, "markdown_zip": 3},
      "by_scope": {"page": 7, "subtree": 1, "workspace": 1},
      "by_person": [{"actor": {"key": "0b6c…", "name": "Ada", "former": false},
                     "count": 6, "last_at": "2026-11-13T09:12:44Z"}],
      "by_person_truncated": false
    }
  }
}
```

`stats` is `null` on cursor pages. `id` and `next_cursor` are opaque strings
(see "Event ids are opaque" above).

**Rate limit.** No bucket: an editor-only, index-backed read, and reads are
unlimited by design. Pin it: add `(Method::GET, "/api/docs/{id}/audit")` to
`reads_and_unlisted_routes_are_not_limited` (`rate_limit/http.rs`). The
recorded export routes keep their `Export` bucket, which also caps audit
growth from exports at 20/min/user.

### Recording points (exact)

All await `audit::record_for_targets` before returning the file and return
`http_error::audit_unavailable()` (503 `audit.unavailable`, no file) when
it fails or `state.pool` is `None` (no database to record in, so no export).

1. **docx/odt**, `crates/knot-server/src/routes/api/export.rs` `export_response`:
   - Replace the `user_id`-only read at :200 with
     `let Some(auth_context) = req.extensions().get::<AuthContext>().cloned() else { … 401 … };`
     and use `auth_context.user_id` where `user_id` is used. This is before
     `read_pictures(req)` consumes the request.
   - After `read_pictures` returns: `let diagram_picture_count = pictures.len();`
     (before `pictures` moves into the spawned task).
   - After `let built = match build.await { Ok(Ok(built)) => built, … };`
     and before `export::file_name`, await `record_for_targets(pool,
     auth_context.workspace_id, Some(auth_context.user_id), "doc.export",
     "doc", &[doc_id], &json!({...}))` with the data in the table above; on
     `Err` return `audit_unavailable()`. Not in the build task, so a 408
     before the file is ready writes nothing.
   - The `Recorded` drop guard keeps its semantics; it now stores the
     `outcome` label, computed from the response by `outcome(&Response)`,
     which maps a response carrying the `AuditUnavailable` marker to
     `server_error` and every other status as before (`status_outcome`).
     `diagrams` is unchanged.
   - Module header: says a built export is recorded before the file goes
     out, and refused with 503 `audit.unavailable` when it can't be.
2. **Zip page/subtree**, `routes/api/export_import.rs` `export_doc`:
   - First line: `let is_get = req.method() == Method::GET;`.
   - Before the `write_export_zip` call:
     `let exported_doc_ids: Vec<Uuid> = subset.iter().map(|doc| doc.id).collect();`
     (**`subset`, never `subtree_ids`**, which still holds unreadable pages).
   - `let response = write_export_zip(...).await;` then, when `is_get &&
     response.status() == StatusCode::OK`, `if let Err(refusal) =
     record_zip_export(&state, &ctx, &exported_doc_ids, data).await { return
     refusal; }`, then `response`, with scope `subtree` + `root_id: doc_id`
     when `q.descendants`, else `page`. `write_export_zip` builds the whole
     zip in memory (`Body::from(buf)`), so nothing has been sent before the
     insert.
3. **Workspace zip**, `export_workspace`: same pattern with
   `readable_docs` ids collected before the move, scope `workspace`, no
   `root_id`.
4. `record_zip_export(state, ctx, ids, scope_fields) -> Result<(),
   Response>` is a private async helper in `export_import.rs` that adds
   `format`, `export_id` and `doc_count`, awaits
   `audit::record_for_targets(pool, ctx.workspace_id, Some(ctx.user_id),
   "doc.export", "doc", ids, &data)` and maps a failure (or no pool) to
   `Err(audit_unavailable())`.
5. `write_export_zip` is unchanged. Module header: the zip is built in
   memory, recorded per included page as `doc.export`, and only then sent;
   a GET whose rows can't be written is refused with 503
   `audit.unavailable`; HEAD is neither recorded nor refused.
6. **Web:** `useExport.ts` (docx/odt) and `downloadFile.ts` (the page and
   workspace Markdown zips, fetched, not navigated) show an error toast
   "Export blocked: it couldn't be recorded in the audit log. Try again
   later." on `audit.unavailable` (`features/export/auditRefusal.ts`). The
   docx/odt retry without pictures stays on 413/408/network error only,
   never on a 503. A stalled audit insert answers 503 within 5 s (see "What
   counts as an export"), so it is not retried; a 408 now means a slow
   build, which the retry without pictures is for.

### Client

**Route.** A real page, sibling of `doc/:id` in `web/src/routes.tsx`:
`{ path: "doc/:id/audit", element: <Lazy><DocAuditPage /></Lazy> }` with
`const DocAuditPage = lazy(() => import("./features/docs/DocAuditPage"))`,
before the `:code` catch-all. A real URL (bookmark, back, new tab); DocPage
unmounts and its collab socket closes, fine for a read-only log.

**Files** (all under `web/src/features/docs/` unless noted):

- `audit.api.ts`: `auditApi.list(docId, {category?, cursor?, limit?})` via
  `apiFetch` on `/api/docs/${encodeURIComponent(docId)}/audit` with
  `URLSearchParams` (omit unset), validated with `parse(DocAuditPage, r.ok)`.
- `web/src/lib/validators.ts`: `auditAuthorSchema`
  (`{key: nullable string, name: nullable string, former: boolean}`),
  `docAuditEventSchema` (`category` and `actor_kind` plain strings,
  `details: v.fallback(v.record(v.string(), v.unknown()), {})`),
  `docAuditStatsSchema`, `docAuditPageSchema`; exported types and values in
  the file's `const xSchema / export type X / export const X` pattern.
- `useDocAudit.ts`: `useInfiniteQuery({ queryKey: ["doc-audit", id, category],
  queryFn: ({pageParam}) => unwrapResult(await auditApi.list(id, {category, cursor: pageParam})),
  initialPageParam: undefined as string | undefined,
  getNextPageParam: (page) => page.next_cursor ?? undefined,
  refetchOnMount: "always", staleTime: 0, enabled })`. `unwrapResult` from
  `features/notifications/useNotifications.ts` (throws with `status` and
  `code`, so 4xx are not retried). Stats come from `pages[0].stats`.
- `auditText.ts`: pure `describeAuditEvent(event): { actor: string; text: string; badge?: string }`
  and `actorLabel(actor, actor_kind)`.
- `DocAuditPage.tsx` (default export), `AuditStats.tsx`, `AuditEventList.tsx`.

**Layout.** `<section className="mx-auto max-w-[760px] px-4 sm:px-6 py-8" data-testid="doc-audit-page">`.

1. `Breadcrumb items={[{title:"Documents"},{id, title: meta.title},{title:"Audit log"}]}`
   (title from the cached `["doc", id]` query via `docsApi.get`).
2. h1 "Audit log" (`text-2xl font-bold text-fg`), an "Archived" badge when
   `archived_at`; p `text-sm text-fg-muted mt-1`: "Who exported this page,
   changed its access or changed it. Only its editors and owners see this."
3. **Exports** (`data-testid="audit-stats"`), h2 in
   `text-sm font-semibold uppercase tracking-wider text-fg-muted`:
   - `<dl>` tiles in `grid grid-cols-2 sm:grid-cols-4 gap-2`, each
     `bg-surface border border-border rounded-lg p-3`: Exports
     (`audit-stat-exports`), People (`audit-stat-people`), Last 30 days
     (`audit-stat-30d`), Last export (`audit-stat-last`: relTime, below it
     "Word · Ada"; "Never" when null; absolute time in `title`).
   - Format line of badges (`inline-flex items-center px-2 h-5 rounded-full
     text-[11px] font-medium bg-muted text-fg-muted`): "Word 5 · OpenDocument
     1 · Markdown (zip) 3", zero counts omitted; under it, only when not all
     are `page`: "1 as part of a subpage export · 1 in a workspace export".
   - "Exported by" list (`audit-exporters`), stacked rows
     `flex items-center gap-3 py-2 border-t border-border`: `Avatar(name,
     seed=key)`, name or "Former member", "6 exports", "last 2h ago". When
     `by_person_truncated`: "Showing the 50 most recent exporters."
   - No exports: card with a `Download` icon, "Nobody has exported this page
     since exports started being recorded." (`audit-exports-empty`).
4. **Other activity**: one line from `by_action` (non-export): "Access changes
   3 · Changes 30", plus a total.
5. **Events** (`audit-events`): filter chips All / Exports / Access / Changes
   (`<button aria-pressed>` in `role="group" aria-label="Show events"`,
   `data-testid="audit-filter-{all|exports|access|changes}"`), a `<ul>` of
   `<li data-testid="audit-event" data-action={action}>`: Avatar (or a muted
   `ShieldCheck` for `admin_cli`, `Clock` for `retention`/`system`), sentence
   with the actor in `font-medium`, `<time dateTime title={toLocaleString()}>`
   with `relTime`. "Load more" `Button variant="secondary"`
   (`audit-load-more`) while `hasNextPage`. Filter empty: "No exports
   recorded." / "No access changes recorded." / "No changes recorded." /
   "Nothing recorded yet."
6. **Scope note** (`audit-scope-note`, `text-[12px] text-fg-muted border-t
   border-border pt-4 mt-8`): "Recorded: downloads as Word, OpenDocument and
   Markdown (zip), also as part of a subpage or workspace export, and access
   and page changes. Not recorded: opening or reading the page, the Markdown
   view and its Copy button, attachments, and public link views. Exports
   made before this version of knot were not recorded."

No tables: every list is stacked, so 375 px has no horizontal scroll.

**Sentences** (`auditText.ts`):

- `doc.export` page: "exported this page as Word (.docx)" / "as OpenDocument
  (.odt)" / "as Markdown (zip)", plus ", with 2 diagrams" when
  `diagram_pictures > 0`.
- subtree on the root (no `root` key): "exported this page and its subpages
  as Markdown (zip)".
- subtree on a descendant: "exported this page as part of <Link to
  /doc/{root.id}>{root.title}</Link> and its subpages", or "… as part of a
  page you can't open" when `root_hidden`.
- workspace: "exported the whole workspace, including this page".
- `doc.grant` with details: "gave Cy editor access" (group: "gave group ops
  editor access"), "(this page only)" when `inherit:false`; old rows:
  "changed access to this page". `doc.grant.delete`: "removed Cy's access" /
  "removed access to this page". `admin.grant`: "granted Cy owner access
  (break-glass)".
- `doc.restrict` "restricted this page", `doc.unrestrict` "lifted the
  restriction", `doc.create` "created this page", `doc.rename` "renamed this
  page", `doc.move` "moved this page", `doc.duplicate` "created this page as
  a copy", `doc.archive` "moved this page to the archive", `doc.restore`
  "restored this page", `doc.purge` "deleted this page for good",
  `doc.template.mark` "saved this page as a template", `.unmark` "removed it
  from templates"; unknown: the raw action.
- Actor labels: name; "Former member" when `former`; `admin_cli` "An
  administrator (command line)"; `retention` "Retention job"; `system`
  "System".

**States.** Loading: `<p className="text-sm text-fg-muted">Loading…</p>`.
`acl.editor_required`: card "Only editors and owners of this page can see its
audit log." with a "Back to page" link (`audit-forbidden`); also rendered
without calling the API when `useEffectiveRole(id).doc === "viewer"`
(`enabled: effRole !== "viewer"`; the server stays the guard).
`acl.no_grant` / `doc.not_found`: "This page doesn't exist or you don't have
access to it." (`audit-no-access`). Other errors on the first load: inline
`<p className="text-sm text-destructive">Couldn't load the audit log.</p>` and
a "Try again" button calling `refetch()` (`audit-retry`); no toasts. When a
filter switch fails after the log has loaded (TanStack keeps the
placeholder only while pending), the page keeps the last stats and the
filter chips, with the failed filter pressed; the event list says
"Couldn't load these events." with its own "Try again"
(`audit-events-failed`, `audit-events-retry`), and no other filter's events
stand in. Another chip switches back (a loaded filter shows from cache).

**Entry points** (effective owner or editor, **including archived pages**,
i.e. `claimsEditShortcut`, `DocPage.tsx:378`, not `canEditDoc`):

- `DocPage.tsx` action row, right after the History IconButton:
  `{claimsEditShortcut && <Link to={`/doc/${id}/audit`} data-testid="open-audit" aria-label="Audit log" title="Audit log" className="inline-flex items-center justify-center rounded h-9 w-9 transition-colors ease-swift duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent text-fg-muted hover:text-fg hover:bg-muted"><ScrollText size={16} aria-hidden /></Link>}`
  (a Link, since it navigates; classes copied from `IconButton` md).
- `TreeRow.tsx`, inside the first `if (canEdit)` block after the move items:
  `items.push({ label: "Audit log", testId: "ctx-audit", onSelect: () => void nav(`/doc/${node.id}/audit`) })`.
- Not in `DocDock`.

**Transparency for exporters.** Viewers can export, so they should know
editors see it:

- `ExportMenu.tsx`: below the last item, a separator and
  `<p role="none" data-testid="export-audit-note" className="px-3 py-1.5 text-[11px] text-fg-muted">Exports are recorded in the page's audit log.</p>`.
  Focus never lands on it (it is no menuitem), so the menu names it as its
  description, `aria-describedby` on `role="menu"`, and screen readers read
  it when the menu opens.
- `web/src/features/workspace/SettingsPage.tsx`: the same sentence ("…in each
  page's audit log.") under the workspace export button
  (`workspace-export-audit-note`).

## Tests (behaviours, each written red first)

**Storage** (`crates/knot-storage/tests/audit_read.rs`, new, `fresh_db()`):

1. `list_doc_events_is_newest_first_and_only_this_page`: excludes other
   pages, other workspaces and `target_kind='user'` rows.
2. `list_doc_events_pages_below_the_cursor_and_filters_by_action`.
3. `record_for_targets_writes_one_row_per_page_and_nothing_for_none`.
3a. `record_for_targets_returns_a_failed_insert_and_counts_it`: a trigger
   raises on `doc.export` inserts; `Err`, `knot_audit_write_failures_total`
   at 1 (one per failed insert, not per row) through a local recorder, and
   no row of the two-page insert.
3b. `record_for_targets_gives_up_on_a_stalled_insert_and_counts_it`: with
   `audit_events` locked in SHARE mode, `Err` within
   `EXPORT_AUDIT_WRITE_TIMEOUT` plus a margin, the counter at 1, and no row
   once the lock goes (the abandoned insert never commits).
4. `doc_stats_counts_actions_formats_scopes_and_exporters`: NULL actors not
   in `exporters`; 30-day window.
5. `audit_target_index_exists`: `pg_indexes` name `audit_events_target_idx`,
   indexdef contains `(target_id, id)` (style of `quota.rs:198-230`).
5a. `the_cursor_bound_is_an_index_condition_in_the_generic_plan`:
   `EXPLAIN (GENERIC_PLAN)` of `list_doc_events_sql()` over many pages'
   rows has `target_id = $2` and `id <` in one Index Cond.
6. `grant_rows_carry_principal_role_and_inherit` and
   `grant_delete_rows_carry_principal` (in `tests/grants.rs`).

**Server integration** (`crates/knot-server/tests/doc_audit_integration.rs`,
new; fixture copied from `history_entries_integration.rs` with Owner, Editor
and Viewer logged in; export helpers like `export_integration.rs`):

1. `editors_read_the_audit_log_and_viewers_are_refused`: owner and editor 200
   with `Cache-Control: private, no-store`; viewer 403
   `acl.editor_required`; anonymous 401; archived page: editor 200, viewer
   404 `doc.not_found`; restricted page without grant: editor and the
   workspace owner 403 `acl.no_grant`, body without ids or names. The page
   role decides both ways: the workspace viewer who owns a restricted page
   reads its log (200); a workspace editor with only a `viewer` grant on a
   restricted page is 403 `acl.editor_required`.
2. `a_viewers_docx_export_is_recorded_with_who_and_format` (and odt): the
   editor sees `doc.export`, `actor.name` = viewer, `format`, `scope:"page"`,
   `diagram_pictures`; stats `exports.total == 1`, `people == 1`,
   `last.format == "docx"`.
3. `the_diagrams_preflight_and_markdown_read_record_nothing`.
4. `refused_and_failed_exports_record_nothing`: 400 bad format, 409 on an
   archived page, 413 too large, 503 busy (`export_permits` of 0, short
   `export_busy_wait`).
5. `a_zip_page_export_is_recorded_once`.
6. `a_subtree_export_records_every_included_page_and_no_restricted_one`:
   rows on root and readable children with a shared `export_id` (SQL); a
   restricted child the exporter can't read has no row (SQL).
7. `a_subtree_row_hides_a_root_the_reader_cannot_open`: an editor with a
   grant only on a restricted child sees `root: null, root_hidden: true`.
8. `a_workspace_export_records_each_readable_page`.
9. `head_on_a_zip_export_records_nothing`.
9a. `an_export_that_cannot_be_recorded_is_refused`: a trigger makes every
    `doc.export` insert raise; docx, odt, the page, subtree and workspace
    zips each answer 503 `audit.unavailable` with no `Content-Disposition`
    and no zip bytes, no row is written, and
    `knot_audit_write_failures_total{action="doc.export"}` rises by one per
    refusal; HEAD still answers 200; with the trigger gone the exports go
    out again.
9b. `an_export_waits_for_a_stalled_audit_write_then_goes_out`: with
    `audit_events` locked in SHARE mode, a docx and a zip export don't
    answer; once the lock goes they answer 200 with the row already there.
    (The export helpers in this file assert the row exists as soon as a 200
    arrives.)
9c. `a_stalled_audit_write_refuses_the_export_before_the_request_timeout`:
    with `audit_events` locked in SHARE mode for good, a docx and a zip
    export each answer 503 `audit.unavailable` with no file within
    `EXPORT_AUDIT_WRITE_TIMEOUT` plus a margin (not a 408 after 30 s), the
    counter rises by 2, and no row lands once the lock goes. 9a and 9c read
    the binary-wide counter, so they hold one test lock between them.
10. `operator_fields_never_reach_the_client`: no `doc_count`, `export_id`,
    `fallbacks`, `source_id` anywhere in the body.
10a. `event_ids_and_cursors_do_not_reveal_the_stored_ids`: a subtree export
    that includes a page the editor can't open; on the pages they can open,
    event ids and `next_cursor` are 16-digit opaque strings, distinct, none
    a stored id.
11. `stats_on_the_first_page_only_and_independent_of_category`.
12. `paging_and_bad_input`: `limit=1` gives `next_cursor`, following it
    yields the rest without gaps or repeats (ids compared as
    `EventTokens::seal` of the stored ids); `cursor=abc`, `cursor=0`, a
    stored id as cursor, `limit=0`, `limit=201`, `category=foo` are 400
    `audit.bad_request` JSON.
13. `category_filter_keeps_paging_correct`.
14. `system_and_former_actors`: break-glass row `actor:null`,
    `actor_kind:"admin_cli"`; retention purge row `"retention"` (insert by
    SQL on a live page); an actor id with no user row renders former, id not
    echoed; two former exporters merge into one `by_person` row and count
    once in `people`.
15. `grant_rows_name_the_principal`: grant via API, the log shows
    `principal.person.name`, `role`, `inherit`.
16. Existing tests stay green: `history_entries_integration`,
    `history_integration` (the moved `require_editor`), export unit tests,
    `restricted_grants_integration`, `grants_membership`.

**Unit** (in crate): `EventTokens` (round trip, page- and key-bound,
consecutive ids give unrelated tokens, malformed text refused), the details
whitelist (unknown data keys never pass),
`actor_kind` mapping, every action written in the codebase is in exactly one
category (list of writer action strings), `reads_and_unlisted_routes_are_not_limited`
with the new route, `audit_metrics_are_described`, and in `export.rs`
`outcome_follows_the_status` and
`an_export_refused_for_its_audit_row_is_a_server_error_not_busy`
(`outcome(&audit_unavailable()) == "server_error"`, not `busy`).

**Web Vitest** (`cd web && NODE_OPTIONS=--no-experimental-webstorage npx pnpm test`):

- `audit.api.test.ts`: URL, encoding, params omitted when unset, error
  envelope passthrough.
- `auditText.test.ts`: every action and scope, old rows without data, hidden
  root, null and former actors, each `actor_kind`.
- `DocAuditPage.test.tsx` (`vi.mock("./audit.api")`, QueryClient
  `retry:false`, MemoryRouter at `/doc/d1/audit`): tiles, format line,
  exporters with "Former member", sentences, filter sends `category=exports`,
  Load more sends `cursor`, `audit-forbidden` on 403 and without a request
  for a viewer role, `audit-no-access`, generic error with retry, a failed
  filter switch keeps the stats and chips with its own retry, empty
  states, scope note.
- `DocPage.audit.test.tsx` (mock `../editor/KnotEditor` as
  `DocPage.archived.test.tsx`): `open-audit` for owner and editor, also
  archived; not for viewer.
- TreeRow: `ctx-audit` for editors, not viewers. ExportMenu: the note
  renders and is the menu's accessible description.
- Fail closed, each on a 503 `audit.unavailable`: `useExport.test.ts` "says
  the export was blocked, without a retry, when the audit log couldn't
  record it" (the "Export blocked" toast, one request, no retry without
  pictures); `ExportMenu.test.tsx` "says when a Markdown zip is blocked
  because the audit log couldn't record it"; `SettingsPage.test.tsx` "says
  the export was blocked when the audit log couldn't record it" (workspace
  zip).
- Also `npx pnpm tsc` and `npx pnpm lint`.

**e2e** (`e2e/flows/doc-audit.spec.ts`, `test.beforeAll(reset)`, a per-run
`KNOT_E2E_DB` because of the migration):

1. Owner sets up, seeds a page, invites an editor and a viewer via /members.
2. Viewer exports Word (`doc-export` → `export-docx`, `waitForEvent("download")`),
   sees `export-audit-note`, has no `open-audit`; `/doc/{id}/audit` shows
   `audit-forbidden`.
3. Owner exports Markdown with subpages from the parent.
4. Editor clicks `open-audit`: `audit-stat-exports` = 1 and an
   `audit-event[data-action="doc.export"]` naming the viewer and Word; on
   the child page's log, "as part of".
5. `mobile.spec.ts`: add `open-audit` to the in-viewport list, update the
   "nine controls" comment, and assert no horizontal overflow on
   `/doc/{id}/audit` at 375 px.

**Gates:** `cargo fmt --check`; `cargo clippy --workspace --all-targets
--all-features -- -D warnings`; `cargo test --workspace` (nextest is not
installed locally); web tsc, lint, vitest; `make helm.check`; e2e on a
throwaway compose Postgres (own project, tmpfs). If Docker is down: `cargo
test --workspace --no-run` and report which DB tests did not run.

## Docs to update

- **CHANGELOG.md `[Unreleased]`:**
  - Upgrading: `20261113120000_audit_doc_index` builds one index on
    `audit_events` under SHARE, taken in bounded attempts
    (`knot_lock_tables`, 5 s lock timeout, gives up after 45 s instead of
    queueing); page create, rename, move, archive, restore, duplicate and
    purge (the retention job's too), template mark/unmark, restrict, grant
    changes and member invites, role changes and removals pause while it
    builds, edits don't. `audit_events` now grows by
    one row per exported page (a workspace export writes one per page); there
    is still no retention.
  - Added: "Each page has an audit log for its editors and owners: who
    exported it (Word, OpenDocument, Markdown zip, also as part of a subpage
    or workspace export), when and how, and who changed its access or the
    page (#TBD)". "`GET /api/docs/{id}/audit` lists a page's audit events and
    export stats for its editors". "Exports are recorded as `doc.export`
    before the file is sent, and refused with 503 `audit.unavailable` when
    they can't be (fail closed)". "`knot_audit_write_failures_total`".
  - Changed: "`doc.grant` and `doc.grant.delete` audit rows now carry the
    principal (and role and inherit)".
- **deploy/helm/knot/README.md:** "Upgrading from 0.9.10 or 0.9.11": a fourth
  migration in the list, its lock text, add `20261113120000` to the halt query
  and the `--timeout` arithmetic (one more 45 s loop), and the optional manual
  `CREATE INDEX CONCURRENTLY`. A new Operational notes bullet **Audit log**:
  what is recorded and not, growth per export, no retention, that an audit
  database failure blocks exports with 503 `audit.unavailable`, the
  `knot_audit_write_failures_total` metric (worth an alert); point the
  existing `quota.exceeded` audit mention to it.
- **.claude/skills/knot-api/SKILL.md:** a row `| **Page audit log** | GET
  /api/docs/{id}/audit?category=all|exports|access|changes&cursor=<next_cursor>&limit=50
  → {"events":[…],"next_cursor","stats"} (stats on the first page only).
  Editor+ |`; a Gotcha: "File exports (docx/odt, Markdown zip, workspace) are
  recorded as `doc.export` with your name and show on each included page's
  audit log; `GET …/markdown` and blob downloads are not"; and that an
  export the server can't record is refused with 503 `audit.unavailable`
  (no file; retry later, not without pictures); add "page audit log" to the
  front-matter description.
- **Module headers:** `docs.rs:1-13`, `export.rs:1-8`, `export_import.rs:1-12`,
  new `audit.rs`, `knot-storage/src/audit.rs`.
- **Process:** plan `docs/superpowers/plans/2026-11-13-doc-audit-page.md`,
  outcome `docs/superpowers/research/2026-11-13-doc-audit-page-outcome.md`,
  a row in `docs/superpowers/README.md` after the rate-limits row.

## Rollout

One forward-only migration, no config, no Helm values, no flag. Exports are
recorded from the first new pod on; exports served by old pods during the
rollout are not. An older binary ignores `doc.export` rows. Old rows render
with labels only.

## Decisions the owner may want to revisit

1. **Markdown view and API reads are not exports.** `GET …/markdown` (also
   what API scripts use to "read as markdown") leaves no trace. Alternative:
   an opt-in `?download=1` recorded as `format:"markdown"`.
2. **Fail closed. Decided by the owner 2026-11-13** ("an export without an
   audit shouldn't work"). An export whose audit rows can't be written is
   refused with 503 `audit.unavailable` and no file; the failure is logged
   and counted in `knot_audit_write_failures_total{action="doc.export"}`.
   The cost: an audit database outage blocks every file export. An insert
   that hangs is given up on after 5 s and refused the same way (503,
   counted), so it no longer runs the export into the request timeout. Other
   best-effort audit writers (member changes, `quota.exceeded`) are
   unchanged.
3. **One row per included page.** A workspace export of N pages writes N rows
   and stats count it once per page. Alternative: one event plus a link
   table.
4. **Editors see everyone's exports, including viewers', and break-glass
   admin grants.** Viewers are told by the ExportMenu note.
5. **No retention.** The table grows forever; deleting rows is an operator
   decision. A `KNOT_AUDIT_RETENTION_DAYS` job (archive.rs pattern) is
   deferred.
6. **Plain `CREATE INDEX` under SHARE** rather than the two-file
   `CONCURRENTLY` pattern; fine unless production `audit_events` is large.
7. **Scope boundary** is static copy ("before this version"), not a date
   read from `_sqlx_migrations`.
8. **Not shown on this page:** copies made from it, an ancestor's
   archive/restore/purge, a purged page's log.
9. **No rate-limit bucket** for the read route.

## Left to the builder

- Exact Tailwind spacing within the recipes above, icon choice beyond
  `ScrollText`, and sentence wording within the rules given.
- Whether `doc_stats` runs its four queries sequentially or with `tokio::join!`
  on the pool.
- Splitting `routes/api/audit.rs` into handler and render modules if it
  passes ~300 lines.
