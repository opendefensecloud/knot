# Hardening Branch 4 — Test Gaps & Tooling Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`).

**Goal:** Close the security-critical test gaps the review found and add coverage/audit tooling.

**Tech Stack:** Rust integration tests (`fresh_db`, `oneshot`, tokio-tungstenite for WS); Makefile tooling. Tests: `cargo nextest run -p knot-server` (dev-compose Postgres; never testcontainers).

**Spec:** `docs/superpowers/specs/2026-06-24-security-robustness-hardening-design.md` (Branch 4).

**Preconditions:** dev-compose Postgres healthy. If the full suite flakes, run `make db.cleanup` (leftover `t_*` test DBs from prior runs cause Postgres resource pressure).

---

## Task 1: `grants_integration.rs` — owner-gating of the grant API

**Files:** Create `crates/knot-server/tests/grants_integration.rs`.

Context: `PUT/DELETE /api/docs/:id/grants/:principal` require `EffectiveDocRole == Owner` (`routes/api/grants.rs:79,139`) and reject `group:` principals (422, `:99`). No HTTP test exists. Reuse the auth+login helper from `crates/knot-server/tests/docs_integration.rs` (`login_state` builds an owner session + returns sid/csrf) and `workspace_invite_password_integration.rs` for inviting a second (editor/viewer) user.

- [ ] **Step 1:** Write tests (read `docs_integration.rs` `login_state` + the invite flow first, reuse verbatim):
  - **owner can grant:** owner `PUT /api/docs/{doc}/grants/user:{other_uuid}` body `{"role":"editor","inherit":false}` → 204; `GET .../grants` lists it; `DELETE .../grants/user:{other}` → 204.
  - **editor/viewer cannot grant:** as a non-owner member, `PUT` → 403 (`acl.owner_required`/`acl.editor_required` — assert the actual code returned).
  - **group principal rejected:** owner `PUT /api/docs/{doc}/grants/group:eng` → 422 (`grant.group_unsupported` or the real code).
  - **cross-workspace / unknown doc:** `PUT` to a random doc UUID not in the workspace → 404/403 (assert the real status).
  Use the exact route paths from `routes/api/grants.rs` `router()`.
- [ ] **Step 2:** `cargo nextest run -p knot-server --test grants_integration` → PASS. Commit `git add crates/knot-server/tests/grants_integration.rs` msg "test(grants): HTTP owner-gating + group-rejection coverage".

---

## Task 2: HTTP-level doc-move cycle rejection

**Files:** Add to `crates/knot-server/tests/docs_integration.rs` (it already has the login helper + doc-create flow).

Context: the move handler maps `DocStoreError::Cycle` → `409 doc.move_cycle` (`routes/api/docs.rs`), but only the storage layer is tested for cycles.

- [ ] **Step 1:** Add a test: create doc A, create child B under A (via `POST /api/docs` with `parent_id`), then `POST /api/docs/{A}/move` with body moving A under B (its descendant) → assert `409` and error code `doc.move_cycle`. Read the move route's request shape (`MoveRequest { parent_id, after_id, before_id }`).
- [ ] **Step 2:** `cargo nextest run -p knot-server --test docs_integration` → PASS. Commit msg "test(docs): HTTP move-cycle returns 409 doc.move_cycle".

---

## Task 3: OIDC callback state-mismatch rejection

**Files:** Add to an OIDC test (new `crates/knot-server/tests/auth_oidc_integration.rs` or extend an existing one). Note: full token-exchange / auto-provision negative paths need a mock IdP and are already covered by the Dex `e2e/flows/oidc.spec.ts`; here we cover the cheap, valuable unit-level rejection that needs no IdP.

Context: the callback (`routes/auth/oidc.rs`) validates the `state` param against the `oidc_flow` cookie before any exchange; a mismatch returns 400 (`auth.oidc.state_mismatch` or similar — read the handler for the exact code).

- [ ] **Step 1:** Build an app with `state.oidc_enabled = true` and a stub/real OidcClient if required to reach the state check (read the handler — if the state check happens before the client is used, no client is needed). `GET /auth/oidc/callback?code=x&state=wrong` with either no `oidc_flow` cookie or a cookie whose state differs → assert 400 + the real error code. If the handler requires a configured `OidcClient` to even reach the state check, and that can't be constructed in a unit test, instead assert the `GET /auth/oidc/login` 503-when-disabled path (already partially covered) and DOCUMENT that the state-mismatch path is covered by e2e — report which you did.
- [ ] **Step 2:** `cargo nextest run -p knot-server` (the new test) → PASS. Commit msg "test(oidc): callback rejects state mismatch".

---

## Task 4: Viewer cannot write over the collab socket

**Files:** Rewrite `crates/knot-server/tests/convergence.rs` (currently `#[ignore]`d). `tokio-tungstenite` 0.24 is a dev-dep.

Context: `collab_upgrade` computes `can_write = Owner|Editor` and `room::serve` drops inbound `SyncStep2/Update` frames when `!can_write` (`room.rs`). This authz boundary is untested. The existing ignored test has the WS-client scaffolding (bind a TcpListener, `axum::serve(router())`, `connect_async`, build+send a y-sync-update frame) — reuse it, but against a **real authed app**.

- [ ] **Step 1:** Replace the test body:
  - Build an authed AppState: `fresh_db` pool → `AppState::with_pool` + `hasher`/`session_key` (mirror `docs_integration.rs` `login_state`), serve it on a bound port.
  - Create an owner + a **viewer** on the same workspace + a doc (via the stores directly, or HTTP setup + invite). Log each in to get a `sid` cookie.
  - Open a WS to `ws://{addr}/collab/doc/{doc_id}` for each, passing the session cookie in the handshake (tokio-tungstenite: build a `http::Request` with a `Cookie: sid=...` header and `client_async`/`connect_async` with that request). Drain the initial sync.
  - **Viewer write is dropped:** the viewer sends a y-sync-update frame; assert the OWNER connection does NOT receive it within a short window (the server dropped it), and the persisted doc state is unchanged.
  - **Editor/owner write propagates:** the owner sends an update; assert it persists / the viewer receives it (proving the harness works and only the viewer path is gated).
  Remove `#[ignore]`.
- [ ] **Step 2:** `cargo nextest run -p knot-server --test convergence` → PASS. This is timing-sensitive — use generous polling/timeouts (the WS round-trip), not fixed sleeps where avoidable. If, after honest effort, the authed-WS handshake or the negative-assertion proves too flaky to be reliable, keep the EDITOR-propagation half (proves the socket works) and mark only the viewer-negative half `#[ignore]` with a precise comment + report why — but make a real attempt; the boundary test is the point.
- [ ] **Step 3:** Commit `git add crates/knot-server/tests/convergence.rs` msg "test(collab): viewer cannot write over the WS; editor can".

---

## Task 5: Coverage + supply-chain tooling

**Files:** `Makefile`; `web/package.json` (script) or CI note.

- [ ] **Step 1:** Add Makefile targets (don't gate CI — just make them runnable):
```make
.PHONY: coverage.rust
coverage.rust: ## Rust line coverage (needs cargo-llvm-cov)
	cargo llvm-cov --workspace --all-features --summary-only

.PHONY: coverage.web
coverage.web: ## frontend coverage
	cd web && $(PNPM) test -- --coverage

.PHONY: audit.web
audit.web: ## frontend dependency audit
	cd web && $(PNPM) audit --prod
```
(Match the Makefile's existing target style/`$(PNPM)` var. If `cargo-llvm-cov` isn't installed, the target still documents the command — add a comment noting `cargo install cargo-llvm-cov`.)
- [ ] **Step 2:** Verify the targets parse: `make coverage.web` runs (vitest supports `--coverage` via c8/v8 — if it needs a coverage provider dep, add `@vitest/coverage-v8` to web devDependencies and `coverage` config, OR just document it and leave the target). Report what you did. Commit msg "chore(make): coverage + web audit targets".

---

## Task 6: Verification
- [ ] `make db.cleanup` (clear leftover test DBs), then `cargo nextest run -p knot-server` → all pass incl. the new `grants_integration`, `convergence`, docs cycle, oidc tests.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- [ ] `cd web && pnpm test && pnpm tsc --noEmit` → pass.

---

## Self-Review notes
- **Spec coverage (Branch 4):** grants_integration (T1) ✓; viewer-write WS test (T4) ✓; OIDC negative (T3, scoped to the IdP-free state-mismatch path; rest noted as e2e-covered) ✓; HTTP cycle rejection (T2) ✓; coverage + audit tooling (T5) ✓.
- **Risk:** T4 (authed WS test) is the flaky-prone one; the plan allows a documented partial fallback but requires a real attempt at the viewer-negative assertion.
