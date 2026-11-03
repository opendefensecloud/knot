# Change suggestions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an editor propose a replace / insert / delete on a page without touching it, shown as blue highlights and cards in a Suggestions sidebar tab. Anyone whose effective doc role is Owner accepts (the server applies the change) or rejects it.

**Architecture:** Suggestions live in Postgres beside the page, anchored by Yjs RelativePositions like comments, and never enter the `default` fragment. On accept, the server claims the row (`pending → applying`). It then sends `Event::ApplySuggestion` to the doc's room. The room resolves the anchors in one yrs transaction, checks the text the owner saw, replaces the range, and records the suggestion id in a side `Y.Map` within the same transaction. That record makes the apply idempotent and lets a sweep recover rows stuck in `applying`. Realtime reuses the `doc_comments` NOTIFY channel with a `topic` field. The inbox gets two new notification kinds.

**Tech Stack:** Rust (axum, sqlx/Postgres, yrs 0.27.3, tokio), React + TipTap 3 + `@tiptap/y-tiptap` 3.0.9 + yjs 13.6.32, TanStack Query, zustand, vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-11-02-change-suggestions-design.md`. Read it first. Its Decisions table is normative. Mockup: <https://claude.ai/artifact/RxYnVQ2xZNy2b5Yqw3ChMf> (ignore its "Undo" button).

## Global Constraints

- **Never `git commit` or `git push`.** Leave your task's changes **unstaged**. The orchestrator reviews `git diff` and commits after review (commits are SSH-signed via 1Password). Work happens on branch `feat/change-suggestions`.
- Suggestion content never enters the `default` XmlFragment until accepted. No change to `tools/schema.json`.
- Error codes, exactly: `suggestion.not_found` (404), `suggestion.not_pending` (409), `suggestion.anchor_lost` (409), `suggestion.changed` (409), `suggestion.not_author` (403), `acl.owner_required` (403), `suggestion.invalid` (400), `suggestion.invalid_anchor` (400), `suggestion.too_long` (422), `suggestion.body_too_large` (413). Reused: `acl.editor_required`, `auth.session_required`, `doc.archived`.
- Limits, counted in characters (`.chars().count()` in Rust, matching SQL `length()`): `proposed_text` ≤ 10 000, `original_text` ≤ 10 000, `context_before` ≤ 200, `reason` ≤ 4 096, reply `body` 1..4 096. `proposed_text` must not contain `\n` or `\r`. Request body cap: 256 KiB, which covers the worst case of escaped surrogate pairs at every limit.
- Notification kinds, exactly: `suggestion`, `suggestion_decided`. Dedupe keys: `suggestion:<sid>`, `suggestion_decided:<sid>`, `suggestion_reply:<reply_id>`, `suggestion_mention:<reply_id>`. `target_kind` is `"suggestion"` and `target_id` is the suggestion id. `data` always has `suggestion_id`, `doc_title` and `excerpt` (140 chars).
- Migrations: `migrations/20261102120000_suggestions.sql`. Forward-only, with header `-- suggestions\n-- Created 2026-11-02`. After editing it, `touch crates/knot-test-support/src/lib.rs`; if tests then report a missing column, `cargo clean -p knot-test-support -p knot-storage`.
- Rust gates for every backend task: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean. Tests: `cargo test -p <crate> [--test <file>]` (nextest is not installed). DB tests need Postgres. Per the dev-stack notes, use the throwaway `knot-task13-pg` on :5434 via `KNOT_TEST_DATABASE_URL=postgres://knot:knot@127.0.0.1:5434/knot` if :5432 is out of `/dev/shm`.
- Web gates (cwd `web/`): `pnpm test <files>`, `pnpm tsc`, `pnpm lint` (max-warnings 0).
- Match surrounding style: doc comments on public items, descriptive names, no new crates or npm packages. Web API clients use plain TS types + `apiFetch` (as in `web/src/lib/comments.api.ts`), not valibot.
- `ySyncPluginKey`, `absolutePositionToRelativePosition` and friends are imported only from `@tiptap/y-tiptap`, never `y-prosemirror`.
- data-testids, exactly (web and e2e tasks share them): `suggest-change-float`, `toolbar-suggest`, `sidebar-tab-comments`, `sidebar-tab-suggestions`, `suggestion-composer`, `suggestion-composer-replace`, `suggestion-composer-reason`, `suggestion-composer-submit`, `suggestion-composer-cancel`, `suggestion-card-<sid>` (attrs `data-active`, `data-state` = `same|changed|removed|unknown|accepted|rejected|withdrawn`), `suggestion-accept-<sid>`, `suggestion-reject-<sid>`, `suggestion-withdraw-<sid>`, `suggestion-waiting-<sid>`, `suggestion-note-<sid>`, `suggestion-show-resolved`, and reply composer `suggestion-reply-input-<sid>` / `suggestion-reply-submit-<sid>`. Editor DOM: `.suggestion-highlight[data-suggestion-id]`, `.suggestion-highlight--active`, `.suggestion-insert-marker[data-suggestion-id]`.
- UI copy, exactly:
  - Float: "Suggest change". Toolbar button label: "Suggest an insertion".
  - Composer: title "Suggest a change"; labels "Current text", "Replace with" (insert: "Insert"), "Why? (optional)"; note "The page stays as it is until an owner accepts."; buttons "Cancel", "Send suggestion".
  - Card row labels: "Now", "Suggested", "Remove", "Add", "When suggested".
  - Card notes: "Changed since it was suggested" (changed); "The text this suggestion was about was removed." (removed); "The text changed while you were looking. Check it and accept again." (after a 409 `suggestion.changed`); "Waiting for an owner" (non-owners).
  - Buttons: "Accept", "Reject", "Withdraw", "Reply".
  - Resolved section heading "Resolved"; status labels "Accepted", "Rejected", "Withdrawn".
  - Disabled-float tooltips: cross_block "Select text inside a single paragraph to suggest a change"; inline_atom "Suggestions can't include mentions, dates, page links or line breaks"; empty_block "Type in the block instead".

## Review Focus

1. **Non-ASCII text before or inside the range** (umlauts, `€`, emoji with surrogate pairs) within one Yjs item. Room docs use `OffsetKind::Bytes`, and `StickyIndex::get_offset` mixes UTF-16 clock offsets with byte lengths, so a naive apply lands in the wrong place. Owned by Task 1 (`non_ascii_before_anchor_in_same_item`, `emoji_surrogate_pair_range`).
2. **A suggestion that starts on the page's very first character.** `relativePositionToAbsolutePosition` returns null there (`isMisresolvedTextPosition`), so the card would wrongly show "removed". Owned by Task 7 (`decodes_start_anchor_on_first_character_of_page`).
3. **Text typed directly before or after the selection** between suggesting and accepting must stay outside the range and survive accept. Owned by Task 1 (`edge_inserts_stay_outside_range`) and Task 7 (`edge_typing_stays_outside_range`).
4. **Accept handled by a replica whose room lags** behind the edit that created the anchored text. It must not answer `anchor_lost` spuriously. Owned by Task 2 (`apply_replays_bus_before_resolving`).
5. **Suggestions inside nested blocks** (table cell, list item, heading) apply correctly. Selections touching a mention, a date or a page link (link marks with `knot://` hrefs) are refused before anything is sent. Owned by Task 1 (`apply_inside_table_cell`) and Task 7 (`scope_refuses_internal_link_marks`).

---

### Task 1: Spike + pure apply — resolve client anchors in yrs

This is the spec's risk gate. **If the Yjs-made fixtures in Step 1 cannot be resolved to the right ranges, stop and report back. Do not continue with the plan.**

**Files:**
- Create: `crates/knot-crdt/src/suggestions.rs`. Add `pub mod suggestions;` to `crates/knot-crdt/src/lib.rs` next to `pub mod tasks;`.
- Test: `#[cfg(test)] mod tests` in the same file (copy the doc builders it needs from `crates/knot-markdown/tests/common/mod.rs`, or build with `XmlElementPrelim`/`XmlTextPrelim` like `tasks.rs:60-72`).

**Interfaces:**
- Produces:
  ```rust
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  pub enum SuggestionKind { Replace, Insert, Delete }

  /// Everything the room needs to apply one accepted suggestion.
  #[derive(Debug, Clone)]
  pub struct ApplySuggestionRequest {
      pub suggestion_id: Uuid,
      pub kind: SuggestionKind,
      pub start: Vec<u8>,          // raw Y.RelativePosition v1 bytes (not base64)
      pub end: Vec<u8>,
      pub seen_text: String,       // "" for Insert
      pub proposed_text: String,   // "" for Delete
  }

  #[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
  pub enum ApplySuggestionError {
      #[error("anchor lost")] AnchorLost,
      #[error("text changed")] Changed,
      #[error("already applied")] AlreadyApplied,
      #[error("decode: {0}")] Decode(String),
      #[error("persist: {0}")] Persist(String),
  }

  /// Root Y.Map recording applied suggestion ids (value: Any::Bool(true)).
  pub const APPLIED_MAP: &str = "knot_applied_suggestions";

  /// The live XmlText a range lives in, plus its bounds in BYTE offsets of the
  /// live (OffsetKind::Bytes) doc, and the plain text between them.
  pub struct ResolvedRange { pub text: XmlTextRef, pub start_byte: u32, pub end_byte: u32, pub current_text: String }

  pub fn resolve_range<T: ReadTxn>(txn: &T, start: &[u8], end: &[u8]) -> Result<ResolvedRange, ApplySuggestionError>;
  pub fn is_applied<T: ReadTxn>(applied: &MapRef, txn: &T, suggestion_id: Uuid) -> bool;
  /// All checks run before the first mutation, so an Err leaves `txn` empty.
  pub fn apply_suggestion(applied: &MapRef, txn: &mut TransactionMut, req: &ApplySuggestionRequest) -> Result<(), ApplySuggestionError>;
  ```
- `resolve_range` algorithm (from the scout's verified approach):
  1. Decode with `StickyIndex::decode_v1` (trait `yrs::updates::decoder::Decode`). An error becomes `Decode`.
  2. Build a UTF-16 replica: `Doc::with_options(Options { offset_kind: OffsetKind::Utf16, ..Default::default() })` and apply `txn.encode_state_as_update_v1(&StateVector::default())`.
  3. `get_offset` both anchors in the replica.
  4. Return `AnchorLost` on: `None`, a non-`TypeRef::XmlText` branch, `branch.is_deleted()`, different branches, or `start.index > end.index`.
  5. Map the branch to the live doc with `offset.branch.id().get_branch(txn)`.
  6. Read plain text from `diff(txn, YChange::identity)`. Never use `get_string`, which renders XML tags. **Refuse when unsure:** return `AnchorLost` if the XmlText contains any non-`Any::String` chunk, or if the summed UTF-16 length of the diff strings differs from the replica XmlText's `len()`. A mismatch means hidden or embedded items, so string offsets would not match item positions.
  7. Convert the UTF-16 indices to byte offsets by walking `chars()` and summing `len_utf16`/`len_utf8`. An index that falls between the two halves of a surrogate pair is `AnchorLost`.
- `apply_suggestion` order:
  1. `is_applied` → `AlreadyApplied`.
  2. `resolve_range`.
  3. For `Replace`/`Delete`, `start_byte == end_byte` → `AnchorLost`.
  4. `current_text != seen_text` → `Changed`.
  5. `remove_range` then `insert_with_attributes(start_byte, proposed_text, attrs)`.
     - For `Replace`, `attrs` is the cloned `Attrs` of the diff chunk containing the first replaced char.
     - For `Insert`, it is the char before the point, or at offset 0 the char after, **with the `link` key removed**. Otherwise an insertion next to a mention, date or page link would become part of that chip, and links are non-inclusive in the editor anyway.
     - Use `Attrs::new()` if there is none.
     - Skip the insert for empty `proposed_text`.
  6. `applied.insert(txn, suggestion_id.to_string(), true)`.

- [ ] **Step 1: Write the failing tests.**
  - `decodes_yjs_made_fixtures`: build the live doc from the Yjs update hex `0107070007010764656661756c74030970617261677261706807000700060400070102c3a48407020462e282ac840704026364c60702070304626f6c64027b7dc60704070504626f6c64046e756c6c00` (clientID 7, paragraph > XmlText `äb€cd`, bold on `b€`). Assert:
    - `resolve_range(start=hex 00070400, end=hex 00070541).current_text == "€c"`;
    - `resolve_range(02070141, 02070141)` is an empty range at byte 0;
    - `resolve_range(00070400, 02070100).current_text == "€cd"`.
  - `non_ascii_before_anchor_in_same_item`: build the doc with `Doc::with_client_id(7)` and one insert of `"äbc"` (chars at clocks 2, 3, 4). Anchors are `StickyIndex::from_id(ID::new(yrs::block::ClientID::new(7), 4), Assoc::After)` and `(…, 4, Assoc::Before)`, both encoded with `encode_v1`. `ClientID` has no `From<integer>`. Expect `current_text == "c"` and `start_byte == 3`.
  - `surrogate_split_index_is_anchor_lost`: an anchor pointing at the second UTF-16 unit of `😀`.
  - `insert_next_to_link_drops_link_mark`: text with a `link` attribute `{href: "knot://user/…"}` right before the point. The inserted text has no `link` key.
  - `block_split_is_anchor_lost`: remove the tail of the XmlText (covering the range) and push it as a new paragraph with a new XmlText, the way y-prosemirror splits a block. Expect `AnchorLost`. Anchors in the part that stayed still resolve.
  - `embedded_item_in_text_is_anchor_lost`: an XmlText with an embed (`insert_embed`) gives `AnchorLost`.
  - `emoji_surrogate_pair_range`: `"a😀b"`; select `😀` (UTF-16 clocks 3..4). Expect `current_text == "😀"`, and that the replace gives `"aXb"`.
  - `replace_keeps_marks_of_first_char`, `insert_at_block_start_takes_next_char_marks`, `delete_removes_range`.
  - `changed_text_is_refused_and_writes_nothing`: wrap the call with `observe_update_v1` and assert no update was emitted.
  - `collapsed_range_is_anchor_lost`: delete the anchored chars first, then apply.
  - `deleted_paragraph_is_anchor_lost`.
  - `cross_text_range_is_anchor_lost`: two paragraphs, start in one and end in the other.
  - `edge_inserts_stay_outside_range`: insert `"XX"` immediately before the first selected char and `"YY"` immediately after the last one. Then `current_text` is unchanged, and the apply leaves `XX…YY` in place.
  - `apply_inside_table_cell`: `table > table_row > table_cell > paragraph > text`.
  - `second_apply_is_already_applied`.
- [ ] **Step 2:** `cargo test -p knot-crdt suggestions` fails (module missing).
- [ ] **Step 3:** Implement `crates/knot-crdt/src/suggestions.rs` per the Interfaces.
- [ ] **Step 4:** `cargo test -p knot-crdt suggestions` passes. Then run `cargo clippy -p knot-crdt --all-targets -- -D warnings`.

### Task 2: Room events `ApplySuggestion` and `SuggestionApplied`

**Files:**
- Modify: `crates/knot-crdt/src/room.rs`. Add the variants next to `PatchTaskChecked` (`room.rs:137`) and their handlers next to `room.rs:477-535`.
- Modify: `crates/knot-crdt/src/lib.rs`. Re-export `suggestions::{ApplySuggestionError, ApplySuggestionRequest, SuggestionKind}`.
- Test: `room.rs` `mod tests`. Reuse `RecordingUpdates` (by_user per row) and `CapturingUpdates` (bytes per row).

**Interfaces:**
- Consumes: Task 1 `apply_suggestion`, `is_applied`, `APPLIED_MAP`.
- Produces:
  ```rust
  Event::ApplySuggestion { request: ApplySuggestionRequest, by_user: Option<Uuid>, reply: oneshot::Sender<Result<i64, ApplySuggestionError>> }
  Event::SuggestionApplied { suggestion_id: Uuid, reply: oneshot::Sender<bool> }
  ```
- `ApplySuggestion` handler:
  1. Call `self.replay_since_watermark().await` first, so a lagging replica sees the latest edits.
  2. Copy the PatchTaskChecked block exactly. Call `get_or_insert_xml_fragment("default")` and `get_or_insert_map(APPLIED_MAP)` BEFORE `transact_mut` (deadlock note at `room.rs:396-399`). Capture with `observe_update_v1`, scope the txn, and `drop(sub)` after it.
  3. Persist with `PersistJob { by_user_id: by_user, .. }`. The caller passes the suggester. Fan out with `wrap_sync_update` and add the metrics label `"source" => "suggestion"`.
  4. A successful apply with no captured update is `Persist("no update")`. Persist failures become `Persist(..)`.
  5. **On any `Persist` error, evict the room** after replying, by cancelling its shutdown token as the nesting refusal path does (`refuse_applied`). No live room may keep an unpersisted marker. Connected clients reconnect. A client that received the fan-out pushes text and marker back together in its resync, so they are persisted as a pair, or neither is. Either way the marker and the text agree, which is what the sweep relies on.
- `SuggestionApplied` handler: `replay_since_watermark`, then reply with `is_applied`. The marker in the doc is the single source of truth for "was this applied".

- [ ] **Step 1: Failing tests.**
  - `apply_suggestion_persists_one_update_by_suggester`: seed with `Event::ApplyUpdate`, apply. Assert exactly one new recorded row with `by_user == Some(suggester)`, and `ExportState` shows the new text.
  - `apply_suggestion_refusal_persists_nothing`: a `Changed` result adds no row.
  - `suggestion_applied_reports_true_after_apply_false_before`.
  - `apply_replays_bus_before_resolving`. The in-memory fakes return nothing from `since()`, so back this test with `PgUpdatesStore` on `knot_test_support::fresh_db()`, as `room_replays_prior_updates_on_boot` does.
    1. Spawn room B and do one `ExportState` round trip so its immediate first catch-up tick has passed.
    2. Write an update that adds new text straight into the store with `insert_batch`. That publishes nothing on the bus.
    3. Send `ApplySuggestion` anchored in that new text to B, and expect `Ok`.
    4. Check that deleting the handler's `replay_since_watermark` call makes the test fail with `AnchorLost`.
  - `persist_failure_evicts_room`: an updates store whose `insert_batch` errors. The apply replies `Persist(..)` and the room's shutdown token is cancelled.
- [ ] **Step 2:** `cargo test -p knot-crdt room::tests::apply_suggestion` fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-crdt` passes, including every pre-existing room test.

### Task 3: Migration and `SuggestionStore`

**Files:**
- Create: `migrations/20261102120000_suggestions.sql`
- Create: `crates/knot-storage/src/suggestions.rs`. Register it in `crates/knot-storage/src/lib.rs` (`pub mod suggestions;` + `pub use suggestions::{…}`, alphabetical).
- Modify: `crates/knot-storage/src/notifications.rs`. Add `Suggestion` and `SuggestionDecided` with `as_str` `"suggestion"` / `"suggestion_decided"`.
- Modify: `crates/knot-storage/tests/migrations_apply.rs:30-54`. Insert `"suggestion_replies"` and `"suggestions"` between `"share_tokens"` and `"users"`.
- Modify: `e2e/support/reset.ts`. Add `suggestion_replies` and `suggestions` to `TABLES` next to `comments`.
- Test: `crates/knot-storage/tests/suggestions.rs` (setup pattern from `crates/knot-storage/tests/notifications.rs:12-48`).

**Interfaces:**
- Migration contents:
  - `suggestions`:
    - `id uuid PK`, `doc_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE`, `author_id uuid NOT NULL REFERENCES users(id)`;
    - `kind text NOT NULL CHECK (kind IN ('replace','insert','delete'))`;
    - `position_y bytea NOT NULL`, `position_y_end bytea NOT NULL`;
    - `original_text text NOT NULL`, `context_before text NOT NULL DEFAULT ''`, `proposed_text text NOT NULL`, `reason text NULL`;
    - `status text NOT NULL DEFAULT 'pending' CHECK IN ('pending','applying','accepted','rejected','withdrawn')`;
    - `decided_by uuid NULL REFERENCES users(id)`, `decided_at timestamptz NULL`, `created_at`, `updated_at`;
    - length CHECKs per Global Constraints;
    - indexes `(doc_id, created_at)` and `(updated_at) WHERE status = 'applying'`.
  - `suggestion_replies`: `id`, `suggestion_id REFERENCES suggestions(id) ON DELETE CASCADE`, `author_id`, `body CHECK length 1..4096`, `created_at`, `updated_at`, `deleted_at NULL`.
  - Notifications: `ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check; ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN ('mention','reply','task_assigned','task_due','doc_shared','suggestion','suggestion_decided'));`
- Produces (in `knot_storage`):
  ```rust
  pub enum SuggestionKind { Replace, Insert, Delete }              // serde lowercase; as_str()/parse()
  pub enum SuggestionStatus { Pending, Applying, Accepted, Rejected, Withdrawn }  // serde lowercase
  #[derive(Serialize)] pub struct Suggestion {
      pub id: Uuid, pub doc_id: Uuid, pub author_id: Uuid, pub kind: SuggestionKind,
      #[serde(with = "base64_bytes")] pub position_y: Vec<u8>,
      #[serde(with = "base64_bytes")] pub position_y_end: Vec<u8>,
      pub original_text: String, pub context_before: String, pub proposed_text: String,
      pub reason: Option<String>, pub status: SuggestionStatus,
      pub decided_by: Option<Uuid>, pub decided_at: Option<DateTime<Utc>>,
      pub created_at: DateTime<Utc>, pub updated_at: DateTime<Utc>,
  }
  pub struct NewSuggestion { pub doc_id, author_id, kind, position_y: Vec<u8>, position_y_end: Vec<u8>, original_text, context_before, proposed_text: String, reason: Option<String> }
  #[derive(Serialize)] pub struct SuggestionReply { pub id, suggestion_id, author_id: Uuid, pub body: String, pub created_at, updated_at: DateTime<Utc> }
  pub enum SuggestionStoreError { NotFound, NotPending, TooLong, Sqlx(#[from] sqlx::Error) }
  #[async_trait] pub trait SuggestionStore: Send + Sync + 'static {
      async fn create(&self, s: &NewSuggestion) -> Result<Suggestion>;
      async fn get(&self, id: Uuid) -> Result<Suggestion>;
      /// include_resolved=false → pending + applying only. Ordered by created_at.
      async fn list(&self, doc_id: Uuid, include_resolved: bool) -> Result<Vec<Suggestion>>;
      /// pending → applying, sets decided_by and updated_at = now(). NotPending if no row moved.
      async fn claim_for_accept(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion>;
      /// (applying | pending) → accepted, sets decided_at and updated_at. The doc's applied marker
      /// is authoritative, so this also finishes a row a sweep released meanwhile. NotPending otherwise.
      async fn finish_accept(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion>;
      /// applying → pending, clears decided_by, bumps updated_at.
      async fn release(&self, id: Uuid) -> Result<()>;
      /// Like release, but only if updated_at still equals `seen_updated_at` (the sweep's compare-and-set).
      async fn release_if_unchanged(&self, id: Uuid, seen_updated_at: DateTime<Utc>) -> Result<bool>;
      async fn reject(&self, id: Uuid, decided_by: Uuid) -> Result<Suggestion>;
      async fn withdraw(&self, id: Uuid) -> Result<Suggestion>;
      async fn list_stuck_applying(&self, older_than: DateTime<Utc>) -> Result<Vec<Suggestion>>;
      async fn create_reply(&self, suggestion_id: Uuid, author_id: Uuid, body: &str) -> Result<SuggestionReply>;
      /// Non-deleted replies of every suggestion of the doc, oldest first.
      async fn list_replies(&self, doc_id: Uuid) -> Result<Vec<SuggestionReply>>;
      async fn get_reply(&self, id: Uuid) -> Result<SuggestionReply>;
      async fn update_reply(&self, id: Uuid, body: &str) -> Result<SuggestionReply>;
      async fn delete_reply(&self, id: Uuid) -> Result<()>;   // soft delete
  }
  pub struct PgSuggestionStore { pool: PgPool }  // pub fn new(pool) ; pub fn pool(&self) -> &PgPool
  ```
- `base64_bytes` is a private non-Option serde module in `suggestions.rs`, modelled on `comments.rs:19-44`.
- Every status change is one conditional `UPDATE … WHERE id = $1 AND status = '<from>' RETURNING …` (pattern from `comments.rs:351-371`). Zero rows means `NotPending`, or `NotFound` if the row doesn't exist.

- [ ] **Step 1: Failing tests** in `crates/knot-storage/tests/suggestions.rs`:
  - `create_get_list_roundtrip`, including that `list(include_resolved=false)` hides rejected ones;
  - `claim_is_exclusive`: two concurrent `claim_for_accept` via `tokio::join!` give exactly one Ok and one `NotPending`;
  - `release_returns_to_pending`, `finish_from_applying_or_pending_only`, `reject_and_withdraw_only_from_pending`;
  - `list_stuck_applying_respects_age`: create a row, backdate `created_at`/`updated_at` by 10 min via SQL, then claim it. `list_stuck_applying(now - 120 s)` is empty because the claim bumped `updated_at`.
  - `release_if_unchanged_is_compare_and_set`;
  - `too_long_proposed_text_is_rejected`: 10 001 chars gives `TooLong`, checked in Rust before SQL;
  - `replies_crud_and_soft_delete`;
  - `doc_purge_cascades`;
  - `new_notification_kinds_insert`: emit one `Suggestion` and one `SuggestionDecided` through `PgNotificationStore`. This proves the constraint name was right.
- [ ] **Step 2:** `touch crates/knot-test-support/src/lib.rs && cargo test -p knot-storage --test suggestions` fails.
- [ ] **Step 3:** Implement the migration and the store.
- [ ] **Step 4:** `cargo test -p knot-storage` passes, including `migrations_apply`.

### Task 4: REST routes — create, list, reject, withdraw, replies, realtime

**Files:**
- Create: `crates/knot-server/src/routes/api/suggestions.rs`. Add `pub mod suggestions;` in `routes/api/mod.rs`, and `.merge(crate::routes::api::suggestions::routes())` next to the comments merge at `routes/api/docs.rs:104`, so `require_doc_role_mw` applies. The doc path param must be `{id}`.
- Modify: `crates/knot-server/src/lib.rs`. Add field `pub suggestions: Option<Arc<dyn SuggestionStore>>`, set `None` in `in_memory()`, and construct it in `with_pool()`.
- Modify: `crates/knot-server/src/routes/api/comments.rs`. Make `require_editor`, `internal` and `extract_mentions` `pub(crate)` and reuse them. Do not change their behaviour.
- Test: create `crates/knot-server/tests/suggestions_integration.rs`. Copy the file-local helpers from `comments_integration.rs` (`state_with_seeded`, `add_bob`, `add_member`, `grant`, `login`, `post_json`, `get_json`, `patch_json`, `delete_req`). Modify `crates/knot-server/tests/archive_integration.rs` (`json_writes` list, ~280-345).

**Interfaces:**
- Consumes: Task 3 store.
- Produces (HTTP, JSON bodies):
  - `POST /api/docs/{id}/suggestions`, Editor+.
    - Body: `{kind, position_y, position_y_end, original_text, context_before, proposed_text, reason?}`. Anchors are base64.
    - Returns 201 `Suggestion`.
    - Validation, in this order:
      1. body ≤ 256 KiB, else 413 `suggestion.body_too_large`;
      2. JSON parses, else 400 `bad_request`;
      3. base64 decodes AND `yrs::StickyIndex::decode_v1` accepts both anchors (yrs is already a knot-server dependency), else 400 `suggestion.invalid_anchor`;
      4. the kind matches the text: replace needs both texts non-empty and different, delete needs `original_text` non-empty and `proposed_text` empty, insert needs `original_text` empty and `proposed_text` non-empty; and `proposed_text` has no `\n`/`\r`. Otherwise 400 `suggestion.invalid`;
      5. limits, else 422 `suggestion.too_long`.
  - `GET /api/docs/{id}/suggestions?include_resolved=bool`, any reader. Returns 200 `[SuggestionWithReplies]`, where `#[derive(Serialize)] struct SuggestionWithReplies { #[serde(flatten)] suggestion: Suggestion, replies: Vec<SuggestionReply> }`. Uses one `list` plus one `list_replies` query.
  - `POST /api/docs/{id}/suggestions/{suggestion_id}/reject`, Owner (else 403 `acl.owner_required`). Returns 200 `Suggestion`, or 409 `suggestion.not_pending`.
  - `POST /api/docs/{id}/suggestions/{suggestion_id}/withdraw`, author only (else 403 `suggestion.not_author`). Returns 200 or 409.
  - `POST /api/docs/{id}/suggestions/{suggestion_id}/replies`, Editor+. Body `{body, mentions?: [uuid]}`. Returns 201 `SuggestionReply`.
  - `PATCH …/replies/{reply_id}`: author only, 403 `suggestion.not_author`.
  - `DELETE …/replies/{reply_id}`: author or effective Owner, returns 204.
  - Every `{suggestion_id}` route runs `ensure_suggestion_in_doc` (404 `suggestion.not_found` if it belongs to another doc), and reply routes also check that the reply belongs to the suggestion.
  - `fn notify_suggestion_change(state: &AppState, doc_id: Uuid)` sends a fire-and-forget `pg_notify('doc_comments', {"doc_id": …, "topic": "suggestions"})` after every successful mutation. `comments_listener` already forwards payloads verbatim, so it needs no change.
  - Notification calls are added in Task 6. Leave a `// Task 6` marker only if Task 6 is not done in the same pass.

- [ ] **Step 1: Failing tests** in `suggestions_integration.rs`:
  - `viewer_cannot_create` (403 `acl.editor_required`), `editor_creates_and_lists`, `invalid_kind_combination_is_400`, `newline_in_proposed_text_is_400`, `too_long_is_422`, `bad_anchor_is_400` (covers both bad base64 and valid base64 that is not a RelativePosition);
  - `editor_cannot_reject_owner_can` (403 `acl.owner_required`, then 200);
  - `only_author_withdraws`, `reject_twice_is_not_pending`;
  - `cross_doc_suggestion_id_is_404`, for reject, withdraw and replies (Task 5 adds accept, and asserts the other doc's row stays `pending`);
  - `list_hides_resolved_unless_asked`;
  - `replies_are_listed_with_their_suggestion`, `reply_edit_author_only`, `reply_delete_author_or_owner`;
  - `mutation_sends_notify_with_topic`: `LISTEN doc_comments` on a `PgListener` from `state.pool` and assert a payload with `"topic":"suggestions"` arrives.
  - In `archive_integration.rs`, add the create, reject, withdraw and accept POSTs to `json_writes`, each expecting 409 `doc.archived`.
- [ ] **Step 2:** `cargo test -p knot-server --test suggestions_integration` fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test suggestions_integration --test archive_integration --test comments_integration` passes.

### Task 5: Accept route and the `applying` sweep

**Files:**
- Modify: `crates/knot-server/src/routes/api/suggestions.rs` (accept handler).
- Create: `crates/knot-server/src/suggestions_sweep.rs`. Add `pub mod suggestions_sweep;` in `lib.rs`, and spawn it in `main.rs` next to `notifications_sweep::spawn` (`main.rs:285-288`) when `state.rooms_v2` is `Some`.
- Test: `crates/knot-server/tests/suggestions_integration.rs`. Give accept tests rooms via the MemBus snippet from `contributors_integration.rs:97-107`, and seed content with `Event::ApplyUpdate` built from a `yrs::Doc` with a fixed client id, so anchors can be hand-built with `StickyIndex::from_id(..).encode_v1()` as in Task 1.

**Interfaces:**
- Consumes: Task 2 events, Task 3 `claim_for_accept` / `finish_accept` / `release` / `list_stuck_applying`.
- Produces:
  - `POST /api/docs/{id}/suggestions/{suggestion_id}/accept`, Owner. Body `{seen_text: string}`. Returns 200 `Suggestion` with status `accepted`.
    - Flow:
      1. `ensure_suggestion_in_doc`;
      2. get `state.rooms_v2` and `rooms.acquire(doc_id)` BEFORE claiming. A failure here is a plain 500 with the row untouched;
      3. `claim_for_accept(sid, ctx.user_id)`, which gives 409 `suggestion.not_pending` on `NotPending`;
      4. map the storage kind to `knot_crdt::SuggestionKind`;
      5. send `Event::ApplySuggestion { request, by_user: Some(suggestion.author_id), reply }`. If `tx.send` fails, `release` and return 500;
      6. await the reply under a `tokio::time::timeout` of 60 s, well under the sweep's 120 s threshold. A timeout or dropped channel gives 500 and leaves the row `applying` for the sweep;
      7. handle the reply:
         - `Ok(_)` or `AlreadyApplied` → `finish_accept(sid, ctx.user_id)`, 200. If that returns `NotPending`, re-read the row and return it as 200 when it is `accepted`; otherwise log an error and return 500;
         - `AnchorLost` → `release`, 409 `suggestion.anchor_lost`;
         - `Changed` → `release`, 409 `suggestion.changed`;
         - `Decode` → `release`, 400 `suggestion.invalid_anchor`;
         - `Persist` → 500 and leave the row `applying` for the sweep.
  - `pub fn spawn(state: AppState) -> tokio::task::JoinHandle<()>` runs every 60 s and skips the first tick (as `notifications_sweep.rs:102`).
  - `pub async fn run_once(state: &AppState, now: DateTime<Utc>) -> Result<SweepOutcome, SuggestionStoreError>` handles each row from `list_stuck_applying(now - 120 s)`. It acquires the room and sends `Event::SuggestionApplied`: `true` means `finish_accept(id, decided_by)`, `false` means `release_if_unchanged(id, row.updated_at)`. The residual window is documented in a module comment: a room event still queued after 120 s on another replica. The marker check plus `finish_accept` from `pending` keeps text and status consistent even then.
  - `pub struct SweepOutcome { pub finished: usize, pub released: usize }`.

- [ ] **Step 1: Failing tests.**
  - `owner_accept_applies_text`: accept, then `ExportState` shows the new text, `status == accepted` and `decided_by == owner`.
  - `accept_credits_suggester_in_contributors`: seed the content with `by_user` = the owner, and assert the suggester has no `doc_contributors` row before the accept and has one after.
  - `cross_doc_accept_is_404_and_leaves_row_pending`.
  - `room_unavailable_leaves_row_pending`: with `rooms_v2 = None`, accept gives 500 and the row is still `pending`.
  - `editor_cannot_accept` (403 `acl.owner_required`).
  - `stale_seen_text_is_409_changed_and_row_back_to_pending`.
  - `deleted_target_is_409_anchor_lost`.
  - `concurrent_accepts_one_wins`: two owners, `tokio::join!`. Exactly one 200 and one 409 `suggestion.not_pending`, and the text is applied once.
  - `accept_after_already_applied_finishes`: a manual `UPDATE suggestions SET status='pending'` after a successful apply, then accept again, gives 200 and the text is unchanged.
  - `sweep_finishes_applied_and_releases_unapplied`: one row applied in the doc but left `applying` via SQL, and one `applying` row never applied, both with `updated_at` 5 min ago. Expect `run_once` to give `finished == 1`, `released == 1`.
- [ ] **Step 2:** `cargo test -p knot-server --test suggestions_integration accept` fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test suggestions_integration` passes.

### Task 6: Server-side notifications

**Files:**
- Create: `crates/knot-server/src/suggestion_notifications.rs` (`pub(crate) mod` in `lib.rs`).
- Modify: `crates/knot-server/src/routes/api/suggestions.rs`. Call the emitters after a successful create, accept, reject and reply.
- Test: `crates/knot-server/tests/suggestions_integration.rs`. Read recipients with `SELECT user_id FROM notifications WHERE kind = $1 AND target_id = $2`, the pattern at `comments_integration.rs:1015-1060`.

**Interfaces:**
- Produces:
  ```rust
  /// Candidates = explicit owner grants on the doc or inheriting ancestors (GrantStore::list_inherited,
  /// role == Owner, principal "user:<uuid>") ∪ {doc.created_by}, kept only if
  /// acl.effective_role == Some(Owner). Only if THAT set is empty: workspace members with role Owner
  /// (list_members), same filter. Remove `author` last, so a creator suggesting on their own page
  /// notifies nobody rather than every workspace owner.
  pub(crate) async fn suggestion_owner_recipients(state: &AppState, workspace_id: Uuid, doc_id: Uuid, author: Uuid) -> Vec<Uuid>;
  pub(crate) async fn emit_suggestion_created(state: &AppState, workspace_id: Uuid, s: &Suggestion);
  /// To s.author_id when decided_by != author. data also carries "decision": "accepted" | "rejected".
  pub(crate) async fn emit_suggestion_decided(state: &AppState, workspace_id: Uuid, s: &Suggestion);
  #[derive(Clone, Copy, PartialEq, Eq)] pub(crate) enum ReplyWrite { Created, Edited }
  /// Mention (explicit ids ∪ extract_mentions(body), members only; on Created and Edited) and Reply
  /// (suggestion author + other repliers; Created only), each filtered by acl.effective_role(..).is_some(),
  /// like comments.rs:247-264. Called from reply POST (Created) and PATCH (Edited).
  pub(crate) async fn emit_suggestion_reply(state: &AppState, workspace_id: Uuid, s: &Suggestion, reply: &SuggestionReply, write: ReplyWrite, explicit_mentions: &[Uuid]);
  ```
- Excerpt: for `suggestion` and `suggestion_decided`, `proposed_text` for replace/insert and `original_text` for delete. For reply and mention rows, the reply body, as comments do. Truncate to 140 chars. Use `join_all` for the ACL filter, as `comments.rs:253-264` does. Errors are logged with `tracing::warn!` and never fail the request.

- [ ] **Step 1: Failing tests.**
  - `created_notifies_explicit_owner_and_creator_not_workspace_owners`: workspace owner W, plus a doc created by owner C, an editor E given an `owner` grant on the doc, and an editor S who suggests. Recipients are exactly {C, E}.
  - `created_falls_back_to_workspace_owners`: the doc's creator has been demoted to editor and there are no owner grants. Recipients are the workspace Owners.
  - `restricted_doc_recipients_must_still_read`.
  - `author_is_never_notified`.
  - `creator_suggesting_on_own_page_notifies_nobody`: the creator is the only explicit owner and no workspace Owner is notified.
  - `reply_edit_adding_mention_notifies`.
  - `decided_notifies_author_once` (`suggestion_decided`, `data.decision`).
  - `reply_mentions_and_reply_kinds_carry_suggestion_id`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server` passes. `cargo clippy --workspace --all-targets -- -D warnings` is clean.

### Task 7: Web — suggestion anchors, scope check, live state

**Files:**
- Modify: `web/src/features/comments/anchor.ts`. Export `getMapping` and type `ProsemirrorMapping`, with no behaviour change.
- Create: `web/src/features/suggestions/anchor.ts`, `web/src/features/suggestions/scope.ts`, `web/src/features/suggestions/liveState.ts`.
- Test: `web/src/features/suggestions/anchor.test.ts`, `scope.test.ts`, `liveState.test.ts` (patterns: `web/src/features/comments/anchor.test.ts:56-133`, `mountBoundEditor`, `placeSelection`).

**Interfaces:**
- Produces:
  ```ts
  // anchor.ts
  /** Start ties to the first selected char (assoc 0), end to the last (assoc -1). Insert: from===to, both assoc -1. */
  export function encodeSuggestionAnchors(editor: Editor, ydoc: Y.Doc, from: number, to: number): { start: string; end: string } | null;
  /** Like decodeAnchorRange, but WITHOUT y-tiptap's isMisresolvedTextPosition check. */
  export function decodeSuggestionRange(editor: Editor, ydoc: Y.Doc, startB64: string, endB64: string): { from: number; to: number } | null;
  // scope.ts
  export type SuggestionScope =
    | { ok: true; kind: "replace" | "insert"; from: number; to: number; text: string; contextBefore: string }
    | { ok: false; reason: "cross_block" | "inline_atom" | "empty_block" };
  export function suggestionScope(state: EditorState): SuggestionScope;
  export const SCOPE_TOOLTIP: Record<"cross_block" | "inline_atom" | "empty_block", string>; // copy from Global Constraints
  // liveState.ts
  /** `from` lets the sidebar order cards by document position (null when the range doesn't resolve). */
  export type SuggestionLive = { state: "same" | "changed" | "removed"; currentText: string; from: number | null };
  export type ClassifiableSuggestion = { kind: "replace" | "insert" | "delete"; position_y: string; position_y_end: string; original_text: string };
  export function classifySuggestion(editor: Editor, ydoc: Y.Doc, s: ClassifiableSuggestion): SuggestionLive;
  ```
- The encoder copies y-tiptap's `absolutePositionToRelativePosition` walk (`y-tiptap.js:1843-1909`) with an `assoc` parameter. Only the XmlText branch line changes, to `Y.createRelativePositionFromTypeIndex(n, pos, assoc)`. The decoder copies `relativePositionToAbsolutePosition` (`y-tiptap.js:1949-2013`) without the `isMisresolvedTextPosition` return. Both depend on two things that are not importable:
  - `lib0`'s `error.unexpectedCase()`: `lib0` is not a direct dependency, so use `throw new Error("unexpected case")`.
  - y-tiptap's module-private `createRelativePosition(type, item)` (`y-tiptap.js:1932-1941`): copy it locally.

  Everything else comes from `yjs` (`Y.findRootTypeKey`, `Y.createID`, `Y.RelativePosition`, `Y.createRelativePositionFromTypeIndex`, `Y.isParentOf`).
- **Known limitation (record it in the spec's Constraints too):** the end anchor is tied to the last selected character. Yjs places text typed after a deleted character to the right of its tombstone, so *retyping the end of the range* (e.g. selecting "charlie" and typing "charlotte") shrinks the range to what is left ("c"). The card then shows that remainder as "Now". This is accepted. Do not try to "fix" it in Task 1 or Task 7.
- `suggestionScope` rules:
  - `$from.parent !== $to.parent` or the parent is not a textblock → `cross_block`.
  - A `hard_break` in the range, or a `link` mark whose href matches `/^knot:\/\/(doc|user|time)\//` → `inline_atom`.
  - Collapsed in an empty textblock → `empty_block`.
  - Collapsed otherwise → `insert`; non-empty → `replace` (the server decides replace vs delete from the composer's text).
  - `contextBefore` is up to 40 chars before `from` in the same block.
- `classifySuggestion` rules:
  - Decode fails → `removed`.
  - For any kind, `doc.resolve(from).parent` is not a textblock → `removed`. When the paragraph holding an insert point is deleted, the decoder returns a doc-level position rather than null, so this check is needed.
  - For replace/delete: `from >= to`, or `from` and `to` not in the same textblock → `removed`.
  - Otherwise `currentText = textBetween(from, to)`, and the state is `same` if it equals `original_text`, else `changed`.
  - Insert with a point inside a textblock → `same` with `currentText ""`.

- [ ] **Step 1: Failing tests.**
  - `encodes_bytes_identical_to_rust_fixture`. The steps matter, because inserting with marks in one step lays out different clocks:
    1. `mountBoundEditor()`, then set `ydoc.clientID = 7` before any write;
    2. `editor.commands.insertContent("äb€cd")`;
    3. `editor.chain().setTextSelection({ from: 2, to: 4 }).toggleBold().run()`;
    4. `encodeSuggestionAnchors(editor, ydoc, 3, 5)`.

    Expect `start` = base64 of hex `00070400`, `end` = base64 of hex `00070541`, and `Y.encodeStateAsUpdate(ydoc)` equal to the Task 1 update hex.
  - `classify_insert_in_deleted_paragraph_is_removed`.
  - `edge_typing_stays_outside_range`: insert at the char before the start and after the end; the decoded `textBetween` is unchanged.
  - `decodes_start_anchor_on_first_character_of_page`.
  - `scope_refuses_cross_block`, `scope_refuses_hard_break`, `scope_refuses_internal_link_marks` (doc, user and time hrefs), `scope_allows_external_link`, `scope_empty_block_insert_refused`, `scope_caret_is_insert`.
  - `classify_same_changed_removed`.
- [ ] **Step 2:** `pnpm test src/features/suggestions` fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `pnpm test src/features/suggestions src/features/comments`, `pnpm tsc` and `pnpm lint` pass.

### Task 8: Web — API client, provider topic, notification kinds

**Files:**
- Create: `web/src/lib/suggestions.api.ts`
- Modify: `web/src/features/editor/KnotProvider.ts`. Add `topic?: "suggestions"` to `CommentChangeMsg`; no new event or MSG type.
- Modify: `web/src/lib/notifications.api.ts`. Add `"suggestion" | "suggestion_decided"` to `kind`.
- Modify: `web/src/features/notifications/notificationTarget.ts`:
  - `KIND_LABEL`: `suggestion: "suggested a change"`, `suggestion_decided: "reviewed your suggestion"`.
  - `ACTORLESS_LABEL`: `suggestion: "New suggestion on your page"`, `suggestion_decided: "Your suggestion was reviewed"`.
  - `targetPath`: when `data.suggestion_id` is a string, return `/doc/<id>?suggestion=<sid>`. `thread_id` keeps priority only when there is no `suggestion_id`.
- Test: `web/src/features/notifications/notificationTarget.test.ts` and `web/src/features/editor/KnotProvider.test.ts`.

**Interfaces:**
- Produces:
  ```ts
  export type SuggestionKind = "replace" | "insert" | "delete";
  export type SuggestionStatus = "pending" | "applying" | "accepted" | "rejected" | "withdrawn";
  export type SuggestionReply = { id: string; suggestion_id: string; author_id: string; body: string; created_at: string; updated_at: string };
  export type Suggestion = { id: string; doc_id: string; author_id: string; kind: SuggestionKind; position_y: string; position_y_end: string;
    original_text: string; context_before: string; proposed_text: string; reason: string | null; status: SuggestionStatus;
    decided_by: string | null; decided_at: string | null; created_at: string; updated_at: string };
  export type SuggestionWithReplies = Suggestion & { replies: SuggestionReply[] };
  export type NewSuggestionBody = Pick<Suggestion, "kind" | "position_y" | "position_y_end" | "original_text" | "context_before" | "proposed_text"> & { reason?: string };
  export const suggestionsApi: {
    list(docId: string, includeResolved?: boolean): Promise<ApiResult<SuggestionWithReplies[]>>;
    create(docId: string, body: NewSuggestionBody): Promise<ApiResult<Suggestion>>;
    accept(docId: string, sid: string, seenText: string): Promise<ApiResult<Suggestion>>;
    reject(docId: string, sid: string): Promise<ApiResult<Suggestion>>;
    withdraw(docId: string, sid: string): Promise<ApiResult<Suggestion>>;
    reply(docId: string, sid: string, body: string, mentions?: string[]): Promise<ApiResult<SuggestionReply>>;
    editReply(docId: string, sid: string, replyId: string, body: string): Promise<ApiResult<SuggestionReply>>;
    removeReply(docId: string, sid: string, replyId: string): Promise<ApiResult<void>>;
  };
  // The card UI offers create only, matching CommentThread, which has no edit/delete buttons either.
  // React Query keys: ["suggestions", docId, includeResolved]; invalidate prefix ["suggestions", docId].
  ```

- [ ] **Step 1: Failing tests.**
  - `targetPath_prefers_suggestion_id`, and label tests for both kinds.
  - `dispatches_topic_to_comments_listeners`: a MSG_COMMENTS frame with `{"doc_id":"d","topic":"suggestions"}` reaches listeners with `topic` set. The provider already forwards parsed JSON verbatim, so this test is expected to pass at once and only pins the behaviour.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `pnpm test src/features/notifications src/features/editor/KnotProvider.test.ts`, `pnpm tsc` and `pnpm lint` pass.

### Task 9: Web — `SuggestionsHighlightExtension`

**Files:**
- Create: `web/src/features/editor/SuggestionsHighlightExtension.ts`, modelled on `CommentsHighlightExtension.ts`.
- Modify: `web/src/features/editor/extensions.ts`. Register it after `CommentsHighlightExtension` and add the `onSelectSuggestion?: (id: string) => void` opt.
- Modify: `web/src/features/editor/schema.test.ts`. Add `"suggestionsHighlight"` to `REGISTERED_EXTENSIONS` in alphabetical order.
- Modify: `web/src/styles/prose.css`. Next to `.comment-highlight` (≈ line 394), add the blue styles: `rgb(37 99 235 / 0.12)` background with a 2px `rgb(37 99 235 / 0.55)` bottom border; active `0.24`/`0.9`; dark variants as for comments. `.suggestion-insert-marker` is a small solid `#2563EB` pill with white "+".
- Test: `web/src/features/editor/SuggestionsHighlightExtension.test.ts`.

**Interfaces:**
- Consumes: Task 7 `decodeSuggestionRange`.
- Produces:
  ```ts
  export type HighlightedSuggestion = { id: string; kind: SuggestionKind; position_y: string; position_y_end: string };
  export type DraftRange = { position_y: string; position_y_end: string };
  export const SuggestionsHighlightExtension: Extension<{ doc: Y.Doc; suggestions: HighlightedSuggestion[]; activeSuggestionId: string | null; draft: DraftRange | null; onSelect: (id: string) => void }>;
  // storage key "suggestionsHighlight" (add to the `declare module "@tiptap/core" { interface Storage }` block)
  export function setSuggestionsEditorRef(editor: Editor | null): void;
  export function refreshSuggestionHighlights(editor: Editor): void;
  ```
- Rendering:
  - A non-null `draft` (the range in the open composer, spec §7):
    - non-collapsed → `Decoration.inline` with class `suggestion-draft` (styled like a selection, `rgb(37 99 235 / 0.28)`) and no data attribute;
    - collapsed (an insert draft) → a widget `<span class="suggestion-insert-marker suggestion-draft-marker" contenteditable="false">`.
  - Replace/delete → `Decoration.inline` with class `suggestion-highlight` (+ `--active`) and `data-suggestion-id`. Skip collapsed ranges.
  - Insert → a `Decoration.widget` at the point: a `<span class="suggestion-insert-marker" data-suggestion-id contenteditable="false" aria-label="Suggested insertion">` with **no text node**. The "+" is drawn with CSS `::before { content: "+" }`, so it never shows up in `textBetween`, copy or e2e `docText`.
  - A click on either calls `storage.onSelect(id)`.

- [ ] **Step 1: Failing tests:** `renders_inline_highlight_for_replace`, `renders_widget_for_insert`, `renders_draft_range`, `active_class_follows_storage`, `click_calls_onSelect`, `collapsed_replace_is_skipped`. Call `setSuggestionsEditorRef(bound.editor)` in each test, because `mountBoundEditor` does not.
- [ ] **Step 2:** The tests fail, and `schema.test.ts` fails once the extension is registered but missing from the list.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `pnpm test src/features/editor` passes, and `pnpm tsc` and `pnpm lint` are clean.

### Task 10: Web — editor integration, UI store, deep link

**Files:**
- Modify: `web/src/stores/ui.ts`.
- Modify: `web/src/features/editor/KnotEditor.tsx`.
- Modify: `web/src/features/editor/EditorToolbar.tsx`.
  - Add the `onSuggest?: () => void` prop.
  - Add an optional `title?: string` prop to `Btn`, defaulting to `label`, so a disabled reason can show.
  - Add a `Btn testId="toolbar-suggest" label="Suggest an insertion"` with a lucide `MessageSquarePlus` icon. Check that the installed lucide-react exports it; otherwise use `MessageSquareDiff` or `PencilLine`.
  - The button is disabled when `onSuggest` is missing or `suggestionScope` is not an `ok` insert. When disabled because of scope, its `title` is `SCOPE_TOOLTIP[reason]`; `empty_block` can only appear here.
- Modify: `web/src/features/editor/addCommentPos.ts`, only if needed to widen `buttonWidth` for the two-button float.
- Modify: `web/src/features/docs/DocPage.tsx`. Add a `?suggestion=` deep link copied from the `?thread=` effect at `DocPage.tsx:153-171`, calling `openSuggestionLink` and then deleting the param.
- Test: `web/src/stores/ui.test.ts` (create if absent), `web/src/features/editor/EditorToolbar.suggest.test.tsx` (pattern: `EditorToolbar.trailing.test.tsx`).

**Interfaces:**
- Consumes: Tasks 7, 8, 9.
- Produces (in `useUi`):
  ```ts
  sidebarTab: "comments" | "suggestions"; setSidebarTab(t): void;
  pendingSuggestion: { kind: "replace" | "insert"; positionY: string; positionYEnd: string; originalText: string; contextBefore: string } | null;
  setPendingSuggestion(p): void; clearPendingSuggestion(): void;
  activeSuggestionId: string | null; setActiveSuggestionId(id: string | null): void;
  suggestionLink: { suggestionId: string } | null; openSuggestionLink(id: string): void; // opens sidebar, tab "suggestions", sets active
  suggestionLive: Record<string, SuggestionLive>; setSuggestionLive(m: Record<string, SuggestionLive>): void;
  ```
- `sidebarTab` is global, so every comment entry point must switch it back to `"comments"`: `openThreadLink`, the add-comment path (`handleAddComment` → `openCommentSidebar`), and the comment-highlight `onSelectComment`. Give `openCommentSidebar` an optional `tab?: "comments" | "suggestions"` parameter, defaulting to `"comments"`, and have the existing callers rely on the default. The `open-comments` header and dock buttons keep the last tab.
- KnotEditor changes:
  1. Query `["suggestions", docId, false]`. Push the pending ones, `activeSuggestionId`, and `draft` (from `pendingSuggestion`, null when there is none) into `editor.extensionStorage.suggestionsHighlight`. Use an effect guarded with `if (!editor || editor.isDestroyed) return;` like `KnotEditor.tsx:403`.
  2. Call `setSuggestionsEditorRef` in the same effect as `setEditorRef`.
  3. In the `pair.doc.on("update")` handler, also run `refreshSuggestionHighlights` and recompute `setSuggestionLive` with `classifySuggestion` for every pending suggestion. Do the same when the query data changes.
  4. In `provider.on("comments")`, invalidate `["suggestions", docId]` when `msg.topic === "suggestions"`, else `["comments", docId]` as today.
  5. A "Suggest change" button `suggest-change-float` sits next to `add-comment-float`. It shows for `canComment` (view and edit mode). It is disabled, with a `title` from `SCOPE_TOOLTIP`, when `suggestionScope` is not ok. Clicking it encodes anchors and calls `setPendingSuggestion` + `openCommentSidebar()` + `setSidebarTab("suggestions")`.
  6. Toolbar `onSuggest` does the same for a collapsed caret, and only renders in edit mode, because the toolbar itself only exists in edit mode.
  7. A highlight click (`onSelectSuggestion`) calls `openSuggestionLink(id)`.

- [ ] **Step 1: Failing tests:** `openSuggestionLink_opens_sidebar_on_suggestions_tab`, `openThreadLink_switches_back_to_comments_tab`, `openCommentSidebar_defaults_to_comments_tab`, `toolbar_suggest_disabled_without_caret_insert_scope` (asserts the `title` too), `toolbar_suggest_calls_onSuggest`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `pnpm test`, `pnpm tsc` and `pnpm lint` pass. The whole web suite must stay green, since KnotEditor is shared.

### Task 11: Web — sidebar tabs, suggestion cards, composer

**Files:**
- Modify: `web/src/features/comments/CommentSidebar.tsx`. Header tabs `sidebar-tab-comments` ("Comments n") and `sidebar-tab-suggestions` ("Suggestions n", pending count) replace the `<h2>`, with `aria-pressed` on the active one. The body shows the existing comments UI or `<SuggestionsPanel docId>`. The existing `comment-show-resolved` checkbox only shows on the comments tab.
- Create: `web/src/features/suggestions/SuggestionsPanel.tsx`, `SuggestionCard.tsx`, `SuggestionComposer.tsx`.
- Test: `web/src/features/suggestions/SuggestionCard.test.tsx`, `SuggestionComposer.test.tsx`, `SuggestionsPanel.test.tsx` (pattern: `CommentComposer.test.tsx`, which mocks API modules with `vi.mock`, plus `QueryClientProvider` with `retry: false`).

**Interfaces:**
- Consumes: Task 8 `suggestionsApi`, Task 10 store slots, `useEffectiveRole(docId).doc`, `useSession()` user id, `CommentComposer` for replies, `["members"]` query for author names.
- Behaviour (copy from Global Constraints):
  - **SuggestionComposer.** Shown at the top while `pendingSuggestion` is set.
    - Fields: "Current text" (struck through; hidden for insert), a **single-line** "Replace with" / "Insert" `<input>` prefilled with `originalText`, and "Why? (optional)". Newlines from paste or change become spaces.
    - Submit derives kind as `insert` if `pendingSuggestion.kind === "insert"`; `delete` if the replacement is empty; else `replace`. Submit is disabled when the replacement equals the original.
    - On success: `clearPendingSuggestion`, then invalidate `["suggestions", docId]`. Cancel also calls `clearPendingSuggestion`, which removes the draft highlight.
  - **SuggestionCard**, `data-state` from `suggestionLive[sid].state` for pending ones. When `suggestionLive[sid]` is missing (the markdown view, or before the first classification), use `data-state="unknown"`: show "Now" / "Suggested" from the stored texts and disable Accept.
    - `same`: rows "Now" / "Suggested" (delete: "Remove"; insert: "Add" plus `…<context_before>`).
    - `changed`: "When suggested" (`original_text`), "Now" (`currentText`), "Suggested", plus the note "Changed since it was suggested".
    - `removed`: the removed note, with Accept disabled.
    - Owner: Accept calls `suggestionsApi.accept(docId, sid, live.currentText)`, using `""` for insert. Reject.
    - Non-owner: `suggestion-waiting-<sid>` with "Waiting for an owner".
    - Author: Withdraw.
    - Replies via `CommentComposer`, with testids `suggestion-reply-input-<sid>` / `suggestion-reply-submit-<sid>`.
    - Focusing a card sets `activeSuggestionId` and scrolls `[data-suggestion-id="<sid>"]` into view.
  - **Accept results:**
    - 409 `suggestion.changed` → show the "changed while you were looking" note in `suggestion-note-<sid>`, invalidate the query, and never retry by itself.
    - 409 `suggestion.anchor_lost` → invalidate; the card re-renders as removed.
    - 409 `suggestion.not_pending` → invalidate.
  - **SuggestionsPanel:**
    - Pending cards in document order, by `suggestionLive[sid].from`. Null or missing goes last, ordered by `created_at`.
    - A collapsed "Resolved" section behind `suggestion-show-resolved`, which loads `include_resolved=true` and shows status label, decider and time.
    - Viewers see cards read-only.
    - `suggestionLink` opens the resolved section if the linked suggestion is not pending, as `CommentSidebar.tsx:88-95` does for threads.

- [ ] **Step 1: Failing tests:**
  - `composer_derives_delete_when_replacement_empty`, `composer_submit_disabled_when_unchanged`, `composer_turns_pasted_newlines_into_spaces`;
  - `card_without_live_state_is_unknown_and_accept_disabled`;
  - `card_same_changed_removed_rows`, `owner_sees_accept_reject_editor_sees_waiting`, `author_sees_withdraw`;
  - `accept_sends_current_text_as_seen_text`, `accept_409_changed_shows_note_and_refetches`;
  - `panel_tabs_show_pending_count`, `resolved_section_lists_decided`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `pnpm test`, `pnpm tsc` and `pnpm lint` pass.

### Task 12: End-to-end flows, changelog

**Files:**
- Create: `e2e/flows/suggestions.spec.ts`. Copy helpers `signIn(browser, email, password)` and `invite(page, email, role, password)` from `e2e/flows/contributors.spec.ts:10-30`, with `role` widened to include `"owner"`. Copy `newDoc(page)` from `comment-anchors.spec.ts:48-57`, `writeHeaders` from `e2e/flows/tree-helpers.ts:38-44`, and use `docText` from `e2e/support/docText.ts`.
- Modify: `CHANGELOG.md`. Add an `## [Unreleased]` entry under `### Added` describing change suggestions, the migration, and the two notification kinds.
- Modify: `.claude/skills/knot-api/SKILL.md`. Add the suggestion endpoints in the same style as the comments section.

**Interfaces:**
- Consumes: every testid in Global Constraints.
- Each `test()` does its own `reset()` and `/setup` (`test.beforeEach(reset)`), uses `test.setTimeout(120_000)`, and closes its contexts in `finally`.
- Text is selected by typing and then `Shift+ArrowLeft`. Never use Home/Cmd chords. Viewers and view-mode tests can't type, so they select a word by `dblclick` (as `e2e/flows/doc-dock.spec.ts:194` does) on text an editor typed. Text is compared via `expect.poll(() => editor.evaluate(docText))`.
- The inbox badge polls every 30 s. After the triggering action, `reload()` the recipient's page (or `goto("/")`) before asserting on `inbox-badge`.
- Wait for `status-dot` `data-status="connected"` on every page before acting.

- [ ] **Step 1: Write the specs.**
  - `editor suggests, owner sees it live and accepts`: the owner (from /setup) and an invited editor are both on the doc. The editor types "alpha bravo charlie", selects "charlie", clicks `suggest-change-float`, types "delta" into `suggestion-composer-replace` and submits. The owner, with the sidebar open on `sidebar-tab-suggestions`, sees `suggestion-card-*` within 10 s with no reload, plus a `.suggestion-highlight` reading "charlie". The owner accepts. Both editors poll `docText` to "alpha bravo delta", and the card moves to Resolved.
  - `direct edit makes the card "changed", accept replaces current text`: after the suggestion, the editor edits *inside* the word, keeping its last character alive. They put the caret after "char" with ArrowLeft and type "X", giving "charXlie" (see the Task 7 limitation). The owner's card gets `data-state="changed"` and shows "When suggested" "charlie" / "Now" "charXlie". Accept leads to "alpha bravo delta".
  - `text changes while owner looks → 409 and refresh`:
    1. The owner's card renders as `same`.
    2. Hold the owner's accept with `owner.route('**/api/docs/*/suggestions/*/accept', …)` until the editor's edit to the target text has been confirmed on the server (poll `GET /api/docs/<id>/markdown` from the editor's `request`).
    3. Release the request. Assert the response is 409 with `error.code === "suggestion.changed"` (via `waitForResponse`) and that `suggestion-note-<sid>` shows the copy.
    4. A second Accept gives 200.
  - `deleted target disables Accept`: the editor deletes the whole sentence. The card gets `data-state="removed"` and `suggestion-accept-<sid>` is disabled.
  - `reject and withdraw`.
  - `viewer sees suggestions read-only`: a viewer selects text; `suggest-change-float` has count 0, the card shows `suggestion-waiting-<sid>`, and accept/reject/withdraw have count 0.
  - `view mode: replace works, insertion button absent`: click `toggle-edit-mode` to aria-pressed=false. The float works, and `toolbar-suggest` has count 0.
  - `inbox deep link opens the suggestion`:
    1. The owner is on `/` when the editor suggests.
    2. The owner sees `inbox-badge` "1", opens `sidebar-inbox`, and clicks the `notification-row` with `data-kind="suggestion"`.
    3. They land on `/doc/<id>` with the Suggestions tab active and the card `data-active="true"`, and the URL no longer has `suggestion=`.
    4. After the owner accepts, the editor's inbox shows `data-kind="suggestion_decided"`.
- [ ] **Step 2: Run them.** `cd e2e && pnpm playwright test flows/suggestions.spec.ts`. Because this branch adds a migration, follow the dev-stack recipe:
  - start `./target/debug/knot-server` yourself against a per-run database `knot_e2e_suggestions` (on :5434), so Playwright reuses :3000;
  - run a :3001→:3000 forwarder, Dex on :5556, and the docker shim for `reset()`;
  - drop the database afterwards.
  - Expected: all pass. `tree-reorder.spec.ts` "nest" is a known pre-existing failure.
- [ ] **Step 3: Fix any failures** in the owning task's code, rerunning that task's unit tests too.
- [ ] **Step 4: Full gates.** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, `cd web && pnpm test && pnpm tsc && pnpm lint`, and the comments e2e specs (`comments-realtime`, `comment-anchors`, `notifications`) as regression checks.
