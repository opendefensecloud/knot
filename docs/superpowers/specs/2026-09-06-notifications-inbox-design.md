# Notifications and inbox — design

**Date:** 2026-09-06
**Status:** Approved (brainstorm)
**Origin:** the deferred "Plan 19.5 — Mention push bridge" in
`docs/superpowers/research/2026-06-03-plan19-outcome.md:72-81`, widened from a
toast to an inbox.

## Context

knot has no way to tell you that something happened. The one event that was
meant to reach a user — an `@mention` in a comment — is wired at both ends and
connected in the middle to nothing.

`broadcast_mentions` (`crates/knot-server/src/routes/api/comments.rs:143`)
extracts handles from a comment body, resolves them to workspace member ids, and
fires `pg_notify('comment_mentions', …)` with a JSON payload. No process in the
codebase ever runs `LISTEN` for that channel. `bus_pg` subscribes only to
`doc:<id>` and `presence:<id>` (`crates/knot-crdt/src/bus_pg.rs:208-211`), and
the two `PgListener` background tasks that exist — `comments_listener.rs` and the
ACL invalidation listener — take `doc_comments` and `acl_invalidate`.

On the other end, `KnotProvider` reserves `MSG_MENTION` (type 4) and maintains a
`mention` listener list (`web/src/features/editor/KnotProvider.ts:7-8,42,195`)
that cannot fire. The file has said so in a comment since 2026-06-03.

So mentioning a colleague today does nothing they will ever see. The same is true
of every other event worth knowing about: a reply in a thread you started, a
checklist item assigned to you, a task going overdue, a page shared with you.

This spec covers all five, in-app only, with the persistence shaped so an email
transport can be added later without a migration.

## Why not just finish the mention bridge

The obvious minimal fix is to add a `comment_mentions` listener mirroring
`comments_listener.rs`, route the payload into the doc room, and emit
`MSG_MENTION` as the June plan intended. That delivers a toast to users who
already have that document open — which is the one case where they would have
seen the comment anyway.

An inbox has to reach you when you have no document open at all. The doc
WebSocket cannot do that: rooms are keyed by `doc_id`, and a user sitting on
`/tasks` or the landing page is in no room. So the transport is per-user, not
per-doc, and the reserved `MSG_MENTION` plumbing is deleted rather than
completed.

## Design

### 1. Schema

One additive migration (`migrations/20261101120000_notifications.sql`). No
backfill: mentions, replies, assignments and shares that happened before this
ships stay unnotified. Assignments need no seeding to stay that way — §2 tells
a new assignment from an old one by the task rows an edit replaces, and an
upgraded deployment's `doc_tasks` already holds its old ones.

```sql
CREATE TABLE notifications (
  id            bigserial PRIMARY KEY,
  workspace_id  uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
  user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  actor_id      uuid NULL REFERENCES users(id) ON DELETE SET NULL,
  kind          text NOT NULL,
  doc_id        uuid NULL REFERENCES documents(id) ON DELETE CASCADE,
  target_kind   text NOT NULL,
  target_id     text NOT NULL,
  dedupe_key    text NOT NULL,
  data          jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now(),
  read_at       timestamptz NULL,
  emailed_at    timestamptz NULL
);

CREATE UNIQUE INDEX notifications_dedupe ON notifications(user_id, dedupe_key);
CREATE INDEX notifications_inbox  ON notifications(user_id, id DESC);
CREATE INDEX notifications_unread ON notifications(user_id) WHERE read_at IS NULL;
```

- `user_id` is the **recipient**, `actor_id` whoever caused it (NULL for
  system-generated events, i.e. `task_due`).
- `kind` ∈ `mention | reply | task_assigned | task_due | doc_shared`, enforced
  by a `CHECK` constraint added in the follow-up migration named above. The
  original decision here was to skip a `CHECK` on the theory that
  `PgNotificationStore` was the sole write path and application code was the
  only thing that needed to agree on the five literals — that stopped being
  true once `task_assigned` (`knot_storage::tasks`) and `task_due` (the sweep,
  §4) started inserting into this table directly. With three writers, the
  constraint is cheap insurance against a typo'd literal in any of them.
- `target_id` is **TEXT**, not UUID, because `doc_tasks.id` is
  `"<doc_id>:<item_index>"` (`migrations/20260604120000_doc_tasks.sql`).
- `data` carries the denormalised excerpt, doc title at emit time, and
  `thread_id` where applicable, so rendering the inbox needs no joins beyond
  actor and current doc title.
- `dedupe_key` + its unique index is the idempotency mechanism. Every emit is
  `ON CONFLICT (user_id, dedupe_key) DO NOTHING`, which makes concurrent emits
  from multiple replicas safe without coordination.
- `emailed_at` is the entire "email-ready" affordance: because emit writes a row
  rather than pushing, the table *is* an outbox. A future mailer polls
  `WHERE emailed_at IS NULL AND created_at < now() - interval '2 minutes'` (the
  delay collapses a burst into one message) and per-user preferences arrive as
  their own table. This table does not change.

### 2. Emit

`NotificationStore::emit(&NewNotification)`, following the codebase's store
convention: a trait behind `Arc<dyn …>` owning its own pool, wired in
`AppState::with_pool`. Emits where `actor_id = user_id` are dropped inside
`emit`, not at each call site.

A trait object cannot accept a transaction, so the request-path emits (mention,
reply, `doc_shared`) run **after** their write commits, exactly as today's
`broadcast_mentions` does. A crash in the window between the two loses the
notification. That is at-most-once, it is what ships today, and the alternative —
generic executors, which `dyn` forbids — would restructure every store in the
crate for a notification nobody is waiting on.

`task_assigned` is the exception and does not go through the trait: it is written
by a plain `INSERT … ON CONFLICT DO NOTHING` inside the transaction
`upsert_for_doc` already opens, so a task's index row and its notification commit
together or not at all.

Five sources:

| kind | Where | `dedupe_key` |
| --- | --- | --- |
| `mention` | `routes/api/comments.rs`, replacing `broadcast_mentions` | `mention:<comment_id>` |
| `reply` | same request path | `reply:<comment_id>` |
| `doc_shared` | a new grant in `routes/api/grants.rs` (not a role change) | `share:<doc_id>:<grantee>:<time of the share>` |
| `task_assigned` | `knot_storage::tasks` reindex upsert | `task_assigned:<doc_id>:<sha256(text)[..16]>:<assignee>` |
| `task_due` | periodic sweep (§4) | `task_due:<doc_id>:<sha256(text)[..16]>:<assignee>:<sweep date>` |

**Reply recipients** are the distinct authors of non-deleted comments in the
thread, minus the actor, minus anyone already receiving a `mention` row for the
same comment. Being mentioned in a reply to your own thread produces one
notification, not two.

**What counts as a new assignment.** `doc_tasks.id` is
`"<doc_id>:<item_index>"`, and `upsert_for_doc` deletes rows that fell out of
the new id set and re-inserts the rest on *every* reindex — which runs every two
seconds while a document is being edited. Neither the row id nor "row inserted
with an assignee" says whether a task is new: a reorder changes every id, and a
task is often assigned before its text is typed, so the text changes on every
flush while it is written.

So `upsert_for_doc` reads the doc's task rows before replacing them and compares,
assignee by assignee (`assignment_changes` in `crates/knot-storage/src/tasks.rs`):

1. A task with the same assignee and text as an old row is that task — moved,
   reopened, or unchanged. No notification.
2. Of the rest, a task whose text is *similar* to a still-unmatched old row of
   the same assignee is that task, edited: what the two texts share at their
   start and end covers at least half of the shorter one. No new notification;
   an unread one moves along with the edit, so it shows the text the task has
   now.
3. Any other open task is newly assigned, and notifies.

Typing, fixing a typo or adding words keeps a task; rewriting it into different
words makes it a new one, which is what it reads as. Handing a task to someone
else notifies them. The quiet refresh sweep (a re-render nobody edited) never
notifies.

A copied page's tasks notify nobody: Duplicate writes them as a copy
(`TaskWrite::Copy`), and from-template and workspace import write the parsed
copy's task rows as a copy before its content reaches the room, so the reindex
that follows finds every assignment already recorded.

Of several unmatched rows, a changed task is paired with the one it is most like
(the share of the longer text the two have in common), the closest pairs first,
so that a task added next to an edited one doesn't take that one's row.

The `dedupe_key` stays content-addressed: two replicas indexing the same edit
compute the same key and write one row. It also means a fresh assignment whose
key already has a row stays silent, which is accepted:

- **Handing a task back.** Bob is notified about "ship it", it goes to Carol,
  then back to Bob: Bob's earlier row holds the key, so he isn't told again
  until retention has removed it.
- **Delete and undo.** Only the rows the write replaces are compared, so a
  task that leaves one reindex and comes back in a later one (deleted, then
  undone more than two seconds later) counts as new. For a task that was
  notified, its row holds the key and nothing is written. For one that never
  was (assigned before the upgrade, copied, self-assigned on an import, or
  pruned), it notifies once. Remembering removed assignments for a while would
  close this; it isn't worth a table yet.
- **An edit right after a duplicate.** Duplicate answers once the copy is
  stored and writes the copy's task rows in the background afterwards, as main
  designed it, so a long copy can't outlast the request. An edit of a copied
  page before its rows are written indexes it as an edit, and notifies its
  assignees.

`task_due` (§4) is computed from the same unstably-keyed rows and uses the same
content-addressed key, for the same reorder reason.

**Identical task text does not notify twice.** Two checklist items reading
"Follow up", assigned to the same person in the same document, hash to the same
key, so only the first notifies. Adding `item_index` to disambiguate them would
reintroduce the re-notify-on-every-reorder bug the key exists to prevent. The
assignee still sees both on `/tasks`.

**Self-assignment is not suppressed on the live-editing path.** `emit`'s
`actor_id == user_id` guard needs an actor, and the path a real edit takes has
none: `Room::on_inbound` persists WebSocket updates with `by_user_id: None`
(`crates/knot-crdt/src/room.rs:586`, deliberately fire-and-forget), and the
dirty channel feeding the reindex worker is an `mpsc::Sender<Uuid>` carrying a
document id and nothing else. So `refresh_markdown_and_index` passes
`actor_id: None`, and typing a checklist item that assigns you a task notifies
you about it. The guard still works on the import paths, where the request
carries `ctx.user_id`.

Threading identity through the collaborative hot path to close this is not worth
it, and would not clearly be correct: under co-editing, several people may have
edited between two reindexes, and whoever typed the mention need not be the last
writer, so "who assigned this" has no single answer. A `task_assigned` row with
a NULL actor renders as "task assigned to you" with no actor name, which is
accurate. `crates/knot-storage/tests/notifications.rs` pins this behaviour so it
stays deliberate.

### 3. Comment mention identity

Document mentions already carry exact identity: `MentionExtension.ts:33` inserts
a link to `knot://user/<uuid>`, and `knot_markdown::tasks` lifts the assignee
back out of that sentinel.

Comment mentions do not. `MentionPicker.tsx:52-56` inserts plain
`@${display_name}` text, and the server re-guesses the recipient with
`Regex::new(r"(?:^|\s)@(\w+)")` matched against lowercased display names
(`comments.rs:31,133`). `\w` is Unicode-aware in the `regex` crate under the
default features this workspace takes (`Cargo.toml:97`), so `@Jörg` resolves
fine. The real defect is the space: `@Christian Hüning` captures `Christian`,
matches no member's full display name, and silently notifies nobody. Any member
whose display name has a space in it — most of them — is unmentionable. Today
that fails invisibly; with an inbox it becomes "why does my colleague never get
pinged".

The editor's `knot://user/<uuid>` sentinel is **not** the fix here. Comment
bodies render as plain text — `CommentThread.tsx:132` drops `comment.body` into
a `whitespace-pre-wrap` paragraph — so a markdown link would show every reader
its raw source. Rendering comment bodies as markdown would work, but it pulls
`marked` plus HTML sanitisation into the comment path, and an XSS surface is a
steep price for a mention chip.

Instead the client sends what it already knows. `MentionPicker` records the
`user_id` of each member picked; the two comment-creation routes (`POST
/api/docs/{id}/comments` and its `/replies` sibling) gain an optional
`mentions: [uuid]` field. The server validates each id is a member of the
comment's workspace and takes the **union** of that list with whatever the
display-name regex still matches in the body — not either/or. Picking one
person from the picker and then hand-typing a second `@name` (closing the
picker, e.g. by typing a trailing space, drops back to free text) must
notify both; an id-only-when-present design silently dropped the typed one
whenever the picker had contributed at least one id. The body stays plain
`@Christian Hüning` text and renders exactly as it does today.

For a comment with no `mentions` field at all — every comment written before
this ships, and any scripted client — the union degenerates to the
display-name regex alone, so nothing regresses for that case. `dedupe_key` is
per comment, so mentioning the same person across multiple edits notifies
them once and re-notifies nobody.

**Editing does not carry a `mentions` field.** `PATCH /api/comments/{id}`
only takes `body`; the server always treats an edit's explicit-id set as
empty, so a mention added while editing is only ever caught by the
display-name regex — a member whose display name contains a space still
cannot be mentioned by editing an existing comment into naming them. Wiring
the picker into the edit form is straightforward but is not part of this
pass; the create path was the one silently notifying nobody, which was the
more common case and the one this design set out to fix.

### 4. Sweep

One `tokio::interval` task, 15 minutes, spawned per replica alongside the
existing listener tasks in `main.rs`. Two jobs:

1. **Overdue tasks** — `doc_tasks WHERE due_at < now() AND due_at > now() -
   interval '7 days' AND NOT checked AND assignee_user_id IS NOT NULL`,
   emitting `task_due`. The 7-day floor exists for an upgrade reason: on an
   existing deployment, `doc_tasks` already
   has tasks that went overdue at any point in the workspace's history, while
   `notifications` starts out empty. Without the floor, the first sweep after
   this ships would treat the *entire* historical backlog of overdue work as
   newly overdue and fire a `task_due` burst for all of it; a task that has
   been sitting overdue for months is still visible on `/tasks` without
   needing a push. The dedupe key is content-addressed exactly like
   `task_assigned`'s — `task_due:<doc_id>:<sha256(text)[..16]>:<assignee>:<sweep
   date>` — rather than keyed on `t.id`, for the identical reorder reason: an
   id-keyed key would re-fire whenever a list edit shifted a later item's
   `item_index`. The date suffix still makes an overdue task notify once per
   day, not once per sweep.
2. **Retention** — delete read rows older than 90 days, and any row older than
   180 days, of every kind. Deleting a `task_assigned` row can't bring the
   assignment back: §2 decides what is new from the task rows, not from the
   notification.

Every replica runs both. The unique index plus `ON CONFLICT DO NOTHING` makes
concurrent sweeps idempotent, so no leader election, no advisory locks, and no
new operational component. The cost is a redundant indexed scan per replica per
15 minutes.

### 5. HTTP API

All routes are session-authenticated and scoped to `ctx.user_id`. Ownership needs
no join — a row belongs to exactly one recipient — but document access is a
separate question, re-checked on read below.

- `GET /api/notifications?filter=unread|all&limit=50&cursor=<id>` — keyset paging
  on `id DESC`, joined to actor display name and current doc title.
- `GET /api/notifications/unread_count` — `{"count": n, "capped": bool}`. The
  count runs `SELECT count(*) FROM (SELECT 1 … LIMIT 100) t` so an inbox nobody
  has read for a year cannot turn the polled endpoint into a sequential scan;
  the UI renders `99+`.
- `POST /api/notifications/read` — body `{"ids": […]}` or `{"all": true}`.

**Stale access.** A notification can outlive your access to the document it
points at: the page restricted, archived (for a viewer) or unshared since. The
list and the count both take the caller's cached visibility snapshot
(`AclCache::visibility`) and leave out rows about its hidden docs in SQL, as
`/api/workspace/tasks` does, so the badge never counts what the list can't show
and hidden rows can't fill a page. The list checks its rows again against a
snapshot covering their docs, for docs newer than the cached one. Both are
scoped to the caller's workspace. Revoking a grant does not have to rewrite
anyone's inbox, and a revoked user's stale rows are unreachable rather than
403-producing.

### 6. Transport: polling

`useUnreadCount()` is a TanStack Query with `refetchInterval: 30_000` and
`refetchOnWindowFocus: true`. Returning to the tab refreshes immediately, which
covers most of the perceived latency; the worst case for a user staring at an
open tab is 30 seconds.

This deliberately adds no new subsystem. The alternative considered was a
per-user SSE stream (`GET /api/notifications/stream`) with one `PgListener` per
replica fanning out in-process — the exact shape of `comments_listener.rs`. It
is the better end state, but it costs a long-lived connection per tab, plus
heartbeats, a connection cap, and multi-tab handling, and it reads the same table
through the same endpoints. It can replace polling behind an unchanged API and
schema if 30 seconds proves too slow in real use.

### 7. UI

- An **Inbox** link with an unread badge in
  `web/src/features/workspace/WorkspaceHeader.tsx:40`, beside the existing Tasks
  link. Badge hidden at zero.
- A **dropdown** showing the last 10: actor, kind, doc title, excerpt, relative
  time. Clicking marks that row read and navigates to its target.
- A **`/notifications` route**, lazy-loaded like every other route in
  `routes.tsx:49-70`, with an all/unread filter and "Mark all read".

### 8. Thread deep-linking

Nothing in `DocPage` reads a thread from the URL today, so clicking a mention
would land you at the top of the document with no indication of where the
comment is. `/doc/:id?thread=<uuid>` opens the comments panel and scrolls that
thread into view. Small, but load-bearing: a notification you cannot act on is
not worth sending.

### 9. Removals

- `broadcast_mentions` and the `comment_mentions` channel
  (`comments.rs:141-188`) — replaced by `emit`.
- `MSG_MENTION`, the `mention` listener list, and `MentionMsg` in
  `KnotProvider.ts` — dead since June, and this design routes around the doc
  WebSocket entirely.

## Observability

One counter, `knot_notifications_emitted_total{kind}`, registered in `knot-obs`
beside the existing metrics. The sweep logs its emit and prune counts at debug.

## Testing

**`crates/knot-storage/tests/notifications.rs`**

- Concurrent `emit` with the same `dedupe_key` produces exactly one row.
- Self-notification (`actor_id = user_id`) writes nothing.
- **Reorder characterisation:** reindex a doc whose two assigned tasks have
  swapped positions; assert no new rows. This is §2's hazard as an executable
  assertion, and it is the test most likely to catch a future change to the task
  id scheme.
- Editing a task's text, and typing it flush by flush, does not emit again; the
  unread notification shows the final text. Rewriting it into different words
  does emit. Reopening a completed task does not; handing it to someone else
  notifies them. A quiet sync notifies nobody.
- A task that lands **checked** (never notified while open — e.g. imported
  already done) does not emit `task_assigned`.
- Retention prunes read rows past 90 days and everything past 180; pruning a
  read `task_assigned` row doesn't make the next edit re-notify.
- Deleting a document cascades its notifications away.

**`crates/knot-server/tests/notifications_sweep.rs`**

- Sweep: an overdue task emits once, a second sweep in the same day no-ops, the
  next day emits again.
- A checked task is never overdue.
- Only a task that went overdue in the last 7 days notifies.
- **Reorder characterisation for `task_due`:** an overdue task notifies once;
  inserting an unrelated item above it (shifting its `doc_tasks.id`) and
  sweeping again the same day must not produce a second `task_due` — the same
  hazard as the `task_assigned` reorder test, for the sweep's own key.

**`crates/knot-server/tests/notifications_integration.rs`**

- Mention writes a row for the mentioned user and none for the author.
- Reply writes rows for thread participants minus the actor; a user both
  mentioned and a participant gets exactly one row.
- A comment carrying `mentions: [<uuid>]` notifies that user even though their
  display name contains a space; a comment sent without the field still resolves
  `@Alice` through the regex fallback.
- A comment carrying one picked id **and** a hand-typed second `@name` in the
  body notifies both — the union, not either/or, per §3.
- An id in `mentions` for a non-member is rejected, not notified.
- Granting access emits `doc_shared`.
- The list endpoint omits rows whose doc access was revoked.
- `POST /read` with ids, and with `{all: true}` — the latter exercised
  end-to-end (two distinct unread rows, marked via `{"all": true}`, both
  confirmed read) rather than only asserted at the store layer, since the
  route's `ids` defaulting to `[]` and `all` to `false` means a wrong key name
  degrades to a silent no-op that still returns 204.

`unread_count`'s `capped: true` branch is covered as a small pure-function
unit test in `crates/knot-server/src/routes/api/notifications.rs` (`is_capped`)
rather than through the HTTP route, which would require provisioning 100 real
unread rows for a user just to cross the hardcoded cap.

**Vitest** — badge hidden at zero and capped at `99+`; dropdown rendering;
optimistic mark-read; an actor-less row (`task_due`, and `task_assigned` on
the live-editing path) renders as a plain statement rather than attributing
the product to itself as "knot"; a picked mention id whose name was
backspaced back out of the comment body before submit is dropped, not
notified.

**`e2e/flows/notifications.spec.ts`** — two browser contexts. Alice comments
`@Bob` on a doc Bob can read; Bob's badge appears, the dropdown lists it,
clicking navigates to the doc with the thread open and scrolled, and the badge
clears. The spec drives the refresh with a window focus event rather than
waiting out the poll interval. A second case mentions a member whose display
name contains a space, which fails on `main` and passes here.

## Sequencing

1. Migration, `emit`, dedupe, storage tests.
2. Mention + reply + `doc_shared` call sites; integration tests.
3. `task_assigned` (content-addressed key) and the sweep.
4. HTTP endpoints.
5. Comment mention identity (§3).
6. Thread deep-linking (§8).
7. UI: badge, dropdown, `/notifications`.
8. e2e.
9. Delete the dead plumbing (§9).

Steps 1–4 are shippable without any UI; steps 5 and 6 are independently useful.

## Non-goals

- **Email.** No SMTP, no templates, no preferences. `emailed_at` reserves the
  seam; nothing writes it.
- **SSE / live push.** §6 — deliberately deferred, and cheap to add later.
- **Watching a page.** "Notify me of any edit to this doc" is a different
  subscription model and a much noisier one.
- **Per-user notification preferences.** Everything in §2 notifies. Preferences
  are worth building once email exists and the noise is real.
- **Push notifications / service worker.** The web manifest added in 0.4.0 makes
  this possible later; it is not this.
- **Cross-workspace inbox.** knot is single-workspace today; `workspace_id` is
  on the table so this does not become a migration later.
