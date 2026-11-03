# Change suggestions — design

**Date:** 2026-11-02
**Status:** Draft (brainstorm), awaiting approval
**Mockup:** <https://claude.ai/artifact/RxYnVQ2xZNy2b5Yqw3ChMf> (two clickable screens: an editor suggests, an owner decides). The mockup's "Undo" button on resolved items is **not** in v1 (see Non-goals).

## Why

Several people work together in one large knot page. Today the only way to
propose a change is to make it (and hope nobody minds) or to describe it in a
comment and have someone else retype it. People want to hand each other
concrete, ready-to-apply changes, and the page's owner wants to stay in
control of what actually lands.

## What it is

An editor selects text and proposes a replacement, insertion or deletion.
The proposal lives **beside the page, like a comment**: the page content does
not change. Suggestions show as blue highlights in the page and as cards in
a new **Suggestions** tab next to Comments. Anyone whose effective role on
the page is **Owner** accepts or rejects. Accepting applies the change to the
page as a normal edit.

## Decisions

Marked **(user)** where the user decided, **(design call)** where it was
settled during the design and the user has not weighed in.

| Question | Decision |
| --- | --- |
| Who decides? | **(user)** Anyone whose *effective* doc role is Owner (`DocAccess.role == Owner`, the same rule comment delete uses). Not `documents.created_by`. |
| Voting / auto-accept by other editors? | **(user)** No. Dropped. |
| New roles (Suggester, Commenter)? | **(user)** No. Roles stay Owner / Editor / Viewer. |
| Who can suggest? | **(design call)** Editor and above, the same as commenting. Owners can suggest too. Editors can still edit directly; suggesting is an extra option, not a restriction. Nothing is enforced on the server beyond "Editor+ may create". |
| Where do suggestions live? | **(user)** Beside the page, in Postgres, anchored by Yjs RelativePositions, the way comments are. Never as marks or nodes in the Y.Doc. |
| How do they look? | **(user, approved mockup)** Blue highlight in the page (comments stay amber), a small "+" marker for insertions, and cards in a Suggestions tab next to Comments. |
| What does the owner see if the target text changed after the suggestion was made? | **(user)** Both versions: the text as it was when suggested, and the text as it is now, next to the suggested text, with a "changed since" note. |
| Accepting a suggestion whose text changed | **(user)** Allowed. The suggested text replaces whatever is between the anchors *now*. |
| Text changes between the owner looking and clicking Accept | **(user)** Refuse and refresh. The server applies only if the text between the anchors is still exactly what the owner's card showed; otherwise it returns 409 `suggestion.changed`, and the card refreshes to its "changed since" form so the owner can accept again. |
| Target text deleted entirely (anchors no longer resolve, or the range collapsed) | **(design call)** The card says "The text this suggestion was about was removed". Accept is disabled. Reject and replies still work. |
| Who applies an accepted change? | **(design call)** The server, inside the room actor, in one yrs transaction (see §4). |
| Contributor credit for an accepted change | **(design call)** The **suggester** (`by_user` = suggester) because they wrote the words. The accepting owner is stored as `decided_by`. |
| Scope of one suggestion | **(design call)** Plain text inside **one text block** (one paragraph, heading, list item or table cell). Selections that cross blocks, or that contain atoms (images, attachments, boards, mentions, dates, internal links), disable "Suggest change" with an explanatory tooltip. Replacement text is plain text with no line breaks and takes the marks of the first replaced character (for an insertion, the marks of the character before the insertion point). |
| Editing a pending suggestion | **(design call)** Not in v1. The author withdraws it and makes a new one. |
| Text typed right at the edge of a suggested range | **(design call)** Stays **outside** the range at both ends, so accepting never deletes words that were added next to the selection. This needs a suggestion-specific anchor encoder (see Constraints); the comment encoder as it is would pull text typed just before the selection into the range. |
| Insertions in view mode, and into empty blocks | **(design call)** Insertions need a caret, so they are offered **only in edit mode** (toolbar "Suggest" button). Selection-based replace/delete work in both modes. Suggesting an insertion into an empty block is disabled ("Type in the block instead"). |
| Notifications | **(design call)** See §6. |

## Goals

- An editor can propose a replace / insert / delete in a few clicks, with an
  optional reason, without touching the page.
- Everyone with the page open sees new and resolved suggestions live.
- An owner can accept or reject from the card, and an accepted change lands
  exactly where it was meant to go.
- Suggestions have reply threads, like comments.

## Non-goals (v1)

- Voting or auto-accept.
- A suggest-only role, or server-side enforcement that someone may only
  suggest.
- Google-Docs-style inline track changes in the document itself.
- Formatting suggestions, block-level suggestions (tables, lists, images,
  boards), and selections spanning several blocks.
- Undo of an accepted/rejected suggestion (the mockup's Undo button), and
  "Accept all".
- Showing suggestions in exports (Markdown, DOCX/ODT, zip), search, public
  share pages, history previews or the task list.
- Email delivery (knot has none yet).
- Copying suggestions on duplicate or create-from-template (comments are not
  copied either).

## Anti-goals (would make it a failure even if it works)

- A pending suggestion changes what anyone reads in the page, in search, in
  an export or in the task list.
- An accepted change lands in the wrong place, or is applied twice. **When
  the server cannot be sure where the change goes, it refuses to apply** and
  the card falls back to "text was removed".
- A suggestion is marked accepted without having been applied, or applied
  without being marked accepted.
- Anyone who cannot read the page learns a suggestion exists (via
  notifications, listings or counts).

## Constraints from the codebase

These came out of a mapping pass over the editor, CRDT, ACL, comments,
notifications and content pipeline.

- **Keep pending text out of the Y.Doc.** `@tiptap/y-tiptap` 3.0.9 *deletes*
  any `Y.XmlText` carrying a mark the local schema does not know, and syncs
  the deletion (`y-tiptap.js` `createTextNodesFromYText`). There is no
  stale-tab detection, so an old tab would destroy in-doc suggestion marks.
  Every server pipeline (`to_markdown`, `knot-export` walk, FTS, task index)
  would also treat pending text as real. The sidecar model avoids all of this.
- **Anchors** follow the comment approach (base64 `Y.RelativePosition` pairs,
  `web/src/features/comments/anchor.ts`, stored as `bytea`), but **not the
  comment encoder byte for byte**. `absolutePositionToRelativePosition` from
  `@tiptap/y-tiptap` encodes every position with assoc = -1 ("right after the
  character before this point"). For a range, that ties the start to the
  character *before* the selection, so text typed just before the selection
  would fall inside the range and be deleted on accept. Suggestions need
  their own encoder: start tied to the **first selected character**
  (assoc ≥ 0), end tied to the **last selected character** (assoc < 0), and
  an insertion point tied to the character before it. The Rust spike decodes
  and tests exactly this encoding. Anchors stop
  resolving when the anchored items are deleted, and every Markdown round trip
  (history restore, `POST /markdown?mode=replace`, workspace import) clears
  the whole `default` fragment. After those, suggestions show as "text was
  removed". That is accepted for v1, as it is for comments.
- **Retyping the end of a range shrinks it.** The end anchor is tied to the
  last selected character. Yjs places text typed after a deleted character to
  the right of its tombstone, so selecting the suggested words and retyping
  them leaves only the untouched remainder inside the range. The card shows
  that remainder as "Now". This is accepted for v1.
- **Typing the range's first character just before it lands inside it.**
  y-tiptap diffs old and new text with lib0 `simpleDiff`, common prefix
  first, so a "b" typed directly before "brave" is recorded after the
  original "b" and the range reads "bbrave". The card shows "changed", so the
  owner sees both versions before accepting. This is accepted for v1.
- **An insertion whose anchored character is deleted is lost.** Pressing
  Enter just before an insertion point moves the text after it into a new
  block; y-tiptap leaves the anchored character behind as a tombstone, which
  would resolve to the split point. Client and server both treat a deleted
  anchor character as "removed", even after an undo brings the text back.
  Likewise a range whose ends land in different text nodes, a caret next to
  a line break with no text beside it, and a selection with a line break in
  a code block are refused.
- **The applied marker can't be forged.** The room records an apply in the
  `knot_applied_suggestions` root map with a per-suggestion token minted on
  the first claim and never sent to clients, so an editor writing that map
  cannot make an accept look done. The room persists the apply before it
  sends it to anyone, and the accept route runs in its own task with a 20 s
  deadline (under the 30 s request timeout, well under the sweep's 120 s), so
  a dropped request never strands a claim.
- **Positions differ between client and server.** Room docs count offsets in
  bytes, while Yjs clocks and the browser count UTF-16 units. The server must
  resolve anchors in UTF-16 and convert, or umlauts and emoji before the range
  shift the apply.
- **The server has never decoded a RelativePosition.** Server-side apply
  needs a yrs resolver for the exact bytes the client encodes against the
  y-tiptap binding (`Y.XmlText` item + offset). `knot-crdt` already uses
  `StickyIndex` in `nesting.rs`. **This is the riskiest piece. The plan should
  start with a spike that decodes client-made anchors in Rust and resolves
  them against a room's doc.** If the spike fails, stop and come back to the
  design; the fallback is client-side apply by the owner's browser, which
  changes the atomicity story.
- **Server-authored edits already exist**: `Event::PatchTaskChecked`,
  `ApplyUpdate` and `ReplaceWithMarkdown` in `crates/knot-crdt/src/room.rs`
  edit the doc in a server transaction, capture the delta with
  `observe_update_v1`, persist it through the writer under `by_user` and fan
  it out. Accept follows the same pattern. Server-applied edits never enter
  any client's Yjs undo stack; that is acceptable.
- **Multi-replica**: an apply must run in the doc's room; `Rooms::acquire`
  boots one if needed. Other replicas pick the change up through the
  existing bus.
- **Stores take no transaction** (`Arc<dyn …Store>`), so "claim then apply"
  must be built from conditional UPDATEs on a status column (§4).
- **Notification kinds** are a DB `CHECK` constraint, a Rust
  `NotificationKind` enum and the compile-enforced `KIND_LABEL` /
  `ACTORLESS_LABEL` tables in `web/src/features/notifications/notificationTarget.ts`.
- **Schema drift test**: `web/src/features/editor/schema.test.ts` pins the set
  of registered editor extensions, so a new highlight plugin must be added to
  its list. No new marks or nodes are added to `tools/schema.json`.

## Design

### 1. Data model

A new `suggestions` table (exact DDL is the builder's call):

- `id`, `doc_id` (FK, `ON DELETE CASCADE`), `author_id`
- `kind`: `replace` | `insert` | `delete`
- `position_y`, `position_y_end` (`bytea`, RelativePositions; equal for
  `insert`)
- `original_text`: the exact text of the range when suggested (empty for
  `insert`). Full text, not the 120-char comment snippet, because it is
  shown and compared.
- `context_before`: a short snippet before the anchor, captured when
  suggested, so an insertion card can say *after "…within 15 minutes"*
- `proposed_text` (empty for `delete`), `reason` (optional)
- `status`: `pending` | `applying` | `accepted` | `rejected` | `withdrawn`
- `decided_by`, `decided_at`, `created_at`, `updated_at`

Kind is derived, not chosen: a non-empty selection with replacement text is
`replace`, with empty replacement text `delete`; a collapsed caret is
`insert`. Limits: `proposed_text` ≤ 10 000 chars, `reason` ≤ 4 096 (as
comment bodies), both checked in the DB and in the route.

**Replies:** they behave exactly like comment replies (plain-text body,
mentions, reply notifications, author-only edit, author-or-owner delete).
Whether they get their own table or reuse `comments` with a
`suggestion_id` column is the builder's call. Reusing `comments` must not
make suggestion replies show up as comment threads.

### 2. API

Mounted under `/api/docs/{id}/…`, so `require_doc_role_mw` (tenancy,
archived-doc 409s) applies.

| Route | Who | Notes |
| --- | --- | --- |
| `POST /suggestions` | Editor+ | Body: anchors, `original_text`, `context_before`, `proposed_text`, `reason`. Rejects a no-op (`proposed_text == original_text`). |
| `GET /suggestions?include_resolved=` | any reader | Pending by default. Resolved ones include `decided_by` / `decided_at`. |
| `POST /suggestions/{sid}/accept` | Owner | Body: `seen_text`, the "Now" text the owner's card showed (empty for `insert`). See §4. 409 `suggestion.not_pending`, 409 `suggestion.anchor_lost`, 409 `suggestion.changed`. |
| `POST /suggestions/{sid}/reject` | Owner | Conditional `pending → rejected`. |
| `POST /suggestions/{sid}/withdraw` | the author | Conditional `pending → withdrawn`. |
| replies (create/list/edit/delete) | as comment replies | |

Every `{sid}` route checks that the suggestion belongs to `{id}`, as
`ensure_comment_in_doc` does for comments.

### 3. Staleness, worked out on the client

The client resolves each pending suggestion's anchors through the ySync
mapping (as comment highlights do) and compares the current text in the range
with `original_text`:

- **Same:** normal card ("Now" / "Suggested").
- **Different:** "Changed since" card with three rows: *When suggested*
  (`original_text`), *Now* (current text), *Suggested*. Accept stays enabled.
- **Unresolvable, collapsed (for `replace`/`delete`), or no longer inside one
  text block:** "The text this suggestion was about was removed." Accept
  disabled.

The server makes the same decision independently on accept (§4), and also
checks that the text is still what the card showed.

### 4. Accept (server-side apply)

1. Conditional UPDATE `pending → applying` (setting `decided_by`). Zero rows →
   409 `suggestion.not_pending`. This is the "claim" that prevents double
   apply when two owners click at once.
2. Send a new room event (e.g. `Event::ApplySuggestion { anchors, kind,
   seen_text, proposed_text, by_user: author_id }`) to the doc's room and
   await the reply. In one yrs transaction it resolves both anchors, checks
   that they land in the same `XmlText` with start ≤ end (and start < end for
   `replace`/`delete`), **checks that the text between them equals
   `seen_text`**, deletes the range and inserts `proposed_text` with the marks
   described in Decisions. It captures and persists the update like
   `PatchTaskChecked` does, credited to the suggester. If a check fails,
   nothing is written.
3. On success: `applying → accepted` with `decided_at`. On "cannot resolve":
   `applying → pending` and 409 `suggestion.anchor_lost`; the client then
   shows the removed state. On "text differs from `seen_text`":
   `applying → pending` and 409 `suggestion.changed`; the client re-reads the
   current text, re-renders the card (normally as "changed since") with a
   short note "The text changed while you were looking. Check it and accept
   again.", and does not retry by itself.
4. **Crash between steps 2 and 3** must not leave the suggestion stuck in
   `applying` forever or apply it twice. How to recover (for example a
   startup or periodic sweep that checks rows stuck in `applying`, or making
   the room step record the suggestion id in the same persisted batch) is the
   builder's call, as long as the anti-goals hold.

Reject and withdraw never touch the Y.Doc.

### 5. Realtime

Same mechanism as comments: after every suggestion or reply mutation the
server does a fire-and-forget `pg_notify`, a listener turns it into a server
frame for every connection in the doc's room, and `KnotProvider` emits an
event so `KnotEditor` invalidates the suggestions query. Whether this shares
the `doc_comments` channel / `MSG_COMMENTS` frame or gets its own (e.g. a
`MSG_SUGGESTIONS` type) is the builder's call. An accepted change also
reaches everyone as a normal CRDT update.

### 6. Notifications

Two new kinds, delivered through the existing inbox (migration for the
`CHECK`, `NotificationKind` variant, web labels):

- **`suggestion`**, on create, to the page's owners except the author.
  **(design call)** "Owners" here is deliberately narrower than "everyone who
  may accept": on an unrestricted page every *workspace* Owner is an
  effective Owner, and notifying all of them on every suggestion would be
  noise. Recipients are users with an explicit `owner` grant on the page or
  an inheriting ancestor, plus `created_by` if they are still an effective
  Owner. Only if that set is empty does it fall back to all workspace Owners.
  Recipients are filtered by `acl.effective_role` as comment notifications
  are.
- **`suggestion_decided`**, on accept or reject, to the author, when someone
  else decided.

Replies reuse the comment `reply` / `mention` behaviour. Dedupe keys follow
the existing `kind:<id>` convention. Deep link: `/doc/<id>?suggestion=<sid>`
opens the Suggestions tab, focuses the card (showing resolved if needed) and
scrolls the page to the highlight.

### 7. Web UI

Follows the approved mockup.

- **Making one:** "Suggest change" sits next to "Add comment" in the selection
  float (view mode and edit mode, Editor+). A "Suggest" button in the editor
  toolbar covers the collapsed-caret (insert) case; the toolbar only exists
  in edit mode, so insertions are edit-mode only. The composer opens in the sidebar: the current text (struck
  through), a "Replace with" field prefilled with the selection, an optional
  "Why?", and the note "The page stays as it is until an owner accepts."
  While the composer is open, the target range keeps a selection-style
  highlight so the user can see what they are changing.
- **Seeing them:** a plugin-only extension, modelled on
  `CommentsHighlightExtension`, draws blue inline decorations for pending
  replace/delete ranges and a small "+" widget for insert points. Clicking
  one opens the Suggestions tab and focuses the card; focusing a card
  emphasises its highlight.
- **Sidebar:** the existing comment sidebar gets two tabs, "Comments *n*" and
  "Suggestions *n*" (pending count). Cards show the author, time, kind badge,
  the change, the reason, replies, and Accept / Reject for owners, "Waiting
  for an owner" for others, and Withdraw for the author. A collapsed
  "Resolved" section lists accepted / rejected / withdrawn ones with who and
  when.
- **Viewers** see the tab read-only.

### 8. Interactions with the rest of knot

- **Archived page:** read-only; mutations already 409 through the middleware.
- **Purge:** cascades.
- **Duplicate / template / workspace export and import:** suggestions are not
  copied.
- **History restore, Markdown replace import:** anchors stop resolving and
  pending suggestions show as removed (see Constraints).
- **Member removal:** suggestions and replies stay, with the author's name,
  like comments.
- **Restricted pages:** all listings and notifications go through the existing
  ACL checks, so nothing leaks.

### 9. Testing

- **Rust unit tests:** the anchor resolver against anchors produced by the
  real JS encoder (generate fixtures from `anchor.ts` against a known doc),
  including after concurrent inserts, after deleting the range, and across
  block splits.
- **Rust room tests:** apply for each kind, marks inheritance, refusal on a
  cross-block range, and a single persisted update with the right `by_user`.
- **Server integration tests:** route permissions (Viewer, Editor, Owner,
  author), cross-doc ids, conditional status transitions, concurrent accepts
  (exactly one wins), notification recipients including the workspace-owner
  fallback and the restricted-page filter.
- **Web unit tests:** staleness classification, kind derivation, the
  highlight plugin, and the updated `schema.test.ts` extension list.
- **e2e (Playwright, multi-context, modelled on `comments-realtime.spec.ts`
  and `comment-anchors.spec.ts`):** an editor suggests → an owner sees it live
  and accepts → the text changes for both; a direct edit after the suggestion
  → the "changed since" card → accept replaces the current text; a co-editor
  changes the text between the owner's card rendering and the click → 409
  `suggestion.changed`, the card refreshes and a second accept succeeds; a
  deleted target → Accept disabled; withdraw; reject; inbox notification deep
  link.
- **Rust room test** for the `seen_text` check: a mismatch writes nothing.

## Left to the builder

The DDL and table split for replies; whether realtime shares the comments
channel; the recovery mechanism for `applying`; error-code wording beyond the
ones named here; component structure in the web app; and any per-page cap on
pending suggestions.
