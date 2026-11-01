# Restricted documents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let an owner mark a document "restricted" so it and its subpages are visible only to explicitly granted people. Workspace roles, including the workspace owner's, stop applying inside.

**Architecture:** A `documents.restricted` flag. A pure step function in `knot-docs` evaluates access, and it is used in two ways: per doc (`acl::resolve`, which folds the ancestor chain) and batched (`acl::compute_access`, which folds the whole tree and is cached per `(ws, user)` as `Visibility`). Every listing filters through `Visibility`. Every doc-id route already goes through `resolve`. Invalidation reuses the existing `acl_invalidations` outbox, and the subtree walk plus room revoke now covers board rooms too.

**Tech Stack:** Rust (axum, sqlx/Postgres, moka, tokio), React + valibot + TanStack Query, vitest, Playwright.

**Spec:** `docs/superpowers/specs/2026-10-30-restricted-documents-design.md`. Read it first. Its Decisions table is normative.

## Global Constraints

- **Never `git commit` or `git push`.** Leave your task's changes **unstaged** in the working tree. The orchestrator reviews `git diff` (unstaged = this task) and stages after review.
- No-access answers stay uniform. A doc-id route answers 403 `acl.no_grant`, identical to an unknown uuid. A parent in a request body answers 404 `doc.not_found`.
- New error codes, exactly: `grant.last_owner` (409), `grant.not_member` (422), `acl.restricted_move` (403), `doc.hidden_descendants` (409), `doc.restore_restricted_parent_archived` (409), `share.restricted` (409).
- Audit actions, exactly: `doc.restrict`, `doc.unrestrict`, `admin.grant`.
- Invalidation reasons, exactly: `restrict`, `unrestrict`, `member-remove`.
- Migration file: `migrations/20261030120000_restricted_docs.sql`. Migrations are forward-only. After editing a migration, run `touch crates/knot-test-support/src/lib.rs`, because `sqlx::migrate!` embeds the file at compile time.
- DB tests run against the throwaway Postgres on `127.0.0.1:5432` (the `knot_test_support::fresh_db()` default). Use `cargo test` (nextest is not installed).
- Web tests: `cd web && NODE_OPTIONS=--no-experimental-webstorage ./node_modules/.bin/vitest run <files>`. Typecheck: `cd web && ./node_modules/.bin/tsc --noEmit`. Lint: `cd web && ./node_modules/.bin/eslint src --max-warnings 0`.
- Rust gates for every backend task: `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean.
- Match surrounding style: doc comments on public items, descriptive variable names (user rule), and no new crates.
- Valibot (not zod) for web schemas. New fields use `v.fallback(..., default)`.

## Review Focus

1. **A user granted only a deep child below a hidden boundary.** They see it under "Shared with you", can open it, and never receive the boundary's id or title from `/api/docs`, `GET /api/docs/{id}` (`parent_id`, `archive_root_*`, `restriction_root_id`), search `parent_id`, or `/access`. Owned by Tasks 4, 5, 9. Each adds a `deep_grantee_*` test.
2. **Nested restriction.** An outer grantee with inherit=true must NOT see an inner restricted doc. Owned by Task 1 (rule table) and Task 5 (list test `nested_boundary_hides_inner_from_outer_grantee`).
3. **Dragging a "Shared with you" or restricted-subtree page to the root** declassifies it. The server refuses unless the actor is Owner. The UI forbids dragging `parent_hidden` rows. Owned by Task 6 (`editor_cannot_move_out_of_restricted_subtree`) and Task 16.
4. **Restricting while another member has the page or its board open.** That member is kicked immediately, with no 60 s cache window, and their reconnect is refused. Owned by Task 3 (generation guard) and Task 13 (`restrict_closes_doc_and_board_sockets`).
5. **Lock-out.** Restricting grants the actor owner. Removing the last owner grant returns 409. If the sole owner is removed from the workspace, the CLI `admin grant` recovers access. Owned by Task 4 (`last_owner_grant_cannot_be_removed`), Task 12, and Task 14.

---

### Task 1: Data model and the pure access rule

**Files:**
- Create: `migrations/20261030120000_restricted_docs.sql`. Use the spec §1 SQL verbatim.
- Create: `crates/knot-docs/src/rule.rs`. Export it from `crates/knot-docs/src/lib.rs`.
- Modify: `crates/knot-storage/src/doc_store.rs`. Add `restricted: bool` to `Document`, then `DocRow`, `COLS` and `doc_from_row`. Fix every construction site the compiler flags, including `crates/knot-storage/src/duplicate.rs` and tests.
- Modify: `crates/knot-docs/src/acl.rs`. Add `pub restriction_root: Option<Uuid>` to `DocAccess`, keep it `Copy`, and fix the constructors in the existing tests.

**Interfaces:**
- Produces (in `knot_docs::rule`):
  ```rust
  pub struct AclNode { pub id: Uuid, pub parent_id: Option<Uuid>, pub restricted: bool, pub archived: bool }
  pub struct PrincipalGrant { pub doc_id: Uuid, pub role: WorkspaceRole, pub inherit: bool }
  /// Access to the LAST node of `path` (root first). None = no access.
  pub fn access_along_path(path: &[AclNode], grants: &[PrincipalGrant], workspace_role: Option<WorkspaceRole>) -> Option<DocAccess>
  /// Access for every node of a workspace tree; nodes with no access are absent.
  pub fn compute_access(nodes: &[AclNode], grants: &[PrincipalGrant], workspace_role: Option<WorkspaceRole>) -> HashMap<Uuid, DocAccess>
  ```
- Rule: implement the step function from spec §2 verbatim.
  - `restriction_root` is the id of the nearest restricted ancestor-or-self.
  - `workspace_role == None` gives no access for every node.
  - In `compute_access`, a node whose `parent_id` is not among `nodes` is a root. Nodes on a parent cycle get no access (fail closed). Use an explicit stack, not recursion.
  - Both functions share one private `fn step(...)`.

- [ ] **Step 1: Write failing table tests in `rule.rs` `#[cfg(test)]`.** Use the names below, each with explicit `assert_eq!` on `role` and `restriction_root`.
  - `unrestricted_uses_max_of_workspace_role_and_grants`: Viewer member with an Editor grant on the root. The child gets Editor, with `restriction_root == None`.
  - `restricted_boundary_ignores_workspace_role`: Owner member, root restricted, no grants. Result is `None`.
  - `grant_on_boundary_inherits_down`: Owner grant inherit=true on boundary B. Child of B gets Owner, with `restriction_root == Some(B)`.
  - `grant_above_boundary_does_not_cross`: Editor grant inherit=true on root R, B a restricted child of R. B gets `None`; R gets Editor.
  - `nested_boundary_resets_inheritance`: Owner grant inherit=true on outer boundary O. Inner restricted I under O gets `None`; a non-restricted child of O gets Owner.
  - `non_inheriting_grant_covers_only_its_doc`: Editor grant inherit=false on B (restricted). B gets Editor; child gets `None`.
  - `non_member_gets_nothing_even_with_grant`: `workspace_role = None`, Owner grant. Result is `None`.
  - `archived_flag_is_carried`: an archived node reports `archived: true`.
  - `orphan_is_treated_as_root` and `cycle_fails_closed`, for `compute_access`.
  - `compute_access_matches_access_along_path`: a deterministic pseudo-random generator (LCG seeded by loop index, no new crate) over 500 trees of up to 30 nodes, with random restricted flags and grants. For every node, `compute_access[id] == access_along_path(path_to(id))`.
- [ ] **Step 2:** `cargo test -p knot-docs rule` fails to compile or fails.
- [ ] **Step 3:** Implement the migration, the storage field, the `DocAccess` field and `rule.rs`.
- [ ] **Step 4:** `touch crates/knot-test-support/src/lib.rs && cargo test -p knot-docs -p knot-storage` passes, including all pre-existing tests.

### Task 2: `resolve` and the batched loaders on the new rule

**Files:**
- Modify: `crates/knot-storage/src/grant_store.rs`. Add the trait methods below and their Pg impls.
- Modify: `crates/knot-storage/src/doc_store.rs`. Add `acl_nodes` and `has_restricted` to the `DocStore` trait and the Pg impl.
- Modify: `crates/knot-docs/src/acl.rs`. Rewrite `resolve` on `chain_for_principal` and `rule::access_along_path`, and fix the stale "(doc_id, user_id)" cache-key comment.
- Test: `crates/knot-storage/tests/grants.rs`, `crates/knot-docs/src/acl.rs` tests.

**Interfaces:**
- Produces:
  ```rust
  // GrantStore
  /// Ancestor chain of `doc_id` ROOT FIRST (doc last), plus `principal`'s grants on it.
  /// Empty chain if the doc is missing or not in `workspace_id`.
  async fn chain_for_principal(&self, workspace_id: Uuid, doc_id: Uuid, principal: &str)
      -> Result<(Vec<knot_storage::ChainNode>, Vec<Grant>), GrantStoreError>;
  /// Every grant of `principal` on docs of `workspace_id`.
  async fn list_for_principal(&self, workspace_id: Uuid, principal: &str) -> Result<Vec<Grant>, GrantStoreError>;
  // knot_storage::ChainNode { id, parent_id: Option<Uuid>, restricted: bool, archived: bool }
  // DocStore
  /// Every doc of the workspace (archived and templates included), for ACL folding.
  async fn acl_nodes(&self, workspace_id: Uuid) -> Result<Vec<ChainNode>, DocStoreError>;
  async fn has_restricted(&self, workspace_id: Uuid) -> Result<bool, DocStoreError>;
  ```
- The `chain_for_principal` SQL reuses the `list_inherited` recursive CTE: workspace filter on each step, `c.depth < 10000`. It selects `restricted, archived_at IS NOT NULL` and orders by depth DESC so the result is root first.
- `resolve` keeps its signature and the tenancy guard (a missing chain gives `Ok(None)`). It maps `ChainNode` to `AclNode` and `Grant` to `PrincipalGrant`. `list_inherited` stays, because tests and `/access` use it.

- [ ] **Step 1: Failing tests.**
  - In `acl.rs`, add `restricted_doc_hides_from_workspace_owner`: the member is an Owner and the doc's restricted flag is set via SQL `UPDATE documents SET restricted = true`. Expect `resolve == None`.
  - In `acl.rs`, add `grant_inside_restricted_subtree_gives_access`: Viewer grant on the boundary. Expect `role == Viewer` and `restriction_root == Some(boundary)`.
  - In `acl.rs`, add `removed_member_with_grant_gets_none`: remove the member via `WorkspaceStore::remove_member` and keep the grant. Expect `None`.
  - In `grants.rs`, add `chain_for_principal_is_root_first_and_filters_principal` and `list_for_principal_scoped_to_workspace`.
  - Every existing `acl.rs` test must still pass. That includes `non_member_with_no_grant_is_none`, `cross_workspace_doc_is_denied` and `ancestor_inherit_propagates`.
- [ ] **Step 2:** `cargo test -p knot-docs -p knot-storage` shows the new tests failing.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-docs -p knot-storage` passes.

### Task 3: AclCache visibility, generation guard, listener hardening

**Files:**
- Modify: `crates/knot-docs/src/cache.rs`
- Modify: `crates/knot-docs/src/listener.rs`
- Test: `crates/knot-docs/src/cache.rs` tests, `crates/knot-docs/tests/listener_integration.rs`

**Interfaces:**
- Produces (in `knot_docs`):
  ```rust
  pub struct Visibility { /* HashMap<Uuid, DocAccess> */ }
  impl Visibility {
      /// Raw access (archived included), like AclCache::access.
      pub fn access(&self, doc_id: Uuid) -> Option<DocAccess>;
      /// Readable = access with can_read(); same as AclCache::effective_role.is_some().
      pub fn readable(&self, doc_id: Uuid) -> bool;
      pub fn effective_role(&self, doc_id: Uuid) -> Option<WorkspaceRole>;
      /// Ids of every doc in the workspace that is NOT readable (for SQL deny-lists).
      pub fn hidden_ids(&self) -> Vec<Uuid>;
  }
  impl AclCache {
      pub async fn visibility(&self, workspace_id: Uuid, user_id: Uuid) -> Result<Arc<Visibility>, ResolveError>;
  }
  ```
- `visibility` makes 3 queries: `acl_nodes`, `list_for_principal`, `get_member_role`. It folds with `compute_access`. Its moka cache is keyed `(ws, user)`, with a 60 s TTL and 10k capacity. No special fast path is needed; the fold is O(n).
- `evict_doc` and `evict_all` also `invalidate_all()` the visibility cache.
- Generation guard: an `Arc<AtomicU64>` bumped (SeqCst) in `evict_doc` and `evict_all`. `access()` snapshots it before `resolve` and inserts only if it is unchanged. Otherwise it retries, at most 3 times, then returns the last value without caching. Apply the same guard to `visibility`.
- Listener: call `cache.evict_all()` right after `listener.listen(CHANNEL)` succeeds. If `descendant_ids` errors, call `cache.evict_all()` and keep the warn log.

- [ ] **Step 1: Failing tests.**
  - In `cache.rs`, add `visibility_matches_effective_role_for_every_doc`: a tree of 6 docs, one restricted boundary, one grant. For each doc, `vis.effective_role(id) == cache.effective_role(...)`.
  - In `cache.rs`, add `visibility_is_invalidated_by_evict_doc`: restrict via SQL, then `evict_doc` and `run_pending_tasks`. The new `visibility` hides the doc.
  - In `cache.rs`, add `stale_resolve_is_not_cached_after_concurrent_eviction`. Call a `pub(crate) fn generation()` accessor, mutate the DB, bump with `evict_doc` between snapshot and insert by using a test-only hook `#[cfg(test)] pub(crate) async fn access_with_hook(...)`, then assert that the next `access` reflects the DB.
  - In `listener_integration.rs`, add `restrict_toggle_evicts_descendants_and_calls_on_invalidate`. Record ids with a `Mutex<Vec<Uuid>>` closure, run `UPDATE documents SET restricted` plus `invalidations::record_in_tx`, and assert that both parent and child ids were recorded.
- [ ] **Step 2:** Run `cargo test -p knot-docs`. The new tests should fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test -p knot-docs`. Everything should pass.

### Task 4: Restrict toggle, grant guards, doc metadata, `/access`

**Files:**
- Modify: `crates/knot-storage/src/doc_store.rs`. Add `set_restricted`.
- Modify: `crates/knot-storage/src/grant_store.rs`. Add the last-owner guard and `GrantStoreError::LastOwner`.
- Modify: `crates/knot-server/src/routes/api/docs.rs`. Add the toggle route and the new `get_one` fields.
- Modify: `crates/knot-server/src/routes/api/grants.rs`. Map `LastOwner` and `not_member`, and add the `/access` handler.
- Test: create `crates/knot-server/tests/restricted_integration.rs`. Copy the `login_state`/`invite_user`/`login_as`/`create_doc` helpers from `grants_integration.rs` into a `mod common`-free local section.

**Interfaces:**
- Produces:
  ```rust
  // DocStore
  /// Sets the flag. When `restricted`, also upserts `user:<actor>` owner inherit=true on the doc
  /// and revokes active share_tokens of the doc and all descendants. Audits doc.restrict /
  /// doc.unrestrict, records invalidation "restrict"/"unrestrict". One transaction. Idempotent.
  async fn set_restricted(&self, workspace_id: Uuid, doc_id: Uuid, actor: Uuid, restricted: bool) -> Result<Document, DocStoreError>;
  // GrantStoreError::LastOwner  — put (downgrade) or delete would leave a doc with restricted=true
  // with zero role='owner' grants. Checked inside the same tx with SELECT ... FOR UPDATE on the doc row.
  ```
- HTTP: `PUT /api/docs/{id}/restricted` with body `{"restricted": bool}`. Mount it in `doc_id_routes`. It needs `EffectiveDocRole == Owner`, otherwise 403 `acl.owner_required`. After commit, call `archive::evict_local(state, &[doc] ++ descendant_ids)`. It returns `DocResponse`.
- `DocResponse` (shared by every route) gains `restricted: bool`. `get_one`'s response additionally gains:
  - `within_restricted: bool`
  - `restriction_root_id: Option<String>`, only when that root is readable by the caller
  - `parent_id` set to null when the parent is not readable
  - `archive_root_id`/`archive_root_title` set to null when the root is not readable
- Grants: map `LastOwner` to 409 `grant.last_owner`. On `put`, a principal that is not a current workspace member gets 422 `grant.not_member`.
- `GET /api/docs/{id}/access` (any reader) returns the JSON shape from spec §4.
  - Build `people` from `list_inherited`, truncated to depth ≤ the boundary depth when within a restricted subtree. A doc's own grants are always included.
  - `source_title` and `restriction_root_title` are given only when readable.
  - Order: the doc's own grants first, then by depth.

- [ ] **Step 1: Failing integration tests in `restricted_integration.rs`.**
  - `owner_restricts_and_member_loses_access`: Alice is the workspace owner and the creator. Bob is a workspace editor. Alice runs PUT restricted true and gets 200 with `restricted:true`. Bob's GET `/api/docs/{id}` returns 403 whose body is byte-identical to GET of a random uuid. Alice's GET returns `within_restricted:true` and `restriction_root_id == id`.
  - `restricting_grants_actor_owner`: after the toggle, Alice's GET grants contains `user:<alice>` owner inherit=true.
  - `editor_cannot_restrict`: an Editor grant holder gets 403 `acl.owner_required`.
  - `last_owner_grant_cannot_be_removed`: DELETE of Alice's own grant gets 409 `grant.last_owner`. After adding Bob as owner, the DELETE gets 204.
  - `grant_to_non_member_is_422`.
  - `restrict_revokes_share_links_in_subtree`: a share on a child exists; after restricting the parent, `GET /p/{token}` returns 410.
  - `deep_grantee_does_not_learn_boundary`: B is restricted, C is B's child, and Bob is granted Viewer on C only. Bob's GET C has `parent_id: null` and `restriction_root_id: null`. Bob's `/access` on C has `restriction_root_id: null` and `restriction_root_title: null`. B's id string appears in neither response body.
  - `access_lists_people_up_to_boundary`: a grant above the boundary is not listed; the boundary's grants are listed with `source_doc_id`.
  - `unrestrict_restores_workspace_access`.
- [ ] **Step 2:** `cargo test -p knot-server --test restricted_integration` fails.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test restricted_integration --test grants_integration --test docs_integration` passes.

### Task 5: Filtered sidebar list and templates

**Files:**
- Modify: `crates/knot-server/src/routes/api/docs.rs`, specifically `list` and `list_templates`.
- Test: `restricted_integration.rs`, `templates_integration.rs`.

**Interfaces:**
- Consumes: `AclCache::visibility` (Task 3).
- `GET /api/docs` keeps only docs with `vis.readable(id)`. Each item is `DocResponse` plus:
  - `effective_role: "owner"|"editor"|"viewer"`
  - `within_restricted: bool`
  - `parent_hidden: bool`, where true means the parent exists but is not readable; `parent_id` is then `null`
- Introduce `struct ListedDoc { #[serde(flatten)] doc: DocResponse, effective_role, within_restricted, parent_hidden }`.
- `GET /api/workspace/templates` is filtered by `vis.readable`.
- A visibility error answers 500. Fail closed; never return an unfiltered list.

- [ ] **Step 1: Failing tests.**
  - `list_hides_restricted_subtree_from_non_grantee`: Bob's list has neither the boundary nor its child.
  - `list_hoists_deep_grantee_doc_with_parent_hidden`: Bob holds a grant on child C only. C appears with `parent_id:null`, `parent_hidden:true` and `within_restricted:true`, and the boundary's id appears nowhere in the response body (string search).
  - `nested_boundary_hides_inner_from_outer_grantee`.
  - `list_reports_per_doc_effective_role`: a workspace Viewer with an Editor grant gets `effective_role:"editor"` on that doc.
  - `restricted_template_not_listed_for_non_grantee`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test restricted_integration --test templates_integration --test docs_integration` passes.

### Task 6: Create, from-template, import, and move rules

**Files:**
- Create: `crates/knot-server/src/restricted.rs` with the shared helpers below, registered in `lib.rs`.
- Modify: `crates/knot-server/src/routes/api/docs.rs`, specifically `create`, `create_from_template_inline` and `move_doc`.
- Modify: `crates/knot-server/src/routes/api/export_import.rs`, the import parent path.
- Modify: `crates/knot-storage/src/doc_store.rs` and `placement.rs` for the boundary check in `move_placed_once`.
- Test: `restricted_integration.rs`, `cross_workspace_integration.rs` (must stay green).

**Interfaces:**
- Produces (in `crate::restricted`):
  ```rust
  /// Parent from a request body: None if top-level. Err(Response) is 404 doc.not_found when
  /// missing/foreign/unreadable, 403 acl.editor_required when viewer, 409 doc.archived via refuse_if_archived.
  pub async fn require_editable_parent(state: &AppState, ctx: &AuthContext, parent_id: Uuid) -> Result<(), Response>;
  /// After creating `doc_id`: if the creator cannot edit it, upsert `user:<creator>` owner inherit=true.
  pub async fn ensure_creator_can_edit(state: &AppState, workspace_id: Uuid, doc_id: Uuid, creator: Uuid) -> Result<(), Response>;
  ```
- Storage helper in `doc_store.rs`: `pub(crate) async fn restriction_root_of(executor: impl sqlx::PgExecutor<'_>, workspace_id: Uuid, doc_id: Option<Uuid>) -> Result<Option<Uuid>, sqlx::Error>`. It returns the nearest restricted ancestor-or-self, or `None` for `None` or an unrestricted chain. It uses the recursive CTE with the workspace filter and the `depth < 10000` cap. It is exposed on the trait as `async fn restriction_root(&self, workspace_id: Uuid, doc_id: Uuid) -> Result<Option<Uuid>, DocStoreError>`. Tasks 8 and 11 use both forms.
- `ChainNode` (Task 2) lives in `grant_store.rs` and is re-exported from `knot_storage`.
- `DocStore::move_placed` gains a trailing `allow_boundary_cross: bool`. Under the move lock, `move_placed_once` computes `restriction_root_of` for the old parent and for the new parent. If they differ and `!allow_boundary_cross`, it returns `DocStoreError::RestrictionBoundary`. Update every caller, including tests and legacy `move_to` (pass `true`; no server caller).
- `move_doc`:
  - With a target parent, call `require_editable_parent`. For a top-level target, require `ctx.role` Editor+.
  - Pass `allow_boundary_cross = (EffectiveDocRole == Owner)`.
  - Map `RestrictionBoundary` to 403 `acl.restricted_move`.
  - Before calling storage, map `after_id`/`before_id` anchors not readable by the caller to 422 `doc.bad_anchor`, using `visibility`.
- `create`, `from-template` and `import` with a parent call `require_editable_parent`. Only top-level keeps the `ctx.role != Viewer` gate. After creation, call `ensure_creator_can_edit`. For import, call it for every created root.

- [ ] **Step 1: Failing tests.**
  - `create_under_unreadable_restricted_parent_is_404`: the body is byte-identical to the one for a random parent uuid.
  - `viewer_with_editor_grant_can_create_inside_restricted`.
  - `creator_with_non_inheriting_grant_gets_owner_on_new_page`.
  - `editor_cannot_move_out_of_restricted_subtree`: Bob has an Editor grant on boundary B; moving child C to the top level gets 403 `acl.restricted_move`, and C is unchanged.
  - `owner_can_move_out_of_restricted_subtree`.
  - `move_into_unreadable_parent_is_404`.
  - `move_within_same_boundary_needs_only_editor`.
  - `from_template_under_unreadable_parent_is_404`.
  - `import_under_unreadable_parent_is_404`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test restricted_integration --test cross_workspace_integration --test placement_integration --test docs_integration --test templates_integration --test workspace_import_integration` and `cargo test -p knot-storage` pass.

### Task 7: Duplicate

**Files:**
- Modify: `crates/knot-server/src/duplicate.rs`, `crates/knot-storage/src/duplicate.rs`, and `DocStore::live_subtree` (return `restricted`).
- Test: `crates/knot-server/tests/duplicate_integration.rs`.

**Interfaces:**
- Consumes: `restricted::require_editable_parent` and `ensure_creator_can_edit` (Task 6), and `AclCache::visibility` (Task 3).
- `PageCopy` gains `restricted: bool`, written by `insert_page`.
- In `run()`:
  - Drop `live_subtree` rows that are not `vis.readable`. Also drop every row whose parent was dropped; the list is depth-first, so track the dropped ids in a set.
  - After insertion, for each copied page with `restricted`, upsert `user:<actor>` owner inherit=true. Then call `ensure_creator_can_edit` on the top copy.
- `check()`:
  - When the source has a parent, `require_editable_parent(source.parent_id)`.
  - Drop the `ctx.role != Viewer` part when there is a parent, and keep it for top-level sources.
- `choose_boards` / `choose_blobs`: when an asset's owning doc is not copied, and its `restriction_root` (`vis.access(owner).restriction_root`) differs from the restriction root of the copy's destination, `continue` (keep the reference). Compute the destination root from the source's parent `vis.access(parent).restriction_root`, or `None` at the top level.

- [ ] **Step 1: Failing tests.**
  - `duplicate_skips_unreadable_restricted_subpages`: the copy tree lacks R' and R's children, and no title of R appears in any created doc.
  - `duplicate_of_boundary_stays_restricted_and_actor_is_owner`: the copy has `restricted:true`; a non-grantee member gets 403 on it; the actor's grants list owner.
  - `duplicate_does_not_rehome_restricted_board_or_blob`: a board and blob owned by restricted R are embedded in unrestricted A; duplicating A leaves the copy referencing the original ids, and a plain member gets 403 on `/api/boards/{id}/svg` and on `/api/blobs/{id}`.
  - Keep `comments_history_shares_and_grants_are_not_copied` green and update its comment to the new rationale.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test duplicate_integration --test restricted_integration` passes.

### Task 8: Archive, purge, restore, archive list

**Files:**
- Modify: `crates/knot-server/src/routes/api/docs.rs`, specifically `archive`, `purge`, `restore`, `list_archive` and `archive_info`.
- Modify: `crates/knot-storage/src/doc_store.rs`, specifically `restore_once`.
- Test: `crates/knot-server/tests/archive_integration.rs`.

**Interfaces:**
- `archive`: before the store call, take `descendant_ids` of the live subtree. If any id is not `vis.readable`, return 409 `doc.hidden_descendants`. `purge` does the same over `subtree_ids`, also with `vis.readable` (an archived doc the actor only views counts as hidden; see the spec).
- `restore_once`: in the branch that re-parents to the top level, if `restriction_root_of(old expected_parent)` is `Some`, return the new `DocStoreError::RestoreRestrictedParentArchived`. The route maps it to 409 `doc.restore_restricted_parent_archived`.
- `list_archive`:
  - Keep only rows where `vis.access(root.id)` has a role of Editor+.
  - Null `parent_id` and `parent_title` unless the parent is readable.
  - `subpage_count` counts only ids in `subtree_ids(root)` that pass `vis.readable`. Compute this in Rust per row; the archive list is small.

- [ ] **Step 1: Failing tests.**
  - `archive_with_hidden_restricted_descendant_is_409`.
  - `purge_with_hidden_restricted_descendant_is_409`.
  - `restore_refuses_to_reparent_out_of_restricted_subtree`.
  - `archive_list_hides_restricted_roots_and_parent_titles`.
  - `archive_banner_does_not_name_unreadable_root`, using GET on the child for a grantee of the child only.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test archive_integration --test restricted_integration` and `cargo test -p knot-storage` pass.

### Task 9: Search, tasks, comments

**Files:**
- Modify: `crates/knot-storage/src/search.rs`, giving `SearchStore::search` a `hidden: &[Uuid]` parameter; and `crates/knot-server/src/routes/api/search.rs`.
- Modify: `crates/knot-storage/src/tasks.rs`. `list_for_assignee` gains `hidden: &[Uuid]` and returns the doc title via a JOIN, by adding `doc_title: String` to the returned row type or a new `AssignedTask` struct. Also modify `crates/knot-server/src/routes/api/tasks.rs`, which drops the N+1 `docs.get`.
- Modify: `crates/knot-server/src/routes/api/comments.rs`. In `broadcast_mentions`, filter recipients by `acl.effective_role(ws, doc, uid).is_some()`. In `delete_comment`, the moderator check uses `EffectiveDocRole == Owner`.
- Test: `search_integration.rs`, `archive_integration.rs` (the tasks test lives there), `comments_integration.rs`, `crates/knot-storage/tests/tasks.rs`.

**Interfaces:**
- SQL deny-list: `AND NOT (d.id = ANY($n))`. Bind an empty array when there is nothing to hide. Search sets `hit.parent_id = None` when the parent is hidden. The per-hit `effective_role` loop stays.

- [ ] **Step 1: Failing tests.**
  - `search_never_returns_restricted_doc_to_non_grantee`.
  - `search_is_not_starved_by_restricted_hits`: 45 restricted docs with the term in the title, 1 readable doc with the term in the body. The readable doc is returned.
  - `deep_grantee_search_hit_has_no_parent_id`.
  - `my_tasks_skip_restricted_docs`: modelled on `my_tasks_skip_archived_docs`.
  - `workspace_owner_with_editor_grant_cannot_delete_others_comment_in_restricted_doc`.
  - `mention_broadcast_skips_members_without_access`: LISTEN `comment_mentions` on a raw `PgListener` and assert the `user_ids` payload.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-storage` and `cargo test -p knot-server --test search_integration --test archive_integration --test comments_integration --test tasks_live_reindex --test tasks_lockstep` pass.

### Task 10: Workspace export and import round trip

**Files:**
- Modify: `crates/knot-server/src/routes/api/export_import.rs`.
- Test: `crates/knot-server/tests/workspace_import_integration.rs`, `export_integration.rs`.

**Interfaces:**
- `export_workspace` filters `all_docs` by `vis.readable`. In `write_export_zip`, `resolve_parent` always drops parents outside the export set; remove the `reparent_roots` distinction if it becomes dead.
- `DocEntry` gains `#[serde(default)] restricted: bool`. It is true for docs with `restricted`, and for zip roots whose `vis.access(id).restriction_root.is_some()`.
- Import creates docs with the flag via `DocStore::set_restricted(ws, id, importer, true)` after creation, which also grants the importer owner. Import then calls `ensure_creator_can_edit` on the roots (already done in Task 6).

- [ ] **Step 1: Failing tests.**
  - `workspace_export_excludes_restricted_docs_owner_cannot_read`: the zip has no file for the doc and its id appears nowhere in `index.json`.
  - `export_import_round_trip_keeps_restriction`: a grantee exports the subtree (`/api/docs/{id}/export?descendants=true`), imports it at the root, the imported boundary has `restricted:true`, and another member gets 403.
  - `subtree_export_prunes_restricted_child`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test workspace_import_integration --test export_integration --test restricted_integration` passes.

### Task 11: Public share links

**Files:**
- Modify: `crates/knot-server/src/routes/api/shares.rs`, `crates/knot-server/src/routes/public.rs`.
- Test: `crates/knot-server/tests/shares_integration.rs`, plus the unit tests in `public.rs`.

**Interfaces:**
- A helper in `public.rs`, `async fn alive_share(state, token) -> Result<ShareToken, Response>`, wraps `find_alive` and then returns 410 when `restriction_root_of(doc)` is `Some`. Use `GrantStore::chain_for_principal(ws, doc, "")` with a check for any `restricted`, or a small `DocStore::restriction_root(ws, doc) -> Option<Uuid>`; add the latter if it is cleaner, using the same CTE as Task 6. `public_doc`, `public_blob` and `public_board_svg` all use `alive_share`.
- The link rewrite skips (emits `#` for) targets whose restriction root is `Some`.
- `Cache-Control: private, no-cache` on all three `/p/*` responses.
- `shares::create` returns 409 `share.restricted` when the doc's `restriction_root` is `Some`. Read it from `acl.access(...)`.

- [ ] **Step 1: Failing tests.**
  - `cannot_create_share_inside_restricted_subtree`.
  - `share_stops_working_when_doc_moved_into_restricted_subtree`: 410 after the move.
  - `public_page_does_not_link_to_restricted_targets`.
  - `public_responses_are_private_no_cache`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test shares_integration --test restricted_integration` and `cargo test -p knot-server --lib public` pass.

### Task 12: Member removal drops grants

**Files:**
- Modify: `crates/knot-storage/src/workspace_store.rs`. `remove_member` runs in a transaction that deletes `document_grants` where `principal = 'user:<id>'` and the doc is in the workspace. It records `invalidations::record_in_tx(ws, doc, "member-remove")` for each affected doc.
- Modify: `crates/knot-server/src/routes/api/workspace.rs` only if the signature changes.
- Test: `crates/knot-storage/tests/workspace_members.rs`, `crates/knot-server/tests/auth_oidc_integration.rs`.

- [ ] **Step 1: Failing tests.**
  - In storage, `remove_member_deletes_their_grants_and_invalidates`, which counts `acl_invalidations` rows.
  - In OIDC, `removed_member_reprovisioned_via_oidc_does_not_regain_restricted_access`. Follow the existing OIDC test harness; if that test needs Dex and the suite skips without it, mirror the harness's skip behaviour.
  - In `restricted_integration.rs`, `reinvited_member_does_not_regain_restricted_access`.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-storage` and `cargo test -p knot-server --test restricted_integration --test auth_oidc_integration` pass.

### Task 13: Realtime: board revocation and the join race

**Files:**
- Modify: `crates/knot-crdt/src/board_registry.rs`, `crates/knot-crdt/src/board_room.rs`, `crates/knot-crdt/src/registry.rs`.
- Modify: `crates/knot-server/src/board_room_shim.rs`, `crates/knot-server/src/room.rs`, `crates/knot-server/src/lib.rs` (both upgrades), `crates/knot-server/src/main.rs` (`on_invalidate`), `crates/knot-server/src/archive.rs` (purge).
- Test: `crates/knot-crdt/src/board_room.rs` tests, `crates/knot-crdt/tests/board_fanout.rs`, `crates/knot-server/tests/restricted_integration.rs` (WS helpers from `contributors_integration.rs`: `Peer::join`, `close_code`).

**Interfaces:**
- Produces:
  ```rust
  // BoardRooms
  pub async fn acquire(&self, board_id: Uuid, doc_id: Uuid) -> Result<Arc<BoardRoomHandle>, BoardAcquireError>; // BoardRoomHandle gains doc_id + revoked: CancellationToken
  pub async fn revoke_for_doc(&self, doc_id: Uuid);   // cancels revoked of matching handles, then evicts them; bumps epoch
  pub fn acl_epoch(&self, doc_id: Uuid) -> u64;
  // Rooms
  pub fn acl_epoch(&self, doc_id: Uuid) -> u64;        // striped [AtomicU64; 64] by hash(doc_id); bumped in revoke_all_for_doc + revoke_and_evict even if no room is live
  ```
- `board_room.rs`: add `Event::Revoke`, mirroring `room.rs`'s handler. `on_join` refuses when `revoked` is cancelled.
- `board_room_shim::serve` passes `handle.revoked.child_token()` to `ws_writer::run`.
- Upgrades:
  - Snapshot `acl_epoch(doc)` before `acl.access`, and pass it into `room::serve` / `board_room_shim::serve`.
  - After the join succeeds, if `acl_epoch(doc) != snapshot`, cancel the connection's revoke token, which closes with 4403.
- `main.rs on_invalidate` also spawns `board_rooms.revoke_for_doc(id)`. Purge does the same.

- [ ] **Step 1: Failing tests.**
  - In `board_room.rs`, `revoke_signals_each_conn_and_refuses_join`.
  - In `registry.rs`, `revoke_bumps_epoch_without_live_room`.
  - In `restricted_integration.rs`, `restrict_closes_doc_and_board_sockets`: Bob has a doc socket and a board socket open as a workspace editor. Alice restricts. Both sockets close with 4403 within 5 s. Bob's reconnect upgrade is refused with 403.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-crdt` and `cargo test -p knot-server --test restricted_integration --test contributors_integration --test convergence` pass.

### Task 14: Break-glass admin CLI

**Files:**
- Modify: `crates/knot-server/src/admin.rs`.
- Test: `crates/knot-server/tests/admin_cli_integration.rs`.

**Interfaces:**
- Add `AdminCmd::RestrictedList { workspace: String }`. It prints one line per doc with `restricted = true`: `<doc_id>\t<title>\towners=<comma-separated emails>`.
- Add `AdminCmd::Grant { doc: Uuid, email: String, role: String (owner|editor|viewer), no_inherit: bool }`.
  - The user must be a member of the doc's workspace, otherwise error.
  - It goes through `GrantStore::put`, so the invalidation fires.
  - It then writes an audit `admin.grant` via `audit::record_with_data` (or the closest existing helper), with actor `None` and data `{"break_glass": true, "principal": ..., "role": ...}`.
- Expose testable fns `pub async fn restricted_list(...) -> anyhow::Result<Vec<String>>` and `pub async fn grant(...) -> anyhow::Result<()>`, mirroring `create_owner`.

- [ ] **Step 1: Failing tests.**
  - `admin_restricted_list_shows_boundaries_and_owners`.
  - `admin_grant_recovers_orphaned_restricted_doc`: the sole owner-grant holder is removed from the workspace, so the workspace owner gets 403. After `grant`, the workspace owner gets 200.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p knot-server --test admin_cli_integration` passes.

### Task 15: Web: schemas, API, PermissionsDialog

**Files:**
- Modify: `web/src/lib/validators.ts`. `Doc` gains `restricted`, `within_restricted`, `parent_hidden` (fallback false) and `effective_role` (fallback null, nullable picklist). `DocWithRole` gains `restricted`, `within_restricted` and `restriction_root_id` (nullable, fallback null). Add a new `DocAccessInfo` schema for `/access`.
- Modify: `web/src/features/docs/docs.api.ts`. Add `setRestricted(id, restricted)` (PUT `/api/docs/{id}/restricted`) and `access(id)` (GET `/api/docs/{id}/access`).
- Modify: `web/src/features/permissions/PermissionsDialog.tsx` and its test.

**Copy (exact):**
- Section heading: `Access`. Switch label: `Restrict access`. Use `data-testid="restricted-toggle"`, `role="switch"`, `aria-checked`.
- Help text when off: `Everyone in the workspace can see this page according to their workspace role.`
- Help text when on: `Only the people listed below can see this page and its subpages. Workspace roles no longer apply.`
- Confirm text on enabling: `Only people you add will keep access to this page and its subpages. You will be added as an owner.`
- Inherited restriction (`within_restricted && !restricted`): a disabled, checked switch with `Restricted by "<root title>"`, or `Restricted by a parent page` when the root title is null.
- Public link section when `within_restricted`: `Public links are disabled for restricted pages.` with no enable button.
- Grants heading: `People with access` when `within_restricted`, otherwise `Explicit grants on this document.`
- Empty state when not restricted: `No explicit grants. Everyone in the workspace has access through their workspace role.`
- Toasts:
  - `grant.last_owner`: `A restricted page needs at least one owner.`
  - `grant.not_member`: `Only workspace members can be added.`
- When restricted, the default for `addInherit` is true, with the hint `Untick "Inherit" to share only this page, not its subpages.`
- After a toggle, invalidate `['doc', id]`, `['docs']`, `['grants', id]`, `['shares', id]` and `['doc-access', id]`.

- [ ] **Step 1: Failing vitest tests.**
  - `validators.test.ts`: a `Doc` without the new fields parses with the defaults.
  - `docs.api.test.ts`: `setRestricted` sends PUT with body `{restricted:true}`.
  - `PermissionsDialog.test.tsx`: extend the stub so `/api/docs/d1` returns a `DocWithRole` and `/api/docs/d1/access` returns a `DocAccessInfo`. Add:
    - `shows restrict toggle off and enables with confirm`: stub `window.confirm`; assert the PUT body and the refetch.
    - `inherited restriction shows disabled switch with root title`.
    - `public link disabled inside restricted subtree`.
    - `last owner error shows toast`.
- [ ] **Step 2:** Run the vitest files. They should fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Vitest on these files passes, and `tsc --noEmit` and `eslint` are clean.

### Task 16: Web: sidebar, DocPage, link picker, members

**Files:**
- Modify: `web/src/features/docs/tree.ts`. `buildTree` returns `{ roots, shared }`, where `shared` holds the `parent_hidden` docs as their own roots. Update every caller. `rowDescription` appends `, restricted` when `within_restricted`.
- Modify: `web/src/features/docs/DocTree.tsx` and `TreeRow.tsx`.
  - A `Shared with you` section with `data-testid="tree-shared"`, rendered only when non-empty.
  - A `Lock` icon (lucide) with `aria-label="Restricted"` on `restricted` rows. Use a muted `Lock` with `aria-label="In a restricted page"` on rows that are `within_restricted` but not `restricted`.
  - Row actions are gated by `node.effective_role`: `canEditRow = owner|editor` and `isOwnerRow = owner`. The top-level "New document" button keeps the workspace role.
  - `parent_hidden` rows are not draggable, and `keyboardMove` returns null for them.
  - A drop whose target restriction state (`within_restricted` of the target parent, or false at the root) differs from the moved doc's calls `window.confirm` first:
    - Into: `Moving "<title>" into "<target>" limits who can see it to people with access there.`
    - Out: `Moving "<title>" here makes it visible to everyone in the workspace.`
  - Move `onError` maps `acl.restricted_move` to `Only an owner of this page can move it out of a restricted area.`
  - `archiveErrorText` maps `doc.hidden_descendants` to `This page has subpages you can't see. Ask their owners to move them first.`
  - It also maps `doc.restore_restricted_parent_archived` to `Restore the archived parent page first.`
- Modify: `web/src/features/docs/DocPage.tsx`.
  - A header chip `data-testid="restricted-chip"` with a Lock icon and the text `Restricted`, shown when `within_restricted`. For owners it links to `permissions`.
  - The not-found branch says `This page doesn't exist or you don't have access to it.`
  - In the lost-access branch, call `qc.invalidateQueries({queryKey:['docs']})` and `qc.removeQueries({queryKey:['doc', id]})` before navigating.
- Modify: `web/src/features/editor/EditorToolbar.tsx`.
  - Show a lock next to `within_restricted` picker entries.
  - When inserting a link with no selection, if the target is `within_restricted` and the current doc is not, the link text is `Restricted page`.
- Modify: `web/src/features/editor/InternalLinkExtension.ts` and `prose.css`. Add the class `knot-internal-link--locked` when the target id is not in the `['docs']` cache. Pass the readable-id set in through the extension's options or storage, and redecorate when it changes.
- Modify: `web/src/features/workspace/MembersPage.tsx`. The remove confirm text is `Remove <email>? They also lose every page-level grant, including access to restricted pages.`
- Tests: `tree.test.ts` (shared grouping, `parent_hidden` not keyboard-movable), `DocTree.archive.test.tsx` style (lock icon and per-row gating), `DocTree.dnd.test.tsx` (confirm on boundary crossing), `DocPage.archived.test.tsx` (chip, no-access copy, `['docs']` invalidated), `EditorToolbar.link.test.tsx` (restricted text), `archive.test.ts`, `MembersPage.test.tsx`.

- [ ] **Step 1:** Write the failing tests named above, asserting the exact copy.
- [ ] **Step 2:** The tests fail.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `vitest run` (the full web suite), `tsc --noEmit` and `eslint` are clean.

### Task 17: e2e and docs

**Files:**
- Create: `e2e/flows/restricted.spec.ts`, modelled on `role-gating.spec.ts`, with two browser contexts.
  - The owner creates P with child C and invites an editor.
  - The owner restricts P via the dialog. The editor's sidebar lacks P, and a direct URL shows the no-access copy.
  - The owner grants the editor Viewer on C (no inherit). The editor sees C under "Shared with you" without P's title anywhere on the page.
- Modify: `.claude/skills/knot-api/SKILL.md`. Add the restrict and access endpoints, the new error codes, and the uniform no-access contract.
- Modify: `CHANGELOG.md`. Add an `### Added` entry under Unreleased, following the file's existing format.
- Modify: `docs/superpowers/README.md`. Add a row.
- Modify: `ARCHITECTURE.md`. Update the ACL section if one exists.

- [ ] **Step 1:** Write the spec. Run it with the e2e procedure from the repo (`make e2e` needs the server, Vite and the DB). If the environment can't run it, report that rather than claiming it passed.
- [ ] **Step 2:** Update the docs.
