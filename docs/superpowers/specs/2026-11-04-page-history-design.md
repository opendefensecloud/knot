# Page history: a lifetime timeline of every change (design)

**Date:** 2026-11-04
**Status:** Approved 2026-11-04
**Idea from:** Lutz Behnke (cypherfox)
**Mockup:** option A ("side drawer") from the brainstorm companion. The layout is described in "The drawer" below.

## Why

Knot pages are written by several people over months. Today nobody can say
who wrote a paragraph, how a page got to where it is, or get back to what it
said in the spring. The History drawer shows a short list of bare timestamps
(the last 5 snapshots plus one per day for 30 days, with no names). Its preview
is raw Markdown, and its restore goes through a lossy Markdown round trip.

Lutz wants three things from history:

1. **Who** changed **what**.
2. **How** the page evolved over time.
3. **Going back** to any earlier state.

"Why" a change was made (a note per version) is out of scope **(user)**.

## What it is

Knot keeps every change to a page for the page's whole life. The History drawer
becomes a timeline: day headings, and under each day one entry per person's
editing session. Clicking an entry shows the page as it was after that
session, rendered like the real page, with that session's changes highlighted.
From there an editor can restore that state in place or as a new page. An
owner can permanently erase a piece of text (a pasted password, for example)
from every stored version.

## Decisions

Marked **(user)** where the user decided, **(design call)** where it was
settled during the design and the user has not weighed in.

| Question | Decision |
| --- | --- |
| How much is kept? | **(user)** Every change, for the page's whole lifetime. Nothing expires. |
| What is one timeline entry? | **(user)** One person's **editing session**: a continuous stretch of their edits on this page. Entries sit under **day headings**, newest first. If two people edit at once, each gets their own entry. |
| When does a session end? | **(design call)** After **20 minutes** with no edit from that person on that page. There is no midnight cut, so boundaries are the same for every viewer and can be stored. An entry is listed under the day it **ended**, in the viewer's local time zone. |
| What does an entry show? | **(design call)** The author's avatar/colour and name, the time span (`09:12–09:40`), and words that author added and removed (`+120 / −15`). Special entries (restore, erase, created as copy, suggestion accepted, import) have their own wording; see "Timeline entries". |
| Overlapping sessions (A edits 09:00–09:40, B edits 09:10–09:20) | **(design call)** Entries are ordered by end time. "The page after entry A" is the **whole page at A's end** (09:40), B's edits included. "Show changes" on A highlights **only A's own** insertions and deletions in A's window, never B's. "Restore here" on A restores that whole-page state, B's edits included. This needs per-update attribution of what each update actually changed (see Constraints), kept per entry. |
| Where does history live in the UI? | **(user, mockup option A)** The existing **side drawer** on the right, made wider, so the live page stays in view behind it. Not a full-page mode. |
| What does selecting an entry show? | **(design call)** The page as it was **right after** that entry, rendered read-only with the real editor styles (not raw Markdown). A **"Show changes"** toggle (on by default) highlights what that entry changed against the state right before it: insertions underlined green, deletions struck through red. |
| Restore in place | **(user)** Available. **(design call)** It is a CRDT-level revert: it changes only what differs between now and the target state, so comment anchors, suggestion anchors and the authorship of unchanged text survive. It no longer goes through Markdown. It shows in the timeline as "Restored by X, to `<date time of target>`", and can itself be undone by restoring the entry before it. |
| Restore as a new page | **(user)** Available as a second button next to "Restore here". The current page is not touched. **(design call)** The new page is created next to the original (same parent, placed directly after it), titled "`<title>` (version of `<date>`)", with the target state as its content. Like Duplicate, it starts its own fresh history whose first entry is "Created from `<original title>`, version of `<date>`, by X". It needs the same permission Duplicate needs. |
| Who can see history? | **(user)** Editors and Owners only, as today. **Viewers see only the current page.** |
| Who can restore? | **(design call)** Editors and Owners, as today. |
| Permanently erasing content | **(user)** Owners only (effective doc role Owner, `DocAccess.role == Owner`, the same rule comment delete and suggestion decisions use). The owner **selects text in any past version** and chooses "Erase from history". |
| What does an erase match? | **(user)** **Every occurrence of the selected string** anywhere in the page's history, however it got there (typed, pasted again, retyped, brought back by a restore), not just the selected characters. **(design call)** The dialog shows the selected text and how many places in how many versions match **before** the owner confirms. Matching is exact and case-sensitive on the plain text, and also finds matches that span formatting or item boundaries inside one text block. The dialog warns when the selection is shorter than 6 characters. |
| What remains after an erase? | **(design call)** The timeline gets an entry "`<Owner>` removed content from history" with the date. It does not say what was removed. In past versions the erased text is gone, as if it had never been typed, and word counts shrink to match. The erase is **not undoable**. |
| Erasing text that is still in the current page | **(design call)** The erase always removes it from the current page too. There is no "keep it on the page" option, because the same text cannot be gone from history and still be live. |
| What else on the page an erase cleans | **(design call)** Everything knot stores **for this page**: update log, snapshots, any history or checkpoint table, `markdown_cache`, the search index, and copies of page text in this page's comments (`anchor_text`), change suggestions (`original_text`, `proposed_text`, `context_before`), tasks (`doc_tasks.text`), notification excerpts and `audit_events.data`. Matches there are replaced by `[removed]`. |
| What an erase does not reach | **(design call)** Other pages (copies made by Duplicate or "Restore as new page", or text pasted elsewhere), images and attachments, and database backups, WAL archives and replicas taken before the erase. The dialog says this in one sentence. Removal is **logical**: no knot API, UI or table query returns the text afterwards. Knot additionally asks Postgres to vacuum the rewritten rows, best effort. |
| Open and offline editors during an erase | **(design call)** The erased text must not come back. Every open editor of the page is forced to reload from the server. Edits that the server had not received yet are lost, and the dialog warns about this when the page is open elsewhere. The server keeps a durable record of the erased content and rejects incoming updates that would bring it back (for example an offline tab reconnecting). Typing the same text again later, as a new edit, is allowed. |
| Data protection: removing a person from history | **(user)** Exact times stay. An **admin function** replaces a person's name in **all** history with "Former member". **(design call)** Instance admins only (`crates/knot-server/src/admin.rs`), for one user at a time, across all workspaces. It is irreversible and covers timeline entries, contributor credit (`doc_contributors`, byline) and the restore, erase and accept wording. It does not change comments or suggestions they wrote, and it does not remove their content. History must therefore reference authors in a way that can be rewritten in one place. |
| Access to the past follows current access | **(design call, from the brainstorm)** History is gated by the page's **current** access. Someone who gets access later, or a page moved out of a restricted area, exposes the page's whole past. Erase is the remedy for anything that must not be seen. No access history is stored. |
| Backups | **(design call)** Covered by "What an erase does not reach". |
| Existing pages (before rollout) | **(design call)** Their early history is already gone, because the current GC deletes it. Whatever survives at rollout (the remaining snapshots) appears at the bottom of the timeline as "Earlier versions (author unknown)", each viewable and restorable, under a line "Complete history starts `<rollout date>`". New pages have complete history from creation. |
| Title changes | **(user)** Not tracked in v1. The timeline covers page content only. Past versions show the **current** title, restore leaves the title alone, and erase does not touch titles. |
| Moves, icon, restriction changes | **(design call)** Not in the timeline in v1. |

## Goals

- An editor opens History on any page and sees, per day, who edited and when,
  back to the page's creation (or to the rollout date for older pages).
- Any entry can be viewed as the page looked right after it, with that
  entry's changes highlighted.
- Any entry can be restored in place without breaking comments or
  suggestions, or restored as a new page beside the original.
- An owner can erase a secret from all of history in a few clicks.
- An instance admin can turn a person into "Former member" across all
  history.
- Live editing is not slower than today.

## Non-goals (v1)

- A "why" or note per version, and named or pinned versions.
- Viewers browsing history.
- Character-level "blame" (hover any word to see who wrote it). The data
  model must not rule it out; see "Constraints".
- Timeline entries for title changes, moves, icons, restriction or grant
  changes, archive and unarchive.
- History of boards, of comments (edits, resolve, delete) and of attachments.
  An old version shows embedded boards with their **current** content.
- Comparing two arbitrary versions (only "this entry against the one
  before").
- History in exports (Markdown, DOCX/ODT, workspace zip) and imports. A
  workspace export or import carries only current content, as today.
- A finer-grained scrubber below session level.

## Anti-goals (it fails even if it technically works when …)

- … a restore or an erase corrupts a page or desyncs connected editors.
  Restore has done this before (CHANGELOG, "Restoring a snapshot merged
  instead of replacing" and the #52 corruption).
- … live typing gets slower, or a room's memory grows with the page's history.
- … erased text can still be found anywhere knot stores page content:
  update log, snapshots, checkpoints, rendered caches, search index, or any
  new history table.
- … a Viewer can reach any past state through any route (API, share links,
  export).
- … the timeline credits an edit to the wrong person in normal use: two
  people editing at once, a reconnect after being offline, or a server-side
  change.

## The drawer

The same right-hand drawer as today, wider (most of the viewport on desktop),
with the dimmed live page visible on its left:

- **Left column: the timeline.** Day headings ("Today", "Yesterday",
  "Mon 2 Nov", …), newest first. Under each, the entries described above.
  The list loads more as you scroll; a page can have thousands of entries.
- **Right column: the selected version.** A bar with the "Show changes"
  toggle, **Restore here**, **Restore as new page**, and, for owners while
  text is selected, **Erase from history**. Below it, the page as it was after
  the selected entry, read-only.
- On a narrow screen the two columns stack: the timeline first, then the version
  view after choosing an entry, with a back control.
- Archived pages: as today, the drawer is not offered and restore is
  refused.

### Timeline entries

| Entry | Wording (English source; translate as the rest of the UI) |
| --- | --- |
| Editing session | `<Name> · 09:12–09:40 · +120 / −15` |
| Restore in place | `Restored by <Name>, to <target date time>` |
| Created as a copy (Duplicate, Restore as new page) | `Created from <source title> by <Name>` |
| Accepted change suggestion | Counted in the **suggester's** session, as today's contributor credit does. **(design call)** The entry also notes `accepted by <Owner>`. |
| Markdown import / replace, template | `<Name> replaced the content (import)` / `Created from template <name>` |
| Task checkbox toggled | Part of the toggler's session like any edit. |
| Erase | `<Owner> removed content from history` |
| Anonymized author | `Former member` wherever the name would appear |
| Pre-rollout | Section `Earlier versions (author unknown)` with one entry per surviving snapshot, then the line `Complete history starts <date>` |

## Constraints (from reading the code)

These are facts about knot today that the design has to live with. File
references are as of `7e1de9d`.

- **Attribution already exists per update.** Every `doc_updates` row has
  `by_user_id`, stamped by the WebSocket shim for writers and by every
  server-side mutation (restore, task toggle, suggestion accept, duplicate)
  (`crates/knot-server/src/room.rs:163-173`,
  `crates/knot-storage/src/updates_store.rs:82-106`). Rows written before
  migration `20261003120000` have `by_user_id NULL`.
- **GC is the blocker.** `crates/knot-crdt/src/gc.rs` runs hourly on every
  replica. It deletes `doc_updates` up to `latest_snapshot_seq − 2·N` and
  thins snapshots to last 5 + 1/day for 30 days
  (`crates/knot-storage/src/snapshot_store.rs:168-196`). `seq` is a
  **global** `bigserial`, so the "2·N" margin is counted across all pages and
  on a busy instance a page's rows are gone within the hour. This deletion
  must stop.
- **Rolling deploys.** Old replicas keep running the deleting GC until they
  are gone. "Complete history starts" must be stamped only once no old replica
  can delete any more (for example a DB-level flag the new GC checks, set
  after rollout, in the style of `documents.contributors_since`).
- **The update log is not guaranteed complete.** A failed writer flush drops
  the batch (`crates/knot-crdt/src/writer.rs:112-118`) after the room has
  already applied and broadcast it, and the next snapshot then contains
  content with no log row. Lifetime history must close this hole (evict the
  room, retry, or record a checkpoint marked "unattributed").
- **`seq` order is not commit order** across replicas, and a room can skip a
  late lower `seq` (`crates/knot-crdt/src/room.rs:762-764`). Entry boundaries
  and "state after entry X" must be stable when late rows arrive (for example
  boundaries defined by state vector or by commit time with a settle delay,
  not by raw `seq`).
- **`by_user_id` means "who delivered the bytes".** A reconnect `SyncStep2`
  can carry other people's items and the client's whole delete set. Offline
  edits are stamped at reconnect time. Attribution of insertions and
  deletions must be computed from what the update **changed** on the server
  doc (items newly integrated, delete set after minus before), not from the
  raw bytes.
- **Server-side edits share the room's random client id**, and client ids are
  random per editor mount, so client id → user is not a stable mapping today.
- **yrs GC is on** (`Doc::new()`, `crates/knot-crdt/src/engine.rs:108-110`, and
  `new Y.Doc()` on the client). Live rooms must stay GC'd. Any past state is
  rebuilt by replaying stored updates from a checkpoint, never by turning off
  GC on the live doc.
- **Rendering past states on the client.** `@tiptap/y-tiptap`'s snapshot
  rendering throws without a `ychange` mark in the schema, and its error path
  deletes Y items. It may only ever run against a throwaway, read-only doc,
  never the live bound one.
- **Other roots in the Y.Doc.** Restore today clears only the `default`
  fragment, and `knot_applied_suggestions` (`crates/knot-crdt/src/suggestions.rs:101`)
  survives. Restore and erase must handle every root deliberately.
- **Comments live outside the Y.Doc**, anchored by RelativePositions. A restore
  that keeps unchanged items keeps their anchors. Comments on text that the
  restore removes behave as comments on deleted text do today.
- **Storage growth is unmeasured.** Rows are roughly per keystroke
  transaction. Before choosing a compaction scheme, measure
  `avg(octet_length(update_bytes))` and rows per page per day on a real
  instance.
- **Hydration does not read old rows.** Rooms boot from the latest snapshot
  plus newer rows (`crates/knot-crdt/src/room.rs:234-260`), so keeping old
  rows does not slow live editing. Only storage and history reads pay.
- **Purge cascades.** Purging an archived page deletes its history, which is
  correct and stays so.
- **Erase rewrites history the CRDT has already shared.** Clients and other
  replicas hold the erased items. Erase must coordinate with the live room
  on every replica (evict and reload), and the server must remember what was
  erased so a reconnecting client's `SyncStep2` cannot bring it back.
- **Erase reach.** See the "What else on the page an erase cleans" decision.
  Several of those tables hold copies of page text today; check the
  migrations for any others before planning.
- New tables go into the e2e reset list (`e2e/support/reset.ts`) and the
  migrations test.

## Left to the builder

- **Storage model.** Whether to keep every raw update forever plus sparse
  checkpoints, or to compact old raw updates into durable per-entry states,
  as long as (a) every timeline entry stays viewable and restorable forever,
  (b) the author(s) of each entry are kept, and (c) the choice does not rule
  out later character-level blame. Detail below session level may be
  compacted away.
- Whether entries are materialized at write time or derived at read time.
- How the "Show changes" diff is computed and rendered (server-side tree
  diff or client-side rendering in a throwaway doc), within the anti-goals.
- How restore-in-place computes the minimal change (diff target vs. current
  in one room transaction) and how erase rewrites stored history, including
  how erase is applied safely while the page is open in live rooms on several
  replicas.
- API shape, pagination, caching, metrics (table size, rows per page) and
  Helm sizing notes.
- How the drawer looks in detail, within the layout above and the existing
  design tokens.
- How the pre-rollout snapshots are turned into the "Earlier versions"
  section.
