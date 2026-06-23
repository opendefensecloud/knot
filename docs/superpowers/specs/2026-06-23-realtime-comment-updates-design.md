# Realtime comment/reply updates — design (item C)

**Date:** 2026-06-23
**Status:** Approved (brainstorm)

## Problem

Comments are a REST-only resource with no realtime transport. When another user
adds a comment or reply, the current user's sidebar does not update — only the
poster's own client invalidates its `["comments", docId]` query
(`CommentSidebar.tsx`, `CommentThread.tsx`). Other viewers see nothing until they
manually refetch (reload, toggle the sidebar). There is a half-built mention
pipeline: the backend `pg_notify('comment_mentions', …)` has **no listener**, and
the frontend collab provider has a `MSG_MENTION` handler that never fires.

## Goal

When any comment changes on a document, every user currently viewing that document
sees the change within ~1s, by reusing the existing collab WebSocket + Postgres
LISTEN/NOTIFY — matching the existing `acl_invalidate` listener pattern.

## Transport (approved)

```
comment mutation (create/reply/edit/resolve/unresolve/react/delete)
  → pg_notify('doc_comments', { doc_id })            [comments.rs, fire-and-forget]
  → background listener LISTEN doc_comments           [new comments_listener.rs, mirrors acl listener]
  → look up the doc's room IF it already exists        [Rooms::notify_doc_comments — non-booting]
  → push a MSG_COMMENTS frame to all room connections  [Event::ServerFrame]
  → KnotProvider emits a "comments" event             [frontend]
  → KnotEditor invalidates ["comments", docId]         [sidebar query refetches]
```

## Key design decisions

1. **Subscribe where the provider already lives.** `CommentSidebar` is a sibling of
   `KnotEditor` and has no provider access. Rather than thread the provider down,
   **`KnotEditor`** (which owns the provider) subscribes to the `"comments"` event
   and calls `queryClient.invalidateQueries(["comments", docId])`. The sidebar's
   existing query refetches via React Query — no provider sharing, context, or new
   props. (`invalidateQueries` with key prefix `["comments", docId]` matches the
   sidebar's `["comments", docId, showResolved]`.)
2. **Never boot an idle room.** The listener uses a **non-booting** lookup
   (`Rooms::notify_doc_comments`, mirroring `revoke_all_for_doc`): if no room exists
   for the doc (nobody is viewing it), the notification is simply dropped. A comment
   on an unviewed doc must not spawn a room.
3. **Clean crate separation.** The server builds the wire frame
   (`[MSG_COMMENTS=5][varuint len][json]` via `protocol::append_var_uint`) and hands
   the bytes to `knot-crdt`'s `notify_doc_comments(doc_id, frame)`. `knot-crdt` knows
   nothing about comment semantics; it only broadcasts the frame to the room's
   connections via a new `Event::ServerFrame(Vec<u8>)`.
4. **All comment mutations** emit the notify (create thread, reply, edit, resolve,
   unresolve, add/remove reaction, delete) — one helper call each, for consistency.

## Components / files

- `crates/knot-server/src/protocol.rs` — `pub const MSG_COMMENTS: u8 = 5;`
  (0,1 used by sync/awareness; 4 is reserved by the frontend for mentions).
- `crates/knot-crdt/src/room.rs` — `Event::ServerFrame(Vec<u8>)` + a handler that
  broadcasts the frame to all `conns` (mirrors the `AwarenessIn` fan-out).
- `crates/knot-crdt/src/registry.rs` — `pub async fn notify_doc_comments(&self,
  doc_id, frame: Vec<u8>)`, non-booting (mirrors `revoke_all_for_doc`).
- `crates/knot-server/src/routes/api/comments.rs` — `notify_comment_change(state,
  doc_id)` helper (mirrors `broadcast_mentions`' pool access:
  `state.pool.as_ref()` → clone → spawn `pg_notify('doc_comments', …)`); called in
  all eight mutation handlers after success.
- `crates/knot-server/src/comments_listener.rs` (new) — `LISTEN doc_comments` loop
  (mirrors `knot_docs::spawn_listener`'s reconnect structure); on each notification
  parse `doc_id`, build the frame, call `rooms.notify_doc_comments`.
- `crates/knot-server/src/main.rs` — spawn the comments listener next to the acl
  listener: `if let (Some(pool), Some(rooms)) = (state.pool.clone(),
  state.rooms_v2.clone()) { … }`. Register `mod comments_listener;`.
- `web/src/features/editor/KnotProvider.ts` — `MSG_COMMENTS` handling, a
  `CommentChangeMsg` type, and a `"comments"` event (same shape as `mention`).
- `web/src/features/editor/KnotEditor.tsx` — subscribe to `"comments"` and
  invalidate the comments query (add `useQueryClient`).

## Testing

- **Frontend unit** (`KnotProvider.test.ts`): build a `MSG_COMMENTS` frame
  (`[5][varuint len][json]`), register a `"comments"` listener, call the private
  `handleFrame`, assert the listener fires with the parsed `{ doc_id }`. Mirrors the
  existing `handleFrame` test.
- **E2E** (`e2e/flows/comments-realtime.spec.ts`): two browser contexts (owner +
  an invited editor) open the same doc. One posts a reply; assert the **other**
  context's sidebar shows the reply within a few seconds **without reload**. This is
  the acceptance proof. If two-context realtime proves too flaky in CI after honest
  effort, downgrade to a documented manual smoke and keep the unit test as the gate.
- **Manual smoke:** two browsers, same doc; a reply in one appears in the other.

## Risks / notes

- **Idle-room avoidance** (decision 2) is the key safety property — verified by the
  non-booting lookup; no room is created from the listener path.
- **Missed notifications during listener reconnect** (5s backoff window): acceptable
  — the sidebar already refetches on open, so a transient miss self-heals on the
  next interaction. (No outbox; YAGNI.)
- **Markdown-only view** (`mdView` in `DocPage`) does not mount `KnotEditor`, so it
  won't live-update. Acceptable; the editor view is the default and common case.
- The poster's own client receives the broadcast too and harmlessly re-invalidates
  (it already refetched optimistically). No special-casing needed.
- `MSG_MENTION` (4) remains reserved/unused; this design does not implement it.
