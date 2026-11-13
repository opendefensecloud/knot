# Subtree-scoped API keys Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An API key can be limited to one page and its subpages (present and future); outside that subtree it reaches nothing.

**Architecture:** The root is folded into the one ACL rule in `knot-docs` (`rule::step`), so `acl::resolve`, `Visibility` and every route that asks the `AclCache` narrow without their own checks. The key's root travels `api_keys.root_doc_id` → `find_live_by_hash` → `AuthContext.root_doc_id` → every `AclCache` call. Workspace-wide routes are refused by a path table in `auth/scope.rs`; top-level destinations by one check in `require_editable_destination` and `duplicate.rs`.

**Tech Stack:** Rust (axum, sqlx, moka, Postgres 18), React + TanStack Query + valibot, Vitest, Playwright.

**Spec:** [docs/superpowers/specs/2026-11-13-subtree-scoped-api-keys-design.md](../specs/2026-11-13-subtree-scoped-api-keys-design.md)

## Global Constraints

- A key's reach is **user ∩ HTTP scopes ∩ subtree**. The subtree only narrows; no code path may grant access because of a root.
- `root_doc_id NULL` = no subtree = today's behaviour, byte for byte. Cookie sessions always have `root_doc_id: None`.
- Foreign key `ON DELETE CASCADE`, never `SET NULL`.
- Trigger error code: `KB005`. Threat id in code comments: `S10`.
- Page outside the subtree: `404 doc.not_found` (indistinguishable from no access). Workspace-wide route or top-level destination: `403 api_key.subtree_denied`.
- Mint with an unreadable root: `404 doc.not_found`.
- Tests run against the dev-compose Postgres (`knot_test_support::fresh_db()`), never testcontainers. Start it with `make compose.up` if `cargo nextest` fails to connect.
- Run Rust tests with `cargo nextest run -p <crate> <filter>`; web tests with `cd web && pnpm test -- <file>`.
- Every commit ends with the trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Commits are GPG-signed; if signing prompts, the human enters the passphrase — never pass `--no-gpg-sign`.

---

### Task 1: Root in the ACL rule (`knot-docs/src/rule.rs`)

**Files:**
- Modify: `crates/knot-docs/src/rule.rs` (`FoldState`, `step`, `access_along_path`, `compute_access`, tests)
- Modify: `crates/knot-docs/src/cache.rs:48-66` (`Visibility::from_nodes` forwards the root)
- Modify: `crates/knot-docs/src/acl.rs:125` (pass `None` for now; Task 2 threads the real value)

**Interfaces:**
- Produces:
  - `pub fn access_along_path(path: &[AclNode], grants: &[PrincipalGrant], workspace_role: Option<WorkspaceRole>, scope_root: Option<Uuid>) -> Option<DocAccess>`
  - `pub fn compute_access(nodes: &[AclNode], grants: &[PrincipalGrant], workspace_role: Option<WorkspaceRole>, scope_root: Option<Uuid>) -> HashMap<Uuid, DocAccess>`
  - `pub fn Visibility::from_nodes(nodes: &[AclNode], grants: &[PrincipalGrant], workspace_role: Option<WorkspaceRole>, scope_root: Option<Uuid>) -> Self`

- [ ] **Step 1: Write the failing tests** — append to `mod tests` in `rule.rs`:

```rust
    /// S10: a root narrows access to itself and its descendants; grants and
    /// the workspace role still decide the role inside.
    #[test]
    fn scope_root_limits_access_to_its_subtree() {
        let top = node(id(1), None, false);
        let root = node(id(2), Some(top.id), false);
        let child = node(id(3), Some(root.id), false);
        let grandchild = node(id(4), Some(child.id), false);
        let sibling = node(id(5), Some(top.id), false);
        let nodes = [top, root, child, grandchild, sibling];
        let scope = Some(root.id);

        assert_eq!(access_along_path(&[top], &[], Some(Editor), scope), None);
        assert_eq!(access_along_path(&[top, sibling], &[], Some(Editor), scope), None);
        for path in [&[top, root][..], &[top, root, child], &[top, root, child, grandchild]] {
            let access = access_along_path(path, &[], Some(Editor), scope).unwrap();
            assert_eq!(access.role, Editor);
        }

        let batched = compute_access(&nodes, &[], Some(Editor), scope);
        let mut reachable: Vec<Uuid> = batched.keys().copied().collect();
        reachable.sort();
        assert_eq!(reachable, vec![root.id, child.id, grandchild.id]);
    }

    /// An inheriting grant above the root still flows into the subtree:
    /// the root masks, it doesn't reset the fold.
    #[test]
    fn scope_root_keeps_grants_inherited_from_above() {
        let top = node(id(1), None, false);
        let root = node(id(2), Some(top.id), false);
        let grants = [grant(top.id, Owner, true)];

        let access = access_along_path(&[top, root], &grants, Some(Viewer), Some(root.id)).unwrap();

        assert_eq!(access.role, Owner);
    }

    /// A restricted page inside the subtree stays closed without a grant on
    /// it, and opens with one.
    #[test]
    fn scope_root_respects_restriction_inside() {
        let root = node(id(1), None, false);
        let locked = node(id(2), Some(root.id), true);
        let scope = Some(root.id);

        assert_eq!(access_along_path(&[root, locked], &[], Some(Owner), scope), None);
        let grants = [grant(locked.id, Editor, false)];
        let access = access_along_path(&[root, locked], &grants, Some(Viewer), scope).unwrap();
        assert_eq!(access.role, Editor);
        assert_eq!(access.restriction_root, Some(locked.id));
    }

    /// A root the user can't reach grants nothing below it.
    #[test]
    fn scope_root_never_grants() {
        let root = node(id(1), None, true);
        let child = node(id(2), Some(root.id), false);

        assert_eq!(access_along_path(&[root, child], &[], Some(Owner), Some(root.id)), None);
        assert!(compute_access(&[root, child], &[], Some(Owner), Some(root.id)).is_empty());
    }

    /// A root that is not in the tree at all (deleted, foreign) reaches nothing.
    #[test]
    fn unknown_scope_root_reaches_nothing() {
        let top = node(id(1), None, false);

        assert_eq!(access_along_path(&[top], &[], Some(Owner), Some(id(99))), None);
        assert!(compute_access(&[top], &[], Some(Owner), Some(id(99))).is_empty());
    }
```

Also extend the existing randomized test `compute_access_matches_access_along_path` (around line 379): inside its loop, pick `let scope_root = if generator.below(3) == 0 { None } else { Some(nodes[generator.below(nodes.len() as u64) as usize].id) };` and pass `scope_root` to both `compute_access` and `access_along_path`, so both evaluators are proven equal with roots too. Every other existing call in the test module gets a trailing `, None`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p knot-docs rule::`
Expected: compile error "this function takes 3 arguments but 4 arguments were supplied".

- [ ] **Step 3: Implement** — in `rule.rs`:

```rust
/// What flows from a doc down to its children.
#[derive(Debug, Clone, Copy)]
struct FoldState {
    /// False from the nearest restricted ancestor-or-self downwards.
    workspace_role_applies: bool,
    /// Max role of the inherit=true grants since the last boundary.
    inherited: Option<WorkspaceRole>,
    /// The nearest restricted ancestor-or-self.
    boundary: Option<Uuid>,
    /// S10: true at and below the key's scope root; always true without one.
    /// Only masks the result: grants and boundaries fold as without a root.
    in_scope: bool,
}

impl FoldState {
    fn start(scope_root: Option<Uuid>) -> FoldState {
        FoldState {
            workspace_role_applies: true,
            inherited: None,
            boundary: None,
            in_scope: scope_root.is_none(),
        }
    }
}
```

`step` gets a `scope_root: Option<Uuid>` parameter (after `workspace_role`). The restricted branch keeps `in_scope: parent_state.in_scope`. After building `state`, add `if scope_root == Some(node.id) { state.in_scope = true; }`. The access becomes:

```rust
    let access = role
        .filter(|_| state.in_scope)
        .map(|role| DocAccess {
            role,
            archived: node.archived,
            restriction_root: state.boundary,
        });
```

`access_along_path` and `compute_access` take `scope_root: Option<Uuid>` as last parameter, start from `FoldState::start(scope_root)` instead of `FoldState::ROOT` (delete the const), and pass `scope_root` to `step`. Update the module doc (line 1-12): add one paragraph "A key's scope root (S10) masks the result: only the root and its descendants get access; the fold above it is unchanged, so inherited grants still count."

`Visibility::from_nodes` gains `scope_root: Option<Uuid>` and forwards it. In `acl.rs:125` and `cache.rs` `load_visibility`, pass `None` for now.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo nextest run -p knot-docs`
Expected: all PASS (DB-backed tests in `acl.rs`/`cache.rs` included).

- [ ] **Step 5: Commit**

```bash
git add crates/knot-docs
git commit -m "feat(acl): scope root masks access to one subtree (S10)"
```

---

### Task 2: Thread the root through `AclCache` and `AuthContext`

**Files:**
- Modify: `crates/knot-docs/src/acl.rs` (`resolve`)
- Modify: `crates/knot-docs/src/cache.rs` (cache keys, every public method, tests)
- Modify: `crates/knot-server/src/auth/context.rs` (`AuthContext.root_doc_id`)
- Modify: every `AuthContext { … }` literal (17 sites: `auth/api_key_loader.rs`, `auth/scope.rs` ×2, `auth/session_loader.rs`, `quota.rs` ×2, `rate_limit/http.rs`, `routes/api/api_keys.rs` ×2, `routes/api/docs.rs`, `routes/api/workspace.rs`, `routes/auth/local.rs`, `tests/duplicate_integration.rs` ×2, `tests/middleware_unit.rs` ×2, `tests/rate_limit_integration.rs`)
- Modify: every `AclCache` call site in `crates/knot-server/src` (≈40; the compiler lists them)

**Interfaces:**
- Consumes: Task 1 signatures.
- Produces:
  - `pub async fn resolve(workspaces, grants, _docs, workspace_id: Uuid, doc_id: Uuid, user_id: Uuid, scope_root: Option<Uuid>) -> Result<Option<DocAccess>, ResolveError>`
  - `AclCache::access(&self, workspace_id: Uuid, doc_id: Uuid, user_id: Uuid, scope_root: Option<Uuid>)`, same trailing parameter on `effective_role`, `visibility(workspace_id, user_id, scope_root)`, `visibility_covering(workspace_id, user_id, scope_root, doc_ids)`.
  - `pub struct AuthContext { pub user_id: Uuid, pub workspace_id: Uuid, pub role: WorkspaceRole, pub root_doc_id: Option<Uuid> }`

- [ ] **Step 1: Write the failing cache test** — append to `mod tests` in `cache.rs`, reusing that module's setup helpers (look at `visibility_matches_effective_role_for_every_doc` for how it creates a workspace, a member and docs):

```rust
    /// S10: the same user with and without a root never shares a cache
    /// entry, in either cache.
    #[tokio::test]
    async fn scope_root_is_part_of_the_cache_key() {
        // Build: workspace `ws`, editor `user`, docs `top` (no parent),
        // `root` under `top`, `child` under `root`, using this module's helpers.
        let cache = AclCache::new(/* the three stores, as the other tests do */);

        // Warm the unscoped entries first: a key that ignored the root would
        // now answer the scoped calls from them.
        assert!(cache.effective_role(ws, top, user, None).await.unwrap().is_some());
        assert!(cache.visibility(ws, user, None).await.unwrap().readable(top));

        assert!(cache.effective_role(ws, top, user, Some(root)).await.unwrap().is_none());
        assert!(cache.effective_role(ws, child, user, Some(root)).await.unwrap().is_some());
        let scoped = cache.visibility(ws, user, Some(root)).await.unwrap();
        assert!(!scoped.readable(top));
        assert!(scoped.readable(root) && scoped.readable(child));
    }
```

Fill the two commented spots with the exact helper calls the neighbouring tests use (no new helpers). Add the same check to `acl.rs` tests: in a new `#[tokio::test] async fn resolve_honours_scope_root()`, create `top` → `root` → `child` with `ctx()`, and assert `resolve(.., top, user, Some(root.id))` is `None` while `resolve(.., child, user, Some(root.id))` is `Some`.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo nextest run -p knot-docs scope_root`
Expected: compile error on the extra argument.

- [ ] **Step 3: Implement in `knot-docs`**

- `resolve`: new last parameter `scope_root: Option<Uuid>`, passed to `rule::access_along_path` (the chain is root-first, so the fold sees the root when it is an ancestor-or-self).
- `cache.rs`: `inner: Cache<(Uuid, Uuid, Uuid, Option<Uuid>), Option<DocAccess>>`, `visibility: Cache<(Uuid, Uuid, Option<Uuid>), Arc<Visibility>>`. Every public method and `access_after_load`/`access_with_hook`/`load_visibility` gain `scope_root: Option<Uuid>` right after `user_id`, include it in the key, and forward it to `resolve`/`Visibility::from_nodes`. `evict_doc`'s predicate `k.1 == doc_id` stays valid. Update the module doc's key descriptions to `(workspace_id, doc_id, user_id, scope_root)` and `(workspace_id, user_id, scope_root)`.
- Fix every existing call in `knot-docs` tests with a trailing `None`.

- [ ] **Step 4: Implement in `knot-server`**

- `AuthContext` gets:

```rust
    /// S10: the API key's scope root; `None` for cookie sessions and
    /// unscoped keys. Pass it to every `AclCache` call made for this request.
    pub root_doc_id: Option<Uuid>,
```

- Every `AuthContext { … }` literal gets `root_doc_id: None` (Task 4 sets the real value in `api_key_loader.rs`).
- Every `AclCache` call made for a request passes `ctx.root_doc_id` (or `auth_context.root_doc_id`). Helpers that receive a bare `user_id` from a request get a `scope_root: Option<Uuid>` parameter next to it and their callers pass `ctx.root_doc_id`: `archive::refuse_if_archived`, the image-asset loader in `export.rs` (≈line 177) and its callers. Calls made for someone other than the request's principal pass `None`: `routes/api/comments.rs:257` (notifying other users), `suggestion_notifications.rs:93` (background).
- `cargo build -p knot-server --all-targets` must be clean; there is no default — every call site is decided explicitly.

- [ ] **Step 5: Run everything**

Run: `cargo nextest run -p knot-docs -p knot-server`
Expected: all PASS (behaviour unchanged: every root is still `None`).

- [ ] **Step 6: Commit**

```bash
git add crates/knot-docs crates/knot-server
git commit -m "refactor(acl): thread the key's scope root through AclCache and AuthContext"
```

---

### Task 3: `api_keys.root_doc_id` in the database and the store

**Files:**
- Create: `migrations/20261114120000_api_keys_root_doc.sql` (column, index, KB005 trigger)
- Create: `migrations/20261114120001_api_keys_root_doc_fk.sql` (foreign key)
- Modify: `crates/knot-storage/src/api_key_store.rs` (`ApiKey`, `NewApiKey`, `COLUMNS`, `ApiKeyRow`, `api_key_from_row`, `create`, `map_insert_err`, `ApiKeyStoreError`)
- Modify: every `NewApiKey { … }` literal (store tests, `routes/api/api_keys.rs`, `tests/api_keys_integration.rs::guards_mint_all_scopes_key`, others the compiler finds) with `root_doc_id: None`
- Test: `crates/knot-storage/tests/api_keys.rs`, `crates/knot-storage/tests/migrations_apply.rs`

**Interfaces:**
- Produces:
  - `ApiKey.root_doc_id: Option<Uuid>`, `NewApiKey.root_doc_id: Option<Uuid>`
  - `ApiKeyStoreError::RootRejected` (KB005)

- [ ] **Step 1: Write the failing tests** — in `crates/knot-storage/tests/api_keys.rs`. Extend `Fx` with a `docs: PgDocStore` built from the pool, and add (use the doc-creation call the other storage tests use, e.g. `PgDocStore::create`; check its signature in `doc_store.rs`):

```rust
#[tokio::test]
async fn root_doc_round_trips_and_is_found_by_hash() {
    let fx = setup().await;
    let root = create_doc(&fx, None, "Project").await;
    let k = mint_rooted(&fx, fx.alice, b"h-root", Some(root)).await.unwrap();
    assert_eq!(k.root_doc_id, Some(root));
    let found = fx.store.find_live_by_hash(b"h-root").await.unwrap().unwrap();
    assert_eq!(found.root_doc_id, Some(root));
}

#[tokio::test]
async fn purging_the_root_deletes_the_key() {
    let fx = setup().await;
    let root = create_doc(&fx, None, "Project").await;
    mint_rooted(&fx, fx.alice, b"h-purge", Some(root)).await.unwrap();
    sqlx::query("DELETE FROM documents WHERE id = $1").bind(root).execute(&fx.pool).await.unwrap();
    assert!(fx.store.find_live_by_hash(b"h-purge").await.unwrap().is_none());
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM api_keys WHERE key_hash = $1")
        .bind(b"h-purge".as_slice()).fetch_one(&fx.pool).await.unwrap();
    assert_eq!(rows, 0, "SET NULL would have widened the key to the workspace");
}

#[tokio::test]
async fn kb005_refuses_a_foreign_root_and_any_root_change() {
    let fx = setup().await;
    let other_ws = PgWorkspaceStore::new(fx.pool.clone()).create("other", "Other").await.unwrap();
    let foreign = create_doc_in(&fx, other_ws.id, None, "Foreign").await;
    let err = mint_rooted(&fx, fx.alice, b"h-foreign", Some(foreign)).await.unwrap_err();
    assert!(matches!(err, ApiKeyStoreError::RootRejected), "{err:?}");

    let root = create_doc(&fx, None, "A").await;
    let other = create_doc(&fx, None, "B").await;
    let k = mint_rooted(&fx, fx.alice, b"h-change", Some(root)).await.unwrap();
    for new_root in [Some(other), None] {
        let err = sqlx::query("UPDATE api_keys SET root_doc_id = $1 WHERE id = $2")
            .bind(new_root).bind(k.id).execute(&fx.pool).await.unwrap_err();
        assert_eq!(err.as_database_error().unwrap().code().as_deref(), Some("KB005"));
    }
}
```

Helpers to add next to `mint`/`mint_by`: `create_doc(fx, parent, title) -> Uuid` and `create_doc_in(fx, ws, parent, title) -> Uuid` (thin wrappers over the doc store's create, with `fx.alice` as creator), and `mint_rooted(fx, user_id, hash, root) -> Result<ApiKey, ApiKeyStoreError>` building a `NewApiKey` like `mint_by` with `root_doc_id: root`. In `migrations_apply.rs`, add the new migration to whatever list of expected migrations/columns it asserts (follow the existing `api_keys` entries there).

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p knot-storage --test api_keys root`
Expected: compile error, no field `root_doc_id`.

- [ ] **Step 3: Write the migrations** — two files, because `knot_lock_tables` takes one mode for all its tables and must never wait for one while holding another (its contract in `20261107120000_history_log_columns.sql`). Adding the column needs ACCESS EXCLUSIVE on `api_keys` only; the foreign key needs SHARE ROW EXCLUSIVE on both tables, which blocks no reader of `documents`. sqlx runs each file in its own transaction.

`migrations/20261114120000_api_keys_root_doc.sql`:

```sql
-- Subtree-scoped API keys, step 1 of 2 (S10,
-- specs/2026-11-13-subtree-scoped-api-keys-design.md).
--
-- root_doc_id NULL keeps today's meaning: the key reaches what its user
-- reaches. Set, the key reaches only that page and its descendants. The
-- foreign key (ON DELETE CASCADE, never SET NULL, which would silently
-- widen the key to the whole workspace) follows in step 2 under a lock
-- that doesn't block page loads.
--
-- A nullable column without a default is a catalog change only, but needs
-- ACCESS EXCLUSIVE on api_keys, which every key-authenticated request
-- reads. Take it in bounded attempts (knot_lock_tables, 20261107120000).
-- The previous release never names the column, so it keeps working.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261114120000_api_keys_root_doc',
                        ARRAY['api_keys']::regclass[],
                        'ACCESS EXCLUSIVE');

ALTER TABLE api_keys ADD COLUMN root_doc_id uuid NULL;
CREATE INDEX api_keys_root_doc_idx ON api_keys (root_doc_id)
    WHERE root_doc_id IS NOT NULL;

-- S10: the root lives in the key's workspace, and a key never changes its
-- root (no widening, no re-targeting). documents has no
-- UNIQUE (workspace_id, id), so a composite foreign key isn't available.
CREATE OR REPLACE FUNCTION api_keys_root_doc_guard() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
  IF TG_OP = 'UPDATE' THEN
    IF NEW.root_doc_id IS DISTINCT FROM OLD.root_doc_id THEN
      RAISE EXCEPTION 'api key % cannot change its root page', NEW.id
        USING ERRCODE = 'KB005';
    END IF;
    RETURN NEW;
  END IF;
  IF NEW.root_doc_id IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM documents
                      WHERE id = NEW.root_doc_id
                        AND workspace_id = NEW.workspace_id) THEN
    RAISE EXCEPTION 'api key root page % is not in workspace %',
      NEW.root_doc_id, NEW.workspace_id
      USING ERRCODE = 'KB005';
  END IF;
  RETURN NEW;
END;
$$;

CREATE TRIGGER api_keys_root_doc_guard
    BEFORE INSERT OR UPDATE OF root_doc_id ON api_keys
    FOR EACH ROW EXECUTE FUNCTION api_keys_root_doc_guard();
```

`migrations/20261114120001_api_keys_root_doc_fk.sql`:

```sql
-- Subtree-scoped API keys, step 2 of 2 (S10).
--
-- Purging a key's root page deletes the key: ON DELETE CASCADE. Never
-- SET NULL, which would turn a subtree key into one for the whole
-- workspace. The constraint needs SHARE ROW EXCLUSIVE on both tables: it
-- blocks writes to documents and api_keys until COMMIT, no reads. Its
-- validation scans api_keys, where every root_doc_id is still NULL.
-- documents first: it is the busier table.
SET LOCAL lock_timeout = '5s';
SELECT knot_lock_tables('20261114120001_api_keys_root_doc_fk',
                        ARRAY['documents', 'api_keys']::regclass[],
                        'SHARE ROW EXCLUSIVE');

ALTER TABLE api_keys
    ADD CONSTRAINT api_keys_root_doc_id_fkey
    FOREIGN KEY (root_doc_id) REFERENCES documents(id) ON DELETE CASCADE;
```

- [ ] **Step 4: Implement the store**

- `const ROOT_ERRCODE: &str = "KB005";` next to `SSO_TTL_ERRCODE`.
- `ApiKey` gets `pub root_doc_id: Option<Uuid>` (doc: "S10: the page whose subtree the key is limited to; `None` = no limit."). `NewApiKey` gets the same field, and its `Debug` impl prints it.
- `COLUMNS` appends `, root_doc_id`; `ApiKeyRow` appends `Option<Uuid>`; `api_key_from_row` sets `root_doc_id: r.10`.
- `create`'s `INSERT` adds `root_doc_id` as `$8` and `.bind(k.root_doc_id)`.
- `ApiKeyStoreError::RootRejected` with `#[error("api key root page rejected")]` and doc "The KB005 trigger refused the root: not in the key's workspace."; `map_insert_err` maps `KB005` to it.
- Every other `NewApiKey { … }` literal in the workspace gets `root_doc_id: None`.

- [ ] **Step 5: Run to verify**

Run: `cargo nextest run -p knot-storage && cargo build -p knot-server --all-targets`
Expected: PASS, clean build.

- [ ] **Step 6: Commit**

```bash
git add migrations crates/knot-storage crates/knot-server
git commit -m "feat(storage): api_keys.root_doc_id with KB005 guard (S10)"
```

---

### Task 4: Mint, list and authenticate subtree keys

**Files:**
- Modify: `crates/knot-server/src/auth/api_key_loader.rs:203` (set `root_doc_id: key.root_doc_id`)
- Modify: `crates/knot-server/src/routes/api/api_keys.rs` (`CreateRequest`, `KeyResponse`, `CreatedResponse`, `create_key`, `list_keys`, module doc)
- Test: `crates/knot-server/tests/api_keys_integration.rs`

**Interfaces:**
- Consumes: `AuthContext.root_doc_id` (Task 2), `NewApiKey.root_doc_id`, `ApiKey.root_doc_id`, `ApiKeyStoreError::RootRejected` (Task 3), `AclCache::access(.., scope_root)`.
- Produces: HTTP `POST /api/auth/api-keys` body field `root_doc_id?: uuid`; `GET` items carry `root_doc_id: uuid|null` and `root_title: string|null`; 201 body carries `root_doc_id`.

- [ ] **Step 1: Write the failing tests** — add to `api_keys_integration.rs` (new section header `// Subtree-scoped keys (S10)`):

```rust
/// A key limited to `root` with the given scopes.
async fn subtree_key(app: &axum::Router, s: &Session, root: &str, scopes: &[&str]) -> String {
    let (st, body) = send(
        app,
        s,
        "POST",
        "/api/auth/api-keys",
        Some(serde_json::json!({"name": "subtree", "scopes": scopes, "root_doc_id": root})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    assert_eq!(body["root_doc_id"], root);
    body["key"].as_str().unwrap().to_string()
}

/// A page under `parent`, created through the session.
async fn child_doc(app: &axum::Router, s: &Session, parent: &str, title: &str) -> String {
    let (st, body) = send(
        app,
        s,
        "POST",
        "/api/docs",
        Some(serde_json::json!({ "title": title, "parent_id": parent })),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    body["id"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn subtree_key_reaches_only_its_subtree() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let top = matrix_doc(&app, &s, "top").await;
    let root = child_doc(&app, &s, &top, "root").await;
    let child = child_doc(&app, &s, &root, "child").await;
    let sibling = child_doc(&app, &s, &top, "sibling").await;
    let key = subtree_key(&app, &s, &root, &["read", "create", "update"]).await;

    for inside in [&root, &child] {
        let (st, body) = guards_bearer(&app, &key, "GET", &format!("/api/docs/{inside}"), None).await;
        assert_eq!(st, StatusCode::OK, "{inside}: {body}");
    }
    for outside in [&top, &sibling] {
        let (st, body) = guards_bearer(&app, &key, "GET", &format!("/api/docs/{outside}"), None).await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{outside}: {body}");
    }
    // The root's parent is outside: blanked, not leaked.
    let (_, body) = guards_bearer(&app, &key, "GET", &format!("/api/docs/{root}"), None).await;
    assert!(body["parent_id"].is_null(), "{body}");

    let (st, body) = guards_bearer(&app, &key, "PATCH", &format!("/api/docs/{child}"),
        Some(serde_json::json!({"title": "renamed"}))).await;
    assert!(st.is_success(), "{body}");
    let (st, body) = guards_bearer(&app, &key, "PATCH", &format!("/api/docs/{sibling}"),
        Some(serde_json::json!({"title": "renamed"}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");

    let (st, body) = guards_bearer(&app, &key, "POST", "/api/docs",
        Some(serde_json::json!({"title": "new", "parent_id": child}))).await;
    assert_eq!(st, StatusCode::CREATED, "{body}");
    let (st, body) = guards_bearer(&app, &key, "POST", "/api/docs",
        Some(serde_json::json!({"title": "new", "parent_id": sibling}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");

    // The tree lists the subtree only.
    let (st, body) = guards_bearer(&app, &key, "GET", "/api/docs", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let listed: Vec<&str> = body.as_array().unwrap().iter()
        .map(|d| d["id"].as_str().unwrap()).collect();
    assert!(listed.contains(&root.as_str()) && listed.contains(&child.as_str()), "{body}");
    assert!(!listed.contains(&top.as_str()) && !listed.contains(&sibling.as_str()), "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn subtree_key_mint_needs_a_readable_root_and_lists_it() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let root = matrix_doc(&app, &s, "Project X").await;

    let (st, body) = send(&app, &s, "POST", "/api/auth/api-keys",
        Some(serde_json::json!({"name": "k", "scopes": ["read"],
                                "root_doc_id": uuid::Uuid::new_v4()}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "doc.not_found");

    subtree_key(&app, &s, &root, &["read"]).await;
    let (st, body) = send(&app, &s, "GET", "/api/auth/api-keys", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let listed = body.as_array().unwrap().iter().find(|k| k["name"] == "subtree").unwrap();
    assert_eq!(listed["root_doc_id"], root);
    assert_eq!(listed["root_title"], "Project X");
}
```

Adjust the `GET /api/docs` parsing to the actual response shape of `routes/api/docs.rs::list` (it serializes `ListedDoc` with the doc flattened or nested — read the struct and match it). Add one more test, `bot_subtree_key_needs_a_root_the_bot_can_read`: with `invite_bot` create a bot (editor), `restrict_doc` a page so only the owner can read it, and assert minting a bot key (`"user_id": bot`) rooted there answers `404 doc.not_found`, while rooting it at an unrestricted page answers 201.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p knot-server --test api_keys_integration subtree`
Expected: FAIL — the body field is ignored, so `root_doc_id` in the 201 is missing and outside pages answer 200.

- [ ] **Step 3: Implement**

- `api_key_loader.rs`: `root_doc_id: key.root_doc_id` in the `AuthContext` it builds.
- `api_keys.rs`:
  - `CreateRequest` gets `#[serde(default)] root_doc_id: Option<Uuid>`; module doc line becomes `body: {name, scopes, expires_in_days?, user_id?, root_doc_id?}`.
  - After TTL validation and before `store.create`, when `body.root_doc_id` is `Some(root)`:

```rust
        // S10: the key's principal must read the root now. An id that does
        // not exist and one it can't read answer alike, so the mint can't
        // probe page ids. Losing access later simply leaves the key with
        // nothing to reach: the rule is evaluated per request.
        let Some(acl) = state.acl.as_ref() else {
            return internal();
        };
        match acl.access(ctx.workspace_id, root, target.id, None).await {
            Ok(Some(access)) if access.can_read() => {}
            Ok(_) => return json_err(StatusCode::NOT_FOUND, "doc.not_found", ""),
            Err(e) => {
                tracing::error!(error = ?e, "api key root acl");
                return internal();
            }
        }
```

  - `NewApiKey { …, root_doc_id: body.root_doc_id }`; map `Err(ApiKeyStoreError::RootRejected)` to `json_err(StatusCode::NOT_FOUND, "doc.not_found", "")` (the race of a purge between check and insert).
  - `KeyResponse` gets `root_doc_id: Option<Uuid>` and `root_title: Option<String>`; `CreatedResponse` gets `root_doc_id: Option<Uuid>`. `From<ApiKey>` sets `root_title: None`.
  - `list_keys`: after loading `keys`, if any has a root, load `acl.visibility(ctx.workspace_id, ctx.user_id, None)` once; for each key with a root that is `readable` for the viewer, fetch the title with `state.docs.get(root)` and set `root_title`. Unreadable or missing → `None`.

- [ ] **Step 4: Run to verify**

Run: `cargo nextest run -p knot-server --test api_keys_integration`
Expected: all PASS, old tests included.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server
git commit -m "feat(api-keys): mint, list and authenticate subtree-scoped keys (S10)"
```

---

### Task 5: Refuse workspace-wide routes and top-level destinations

**Files:**
- Modify: `crates/knot-server/src/auth/scope.rs` (`SUBTREE_DENIED`, `SUBTREE_OK`, `subtree_denied()`, `scope_mw`, tests)
- Modify: `crates/knot-server/src/routes/api/docs.rs:300-315` (`require_editable_destination`)
- Modify: `crates/knot-server/src/duplicate.rs:238-250` (the `None` arm)
- Test: `crates/knot-server/tests/api_keys_integration.rs`

**Interfaces:**
- Consumes: `AuthContext.root_doc_id`.
- Produces: `pub const SUBTREE_DENIED: &[&str]`, `pub const SUBTREE_OK: &[&str]`, `pub fn subtree_denied() -> Response` (the `403 api_key.subtree_denied` body), used by `docs.rs` and `duplicate.rs`.

- [ ] **Step 1: Write the failing tests**

In `scope.rs` `mod tests`:

```rust
    #[test]
    fn subtree_tables_have_their_rows() {
        assert_eq!(SUBTREE_DENIED.len(), 10);
        assert!(SUBTREE_DENIED.iter().all(|p| !SUBTREE_OK.contains(p)));
    }

    /// S10: every `/api` route is classified for subtree keys. A new route
    /// fails here until it is added to SUBTREE_DENIED or SUBTREE_OK.
    #[test]
    fn every_api_route_is_classified_for_subtree_keys() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut routes = Vec::new();
        collect_route_paths(&src, &mut routes);
        assert!(routes.len() > 40, "route scan found too little: {routes:?}");
        let unclassified: Vec<_> = routes
            .iter()
            .filter(|p| !SUBTREE_DENIED.contains(&p.as_str()) && !SUBTREE_OK.contains(&p.as_str()))
            .collect();
        assert!(unclassified.is_empty(), "classify for subtree keys: {unclassified:?}");
    }

    /// The first string argument of every `.route(` call under `dir`, when
    /// it starts with `/api/`.
    fn collect_route_paths(dir: &std::path::Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_route_paths(&path, out);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (at, _) in text.match_indices(".route(") {
                let rest = text[at + ".route(".len()..].trim_start();
                let Some(rest) = rest.strip_prefix('"') else { continue };
                let literal = &rest[..rest.find('"').unwrap()];
                if literal.starts_with("/api/") && !out.iter().any(|p| p == literal) {
                    out.push(literal.to_string());
                }
            }
        }
    }
```

In `api_keys_integration.rs`:

```rust
/// S10: every SUBTREE_DENIED path names a real route and refuses a subtree
/// key with every scope, whatever the method.
#[tokio::test(flavor = "multi_thread")]
async fn every_subtree_denied_route_refuses_a_subtree_key() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let root = matrix_doc(&app, &s, "root").await;
    let key = subtree_key(&app, &s, &root, &ALL_SCOPES).await;
    let id = uuid::Uuid::new_v4().to_string();
    for template in knot_server::auth::scope::SUBTREE_DENIED {
        let uri = template.replace("{id}", &id);
        for method in ["GET", "POST", "PATCH", "DELETE"] {
            let body = (method != "GET").then(|| serde_json::json!({}));
            let (st, body) = guards_bearer(&app, &key, method, &uri, body).await;
            if st == StatusCode::METHOD_NOT_ALLOWED {
                continue;
            }
            assert_eq!(st, StatusCode::FORBIDDEN, "{method} {template}: {body}");
            assert!(
                body["error"]["code"] == "api_key.subtree_denied"
                    || body["error"]["code"] == "api_key.session_required",
                "{method} {template}: {body}"
            );
        }
    }
    // An unscoped key still reaches them, as before.
    let unscoped = matrix_key(&app, &s, &["read"]).await;
    let (st, body) = guards_bearer(&app, &unscoped, "GET", "/api/workspace/templates", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
}

#[tokio::test(flavor = "multi_thread")]
async fn subtree_key_cannot_reach_the_top_level_or_move_its_root() {
    let state = state_with_owner("owner@example.com", "ownerpass1").await;
    let app = router_with_state(state.clone());
    let s = login(&app, "owner@example.com", "ownerpass1").await;
    let top = matrix_doc(&app, &s, "top").await;
    let root = child_doc(&app, &s, &top, "root").await;
    let child = child_doc(&app, &s, &root, "child").await;
    let key = subtree_key(&app, &s, &root, &ALL_SCOPES).await;
    let denied = |st: StatusCode, body: &serde_json::Value| {
        st == StatusCode::FORBIDDEN && body["error"]["code"] == "api_key.subtree_denied"
    };

    let (st, body) = guards_bearer(&app, &key, "POST", "/api/docs",
        Some(serde_json::json!({"title": "top-level"}))).await;
    assert!(denied(st, &body), "create at top level: {st} {body}");
    let (st, body) = guards_bearer(&app, &key, "POST", &format!("/api/docs/{child}/move"),
        Some(serde_json::json!({"parent_id": null}))).await;
    assert!(denied(st, &body), "move to top level: {st} {body}");
    let (st, body) = guards_bearer(&app, &key, "POST", &format!("/api/docs/{root}/move"),
        Some(serde_json::json!({"parent_id": top}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "move root under its parent: {body}");
    let (st, body) = guards_bearer(&app, &key, "POST", &format!("/api/docs/{root}/duplicate"),
        Some(serde_json::json!({}))).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "duplicate root next to itself: {body}");
    let (st, body) = guards_bearer(&app, &key, "POST", &format!("/api/docs/{child}/duplicate"),
        Some(serde_json::json!({}))).await;
    assert!(st.is_success(), "duplicate inside: {body}");

    // Search and tasks stay inside too.
    let (st, body) = guards_bearer(&app, &key, "GET", "/api/search?q=top", None).await;
    assert_eq!(st, StatusCode::OK, "{body}");
    assert!(!body.to_string().contains(&top), "search leaked the parent: {body}");
}
```

A duplicate of a top-level page needs a separate root at top level: add to the same test a second key rooted at `top` and assert `POST /api/docs/{top}/duplicate` is `denied`.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo nextest run -p knot-server subtree`
Expected: compile error (`SUBTREE_DENIED` missing).

- [ ] **Step 3: Implement `scope.rs`**

```rust
/// S10: routes a key with a scope root may not call, whatever the method:
/// they are workspace-wide, or they change the user's own state. Keyed by
/// the exact axum `MatchedPath`. Routes that filter through the doc ACL
/// narrow on their own and are listed in [`SUBTREE_OK`].
pub const SUBTREE_DENIED: &[&str] = &[
    // Who is in the workspace is not page content.
    "/api/workspace/members",
    "/api/workspace/members/{id}",
    // Workspace-wide by definition.
    "/api/workspace/templates",
    "/api/workspace/export",
    "/api/workspace/import",
    "/api/workspace/archive",
    // The user's inbox: doc-less notifications pass the visibility filter,
    // and marking read checks no ACL.
    "/api/notifications",
    "/api/notifications/unread_count",
    "/api/notifications/read",
    // Changes the user's own state.
    "/api/whats-new/seen",
];
```

`SUBTREE_OK`: every other path the scan finds, one comment line per group, e.g.:

```rust
/// S10: routes open to a subtree key, each with why that is safe. Every
/// `/api` route is in exactly one of the two tables (tested).
pub const SUBTREE_OK: &[&str] = &[
    // Resolved through the doc ACL, which masks everything outside the root.
    "/api/docs/{id}",
    "/api/docs/{id}/move",
    // … every /api/docs/{…}, /api/blobs/{…}, /api/boards/{…}, /api/short-links/{code} path
    // Filtered by Visibility; every row has a doc.
    "/api/docs",
    "/api/search",
    "/api/workspace/tasks",
    // Workspace id, slug, name, role, retention: no page content.
    "/api/workspace",
    // Key management is SESSION_ONLY anyway.
    "/api/auth/api-keys",
    "/api/auth/api-keys/{id}",
    // No page content.
    "/api/version",
    "/api/release-notes",
    "/api/whats-new",
    "/api/healthz",
    "/api/readyz",
];
```

Run the scan test once to get the exact list it finds and fill the `…` with those paths — no wildcards; each path literally.

```rust
/// The answer to a subtree key on a workspace-wide route or destination.
pub fn subtree_denied() -> Response {
    json_err(
        StatusCode::FORBIDDEN,
        "api_key.subtree_denied",
        "this API key is limited to one page and its subpages",
    )
}
```

In `scope_mw`, right after the `session_only` refusal:

```rust
    let has_root = req
        .extensions()
        .get::<AuthContext>()
        .is_some_and(|ctx| ctx.root_doc_id.is_some());
    if has_root && matched.is_some_and(|m| SUBTREE_DENIED.contains(&m)) {
        tracing::info!(%key_id, %method, %route, required = %"subtree", "api key refused outside its subtree");
        return subtree_denied();
    }
```

(`use super::context::AuthContext;` or the crate path the module already uses.) Extend the module doc (line 1-14) by one line: "Routes in [`SUBTREE_DENIED`] refuse every key that has a scope root (S10)."

- [ ] **Step 4: Implement the top-level checks**

`docs.rs::require_editable_destination`, `None` arm:

```rust
        None if ctx.root_doc_id.is_some() => Err(crate::auth::scope::subtree_denied()),
```

placed before the existing `None` arm. `duplicate.rs` around line 241, the `None` arm of `match source.parent_id`: the same check first (`ctx.root_doc_id.is_some()` → `Err` carrying `subtree_denied()`; follow how that function's other refusals are returned — it has an error enum with a `Response`-carrying variant for `require_editable_parent`, reuse it). Both get the comment `// S10: a top-level page is outside every subtree.`

- [ ] **Step 5: Run to verify**

Run: `cargo nextest run -p knot-server`
Expected: all PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/knot-server
git commit -m "feat(api-keys): refuse workspace-wide routes and top-level pages for subtree keys (S10)"
```

---

### Task 6: Web UI

**Files:**
- Modify: `web/src/features/apiKeys/apiKeys.api.ts`
- Modify: `web/src/features/apiKeys/ApiKeysSection.tsx`
- Test: `web/src/features/apiKeys/ApiKeysSection.test.tsx`

**Interfaces:**
- Consumes: Task 4 HTTP shapes; `docsApi.list()` (`features/docs/docs.api.ts`), `buildTree`, `flattenVisible` (`features/docs/tree.ts`).
- Produces: `CreateApiKeyInput.rootDocId?: string | null`; `ApiKey.root_doc_id`, `ApiKey.root_title`.

- [ ] **Step 1: Write the failing tests** — in `ApiKeysSection.test.tsx`, follow the file's existing mocking of `apiKeysApi` / fetch. Add:

```tsx
it("sends the chosen page as root_doc_id", async () => {
  // Arrange the docs list mock to return two pages: "Project" (id p1) and
  // its child "Notes" (id p2, parent_id p1), in the shape docsApi.list() parses.
  render(<ApiKeysSection />);
  await user.type(screen.getByTestId("api-key-name"), "agent");
  await user.selectOptions(await screen.findByTestId("api-key-root"), "p1");
  await user.click(screen.getByTestId("api-key-submit"));
  expect(createSpy).toHaveBeenCalledWith(expect.objectContaining({ rootDocId: "p1" }));
});

it("shows the root page of a subtree key, or that it is hidden", async () => {
  // Arrange the key list mock with two keys: one with root_doc_id "p1" and
  // root_title "Project", one with root_doc_id "p9" and root_title null.
  render(<ApiKeysSection />);
  const chips = await screen.findAllByTestId("api-key-root-chip");
  expect(chips[0]).toHaveTextContent("Project");
  expect(chips[0].closest("a")).toHaveAttribute("href", "/doc/p1");
  expect(chips[1]).toHaveTextContent("a page you can't see");
});

it("explains a root the key's user can't read", () => {
  expect(apiKeyErrorMessage("doc.not_found")).toBe("You or the bot can't read that page, or it no longer exists.");
});
```

Use the exact names (`createSpy`, `user`, mock helpers) that the existing tests in this file define.

- [ ] **Step 2: Run to verify they fail**

Run: `cd web && pnpm test -- ApiKeysSection`
Expected: FAIL (`api-key-root` not found).

- [ ] **Step 3: Implement**

`apiKeys.api.ts`: `apiKeySchema` adds `root_doc_id: v.fallback(v.nullable(v.string()), null)` and `root_title: v.fallback(v.nullable(v.string()), null)`; `createdApiKeySchema` adds `root_doc_id: v.fallback(v.nullable(v.string()), null)`; `CreateApiKeyInput` adds `/** Limit the key to this page and its subpages (S10). */ rootDocId?: string | null;`; `create` sends `root_doc_id: input.rootDocId ?? undefined` (omit when unset).

`ApiKeysSection.tsx`:

```tsx
  const docs = useQuery({ queryKey: ["docs"], queryFn: () => docsApi.list() });
  const pageRows =
    docs.data && "ok" in docs.data
      ? flattenVisible(buildTree(docs.data.ok.filter((d) => !d.archived)).roots, new Set())
      : [];
  const [rootDocId, setRootDocId] = useState<string>("");
```

(`docs.data.ok` must be the `Doc[]` the tree uses — check how `DocTree.tsx` unwraps `list.data` and copy it. Include `buildTree(..).shared` rows after `roots` the same way.)

Form, after the expiry `<label>`:

```tsx
        <label className="grid gap-1 text-[13px] text-fg-muted">
          Pages
          <select
            data-testid="api-key-root"
            value={rootDocId}
            onChange={(e) => setRootDocId(e.target.value)}
            className="h-9 px-2 rounded border border-border bg-bg text-fg text-sm"
          >
            <option value="">All pages</option>
            {pageRows.map((r) => (
              <option key={r.id} value={r.id}>{"  ".repeat(r.depth) + r.title}</option>
            ))}
          </select>
          {rootDocId && <span>The key sees only this page and its subpages.</span>}
        </label>
```

The mutation passes `rootDocId: rootDocId || null`; `onSuccess` resets it with `setRootDocId("")`. In the key row, inside the scopes `<span className="flex flex-wrap gap-1">`, after the scope chips:

```tsx
                      {k.root_doc_id && (
                        k.root_title !== null ? (
                          <Link to={`/doc/${k.root_doc_id}`} className="px-1.5 py-0.5 rounded bg-muted text-fg text-[11px] underline">
                            <span data-testid="api-key-root-chip">{k.root_title}</span>
                          </Link>
                        ) : (
                          <span data-testid="api-key-root-chip" className="px-1.5 py-0.5 rounded bg-muted text-fg-muted text-[11px]">
                            a page you can't see
                          </span>
                        )
                      )}
```

(`Link` from `react-router-dom`, as other features import it.) `apiKeyErrorMessage` gets `case "doc.not_found": return "You or the bot can't read that page, or it no longer exists.";` and `case "api_key.subtree_denied": return "This key is limited to one page and its subpages.";`. Update the intro sentence to "…Each key is limited to the scopes you pick, and optionally to one page and its subpages."

- [ ] **Step 4: Run to verify**

Run: `cd web && pnpm test -- ApiKeysSection && pnpm exec tsc --noEmit`
Expected: PASS, no type errors.

- [ ] **Step 5: Commit**

```bash
git add web/src/features/apiKeys
git commit -m "feat(web): limit an API key to one page and its subpages"
```

---

### Task 7: e2e and documentation

**Files:**
- Modify: `e2e/flows/api-keys.spec.ts`
- Modify: `ARCHITECTURE.md` (request lifecycle paragraph on API keys)
- Modify: `CHANGELOG.md` (Unreleased → Added)
- Modify: `.claude/skills/knot-api/SKILL.md`
- Modify: `docs/superpowers/specs/2026-11-06-api-keys-and-bots-design.md:13-16`

- [ ] **Step 1: Write the e2e flow** — in `api-keys.spec.ts`, following the file's existing login/mint helpers:

```ts
test("a key limited to a page can't reach a page outside it", async ({ page, request }) => {
  // As the owner: create pages "Inside" and "Outside" through the UI or API helpers the file uses.
  // Settings → API keys: name "scoped", Pages = "Inside", Create; read the revealed key.
  const inside = await request.get(`/api/docs/${insideId}`, { headers: { authorization: `Bearer ${key}` } });
  expect(inside.status()).toBe(200);
  const outside = await request.get(`/api/docs/${outsideId}`, { headers: { authorization: `Bearer ${key}` } });
  expect(outside.status()).toBe(404);
  await expect(page.getByTestId("api-key-root-chip")).toHaveText("Inside");
});
```

Replace the comments with the exact helper calls the existing tests in that file use.

- [ ] **Step 2: Run e2e**

Run: `make e2e` (needs `make dev` running, see `e2e/README` or the Makefile).
Expected: PASS.

- [ ] **Step 3: Update the docs**

- `ARCHITECTURE.md`: after the sentence about `SESSION_ONLY`, add: "A key can carry a scope root (`api_keys.root_doc_id`, S10). The root is part of the ACL rule in `knot-docs`, so outside the root's subtree every page answers like one without access (404), listings, search and tasks narrow by themselves, and `SUBTREE_DENIED` in `auth/scope.rs` refuses workspace-wide routes (members, templates, export/import/archive, notifications) with 403 `api_key.subtree_denied`, as does creating, moving or duplicating to the top level. A test makes every `/api` route choose between `SUBTREE_DENIED` and `SUBTREE_OK`."
- `CHANGELOG.md` Unreleased/Added: "API keys can be limited to one page and its subpages (Settings → API keys → Pages). Such a key can't call workspace-wide endpoints."
- `knot-api` skill: document `root_doc_id` on create, `root_doc_id`/`root_title` on list, and the two error codes.
- Old spec line 13-16: replace "per-document scoping of a key (a key acts with the full page access of its user, narrowed only by HTTP scope)" with "per-document scoping of a key (since added as subtree scoping, see [2026-11-13-subtree-scoped-api-keys-design.md](2026-11-13-subtree-scoped-api-keys-design.md))".

- [ ] **Step 4: Full verification**

Run: `make lint && make test`
Expected: clippy, fmt, tsc clean; all Rust and web tests PASS.

- [ ] **Step 5: Commit**

```bash
git add e2e ARCHITECTURE.md CHANGELOG.md .claude/skills/knot-api/SKILL.md docs/superpowers/specs/2026-11-06-api-keys-and-bots-design.md
git commit -m "docs: subtree-scoped API keys"
```
