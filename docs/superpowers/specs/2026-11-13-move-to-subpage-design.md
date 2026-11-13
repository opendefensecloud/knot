# Move selection to a new subpage — design

**Date:** 2026-11-13
**Status:** Draft (brainstorm), awaiting approval
**Issue:** <https://github.com/opendefensecloud/knot/issues/92>

## Why

A page grows too large and someone wants to break it into smaller pages
while discussions on it are still open. That is the normal state of a page
being worked on. Today the only tools are cut/paste and Duplicate, and both
drop comments and suggestions:

- **Cut/paste.** Comments and suggestions are rows beside the page, anchored
  by Yjs RelativePositions. Pasted text is new Y items with new ids, so no
  anchor can follow it.
- **Duplicate.** It deliberately starts the copy without comments (spec D,
  `2026-10-24-tree-markdown-sortkeys-duplicate-design.md`, non-goals).

Moving is neither a copy nor a paste. The discussion moves together with the
text it is about, so the decision "a copy starts its own life" is untouched.

## What it is

An editor selects content and clicks **Move to subpage**. They confirm a
title, prefilled from the first selected block. The server then does three
things:

1. It creates a child page of the current page holding the selected blocks.
2. It replaces those blocks in the current page with a paragraph that links
   to the new page.
3. It moves the comments and suggestions that sit entirely inside the moved
   blocks to the new page: threads with their replies, reactions, authors
   and resolved state, and suggestions with their replies.

## Decisions

Marked **(user)** where the user decided, **(design call)** where it was
settled during the design.

| Question | Decision |
| --- | --- |
| Granularity | **(user)** The selection (A), not "split at headings" (B). B can come later on the same server operation. |
| What stays in the source page | **(user)** A paragraph with a link to the new subpage, where the moved content was. |
| Comments/suggestions crossing the selection boundary | **(user)** They stay on the source page. The part of their range that moved disappears with the moved content. |
| Partial blocks | **(design call)** The selection expands to whole **top-level blocks** (children of the `default` fragment): paragraphs, headings, whole lists, tables, code blocks. Moving part of a paragraph would mean splitting and merging Y elements on the server; that is risky and not worth it for v1. The editor shows the expanded range before confirming. |
| Where the new page goes | **(design call)** A child of the source, last among its siblings (`Placement::Last`). It inherits access like any child, so no grants are copied. |
| Who may do it | **(design call)** Editor or higher on the source. That is also "editable destination", since the source is the parent. The page quota is enforced as for `POST /api/docs`. |
| Boards and files shown in moved blocks | **(design call)** References stay as they are. `boards.doc_id` / `blobs.doc_id` keep pointing at the source page. |
| Suggestions in state `applying` inside the range | **(design call)** Refuse with 409 `move.suggestion_applying`; the client retries. Their applied marker lives in the source doc. |
| Notifications about moved comments/suggestions | **(design call)** Re-pointed to the new page (`notifications.doc_id`) so links in the inbox keep working. |
| Undo | **(design call)** Not in v1. Ctrl+Z cannot undo a server edit. Undo means moving the content back by hand. |

## How it works

### Why the anchors survive

The new page's Y state is the **full source state**, with every top-level
block outside the range deleted. That is the same encode as Duplicate
(`engine.encode_state_as_update`). Items keep their (client, clock) ids, so
the stored `position_y` / `position_y_end` bytes of the moved rows resolve in
the new page unchanged. The rows only need a new `doc_id`.

### API

```
POST /api/docs/{id}/move-to-subpage
{ "start": "<base64 RelativePosition>", "end": "<base64 RelativePosition>", "title": "…" }
→ 201 { "doc": <new doc>, "moved": { "comments": n, "suggestions": m } }
```

`start` and `end` use the same encoding as comment anchors
(`encodeAnchorRange`). Because they are anchors and not indices, edits made
between the click and the request don't shift the range.

### Server flow

All of it runs on a spawned task like `apply_claimed`, so a request timeout
can't stop it halfway. The room is held across the DB step, so the source
can't change between "what we copied" and "what we cut".

1. **Route:** authz (Editor+ on source; not a template; not archived), quota,
   then `rooms.acquire(id)`.
2. **Room event `MoveBlocksOut { start, end, reply, proceed }`** (phase 1, in
   the actor):
   - `catch_up`;
   - resolve both anchors to their top-level block indices `i..=j`
     (walk up from the anchored item to the child of `default`);
   - build the child state: full state, then remove `0..i` and `j+1..` in a
     transient doc;
   - send `Prepared { child_state, i, j }` through `reply`, then wait on
     `proceed` (with a deadline) while holding the doc.
3. **Route, one DB transaction** (duplicate-style, `knot-storage`):
   - insert the page under the source, with the child state as its first
     `doc_updates` row (`UpdateSource::Move`), plus audit, invalidations and
     placement;
   - pick the rows to move. Comment threads move when **both** anchors of the
     root resolve inside blocks `i..=j` of the snapshot; whole-doc threads
     (`position_y` NULL) stay. Suggestions follow the same rule;
   - `UPDATE comments SET doc_id = new WHERE thread_id = ANY(..)`,
     `UPDATE suggestions SET doc_id = new WHERE id = ANY(..)`, and the
     matching `notifications`;
   - commit, then `proceed.send(Commit { href, title })`.
4. **Room event, phase 2:** in one transaction, remove blocks `i..=j` and
   insert a paragraph `[title]` with a `link` mark
   `href = knot://doc/<new id>` at index `i`. Then `persist_server_edit`
   (see "As built") and `fan_out`.
5. **After:** `notify_comment_change` / `notify_suggestion_change` for both
   pages, and a markdown/tasks refresh of both pages (best effort, as
   Duplicate does).

**Failures.**
- Before commit, nothing has happened: a refused commit releases the doc
  unchanged.
- If commit succeeded but phase 2 can't persist, the room evicts itself
  (existing behaviour) and the route answers 500. No compensation in v1:
  the new page holds the blocks and their discussion, and the source still
  holds the blocks. Nothing is lost.

### As built (deviations from the flow above)

- The room awaits a `commit` callback (`knot_crdt::move_blocks::Commit`)
  instead of a `reply`/`proceed` channel pair. Same effect, one event.
- No new `UpdateSource`. The new page's first row is `copy` with
  `meta.moved_from`, and the cut is `edit` by the mover with
  `meta.moved_to`. A `move` source would need a check-constraint migration
  and history labels, for no behaviour.
- Rows are chosen by resolving their anchors against the **new page's**
  state: a range moves when both anchors land in its live content.
- No markdown/tasks refresh on the request path. The room's reindex covers
  the source, and the refresh sweep covers the new page (as for Duplicate).

### Client

- A **Move to subpage** button sits next to "Add comment" / "Suggest
  change" in the selection float (`KnotEditor.tsx`). It shows only when the
  user can edit.
- On click, a `window.prompt` asks for the title (as renaming in the tree
  does), prefilled with the first block's text and capped at 120 chars. v1
  does not preview the expanded whole-block range.
- On success, the editor's Yjs provider receives the cut as a normal update,
  the doc tree refreshes and the comment and suggestion lists reload. A
  toast says "Moved to <title>"; the link in the page leads there.

## Tests

- **Integration** (`crates/knot-server/tests/move_to_subpage_integration.rs`),
  a page with paragraphs P1, P2, P3:
  - a thread with a reply and a reaction on P2, a thread spanning P1 to P2,
    a whole-doc thread, and a pending suggestion on P2;
  - moving P2 creates a child under the source with content P2;
  - the P2 thread (reply, reaction, author, resolved state) and the
    suggestion are listed on the child, and their anchors resolve to the same
    text there;
  - the spanning thread and the whole-doc thread stay on the source;
  - the source reads P1, link to the child, P3.
- Anchors that don't resolve to live content of this page get 422
  `move.anchor_lost`, and nothing changes.
- Permission: a viewer gets 403.
- Not covered by a test yet: a suggestion in `applying` gets 409, and the
  page quota is enforced.
- A selection inside P2 moves all of P2; a selection over P1 to P2 moves both.
- **e2e** (`e2e/flows/move-to-subpage.spec.ts`): comment on a paragraph, move
  it, and see the comment highlighted on the new page.

## Non-goals

- Moving part of a block (see "Partial blocks").
- Split at headings (B), a follow-up on the same server operation.
- Moving content into an existing page, or between pages in general.
- Copying (rather than moving) comments on Duplicate or paste. Spec D stands.
- Undo.
