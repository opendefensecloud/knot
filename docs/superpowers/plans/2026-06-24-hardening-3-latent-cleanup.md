# Hardening Branch 3 — Latent Correctness & Cleanup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`).

**Goal:** Fix the latent-correctness and defense-in-depth items from the review (no behavior regressions; mostly small, isolated fixes).

**Tech Stack:** Rust (knot-docs, knot-crdt, knot-storage, knot-server, knot-auth) + TS frontend. Tests: `cargo nextest run -p <crate>` (dev-compose Postgres; never testcontainers); `cd web && pnpm test`/`tsc`. `cargo clippy -- -D warnings`.

**Spec:** `docs/superpowers/specs/2026-06-24-security-robustness-hardening-design.md` (Branch 3).

**Preconditions:** dev-compose Postgres healthy.

---

## Task 1: ACL cache key includes workspace_id

**Files:** `crates/knot-docs/src/cache.rs`. Test: same crate (add a `#[cfg(test)]` or extend an existing cache/acl test).

Context: `AclCache.inner: Cache<(Uuid, Uuid), Option<WorkspaceRole>>` keyed `(doc_id, user_id)` (`cache.rs:20,51`), but the resolved value depends on `workspace_id` (the tenancy guard in `acl::resolve`). Latent cross-tenant poisoning for multi-workspace users.

- [ ] **Step 1:** Change the key type to `(Uuid, Uuid, Uuid)` = `(workspace_id, doc_id, user_id)`:
  - `inner: Cache<(Uuid, Uuid, Uuid), Option<WorkspaceRole>>` (`:20`).
  - in `effective_role`: `let key = (workspace_id, doc_id, user_id);` (`:51`).
  - in `evict_doc`'s `invalidate_entries_if` predicate: it currently matches on the `doc_id` position of the 2-tuple; update it to match the **middle** element of the 3-tuple (`move |k, _v| k.1 == doc_id`). Read the closure and fix the tuple index.
- [ ] **Step 2:** Add a test proving per-workspace isolation: two workspaces, the same `(doc_id, user_id)` but resolving via different `workspace_id` returns independent cached values (i.e. a value cached under ws A is not returned for ws B). Mirror the existing acl/cache test setup in the crate. Run `cargo nextest run -p knot-docs` → PASS.
- [ ] **Step 3:** clippy clean. Commit `git add crates/knot-docs/src/cache.rs` msg "fix(acl): key the role cache by workspace_id (cross-tenant isolation)".

---

## Task 2: Share-token revoke is doc-scoped

**Files:** `crates/knot-storage/src/share_tokens.rs` (`revoke`), `crates/knot-server/src/routes/api/shares.rs` (revoke handler). Test: `crates/knot-server/tests/shares_integration.rs`.

Context: `revoke(share_id)` runs `UPDATE share_tokens SET revoked_at=NOW() WHERE id=$1` (`share_tokens.rs:138`); the handler `require_owner(doc_id)` then `shares.revoke(share_id)` (`shares.rs:133`) — but never checks the token belongs to `doc_id`. An owner of any doc can revoke an arbitrary token by pairing their `doc_id` with a victim `share_id`.

- [ ] **Step 1: Failing test** — in `shares_integration.rs`, create a share token on doc A (owner), then as owner of a *different* doc B attempt to revoke A's token via `DELETE /api/docs/{B}/shares/{tokenA}` (use the real route shape — read the router); assert it does NOT revoke (404), and A's token still resolves. Run → FAIL.
- [ ] **Step 2:** Change the store method to `revoke(&self, share_id: Uuid, doc_id: Uuid)` with SQL `... WHERE id = $1 AND doc_id = $2` returning the affected-row count (or `RETURNING id`); the handler returns 404 when nothing was revoked. Update the handler to pass `doc_id` and map zero-rows → 404. Read both files for exact signatures/return types and adjust callers/tests.
- [ ] **Step 3:** `cargo nextest run -p knot-server --test shares_integration` → PASS (incl. existing revoke→410 test). clippy clean. Commit msg "fix(shares): scope token revoke to its doc (close cross-doc IDOR)".

---

## Task 3: Grant-inheritance CTE depth cap

**Files:** `crates/knot-storage/src/grant_store.rs` (`list_inherited`, recursive CTE ~`:106`).

Context: the parent-chain CTE has no depth bound; a reintroduced cycle would loop in Postgres (ACL-resolution DoS).

- [ ] **Step 1:** Add a depth column + cap to the recursive arm, mirroring the heal migration's bounded approach:
  - base: `SELECT id, parent_id, 0 AS depth FROM documents WHERE id = $2 AND workspace_id = $1`
  - recursive: `SELECT d.id, d.parent_id, c.depth + 1 FROM documents d JOIN chain c ON d.id = c.parent_id WHERE d.workspace_id = $1 AND c.depth < 10000`
  Keep the rest of the query identical. (If `depth` isn't selected by the outer query, it's only used in the recursive guard — fine.)
- [ ] **Step 2:** `cargo nextest run -p knot-storage --test grants` (and any grant_store tests) → PASS unchanged. clippy clean. Commit msg "fix(grants): bound the inheritance CTE depth (defense-in-depth)".

---

## Task 4: OIDC `"always"` requires email_verified

**Files:** `crates/knot-auth/src/oidc.rs` (~`:244`, the `"always"` branch). Test: same file's `#[cfg(test)]`.

Context: `"domain"` policy requires `email_verified` (`oidc.rs:223`); `"always"` sets `allow = true` unconditionally.

- [ ] **Step 1:** In the `"always"` arm, gate on the verified-email signal the same way `"domain"` does (read how `"domain"` reads `email_verified` from the claims and reuse it): `allow = email_verified;` (or the crate's equivalent). Add a unit test: `"always"` with `email_verified=false` → not provisioned; with `true` → provisioned. Mirror the existing domain-policy test (`oidc.rs` has one at ~`:326`).
- [ ] **Step 2:** `cargo nextest run -p knot-auth` → PASS. clippy clean. Commit msg "fix(oidc): require verified email for auto-provision=always".

---

## Task 5: Hoist the mention regex to `LazyLock`

**Files:** `crates/knot-server/src/routes/api/comments.rs` (~`:131`).

- [ ] **Step 1:** Replace the per-call `Regex::new(...)` in `extract_mentions` with a module-level `static MENTION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| regex::Regex::new(<same pattern>).expect("valid mention regex"));` and use `MENTION_RE.captures_iter(...)` (etc.). Keep behavior identical.
- [ ] **Step 2:** `cargo nextest run -p knot-server --test comments_integration` → PASS. clippy clean. Commit msg "perf(comments): compile the mention regex once".

---

## Task 6: Registry `acquire` graceful close + `inflight` prune

**Files:** `crates/knot-crdt/src/registry.rs`, `crates/knot-crdt/src/board_registry.rs`, and the WS shims `crates/knot-server/src/room.rs` (`serve`) + `crates/knot-server/src/board_room_shim.rs` (`serve`).

Context: `acquire` does `.expect("bus subscribe")` / `.expect("hydrate")` (`registry.rs:82,94`); a transient DB/bus blip panics the per-connection WS task. Also `inflight: DashMap<Uuid, Arc<Mutex<()>>>` entries are never removed (slow leak).

- [ ] **Step 1: Make `acquire` fallible.** Change `Rooms::acquire`/`BoardRooms::acquire` to return `Result<Arc<...>, EngineError>` (or `Option<...>`): replace the two `.expect(...)` with `?`/`map_err`. After successfully inserting into `map`, REMOVE the `inflight` entry for that id (`self.inflight.remove(&doc_id);`) so it doesn't accumulate.
- [ ] **Step 2: Update the shims.** In `room::serve` and `board_room_shim::serve`, the `rooms.acquire(...)` call now returns `Result`/`Option`; on `Err`/`None`, log and return (close the socket cleanly) instead of unwrapping. Read both `serve` fns and adjust.
- [ ] **Step 3: Fix any other `acquire` callers** (e.g. tests, `notify_doc_comments`/`revoke_all_for_doc` use `map.get`, not acquire — unaffected). Build the workspace; fix call sites until green.
- [ ] **Step 4:** `cargo nextest run -p knot-crdt -p knot-server` → PASS (existing room/board/convergence tests still green). clippy clean. Commit msg "fix(crdt): acquire returns Result (graceful WS close); prune inflight map".

---

## Task 7: Frontend defense-in-depth

**Files:** `web/src/features/editor/nodes/MermaidCodeBlock.tsx` (~`:196`), `web/src/features/.../ExcalidrawModal.tsx` (the `importLibrary` fetch).

- [ ] **Step 1: Mermaid** — wrap the rendered SVG through the existing `sanitizeSvg` (from `web/src/lib/sanitize.ts`) before `dangerouslySetInnerHTML={{ __html: ... }}`: `__html: sanitizeSvg(svg)`. Import `sanitizeSvg`.
- [ ] **Step 2: Excalidraw library host** — in `ExcalidrawModal.tsx` `importLibrary`, before `fetch(new URL(libraryUrl))`, validate the host: only allow `libraries.excalidraw.com` (e.g. `const u = new URL(libraryUrl); if (u.host !== "libraries.excalidraw.com") { notify error; return; }`). Read the function for the exact variable names + how it surfaces errors.
- [ ] **Step 3:** `cd web && pnpm tsc --noEmit && pnpm test` → PASS. (Add a small unit test if `sanitizeSvg`/host-check is easily unit-testable; otherwise rely on tsc + existing tests.) Commit msg "fix(web): sanitize mermaid SVG; restrict excalidraw library host".

---

## Task 8: Remove stale deny.toml ignore

**Files:** `deny.toml` (~`:14`).

- [ ] **Step 1:** Remove the `RUSTSEC-2025-0111` ignore line (testcontainers was removed; cargo-deny warns it no longer matches). Run `cargo deny check` → `advisories ok` with no "advisory-not-detected" warning. Commit msg "chore(deny): drop stale testcontainers advisory ignore".

---

## Task 9: Full verification
- [ ] `cargo nextest run --workspace` → all pass.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- [ ] `cargo deny check` → ok, no warnings.
- [ ] `cd web && pnpm test && pnpm tsc --noEmit` → pass.

---

## Self-Review notes
- **Spec coverage (Branch 3):** ACL cache key (T1) ✓; acquire graceful close + inflight prune (T6) ✓; share revoke scoping (T2) ✓; grant CTE cap (T3) ✓; OIDC email_verified (T4) ✓; LazyLock regex (T5) ✓; mermaid + excalidraw (T7) ✓; deny.toml (T8) ✓.
- **No behavior regressions intended:** each task keeps existing tests green; new tests cover the cross-tenant cache, the cross-doc revoke, and OIDC always+unverified.
- Order is independent; T6 is the only one touching multiple crates — land it carefully.
