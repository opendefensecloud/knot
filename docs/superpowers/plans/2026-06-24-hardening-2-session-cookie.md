# Hardening Branch 2 — Session & Cookie Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Breach-resilient sessions — keyed at-rest hashing, invalidation on password change, mandatory session secret, and constant-time login.

**Architecture:** Store `HMAC-SHA256(session_key, token)` as `sessions.id` (keying done in the auth layer, which reads `state.session_key` per request — the store is built before the key is set, so it must not hold the key). Add `SessionStore::delete_for_user`; call it on password change and re-mint the current session. Require `KNOT_SESSION_KEY` in every env. Run Argon2 unconditionally on login to kill the existing-user timing oracle.

**Tech Stack:** Rust — knot-auth (hmac/sha2), knot-storage, knot-server, knot-config. Tests: `cargo nextest run -p <crate>` (dev-compose Postgres; never testcontainers). `cargo clippy -- -D warnings`.

**Spec:** `docs/superpowers/specs/2026-06-24-security-robustness-hardening-design.md` (Branch 2). Note: the `cookie_secure` cookie wiring already landed in Branch 1, so it is NOT in this branch.

**Preconditions:** dev-compose Postgres healthy.

---

## File Structure
- Modify: `crates/knot-auth/src/csrf.rs` (or `lib.rs`) — `hash_session_id` HMAC helper.
- Modify: `crates/knot-storage/src/session_store.rs` — `delete_for_user` trait method + impl.
- Modify: `crates/knot-server/src/auth/session_loader.rs` — `SessionDeps.session_key`; hash on lookup/touch.
- Modify: `crates/knot-server/src/lib.rs` — `session_deps()` passes `session_key`.
- Modify: `crates/knot-server/src/routes/auth/local.rs` — hash on create/delete; password-change invalidation + re-mint; login dummy-hash.
- Modify: `crates/knot-config/src/lib.rs` — require `session_key` in all envs.
- Test: `crates/knot-storage/tests/sessions.rs`; `crates/knot-server/tests/auth_password_integration.rs` / `auth_local_integration.rs`.

---

## Task 1: `SessionStore::delete_for_user`

**Files:** Modify `crates/knot-storage/src/session_store.rs`; test in `crates/knot-storage/tests/sessions.rs`.

Context: `SessionStore` trait (`:29`) has `create/find_active/touch/delete`, all by `id: &[u8]`. `PgSessionStore { pool }`. `sessions` table has a `user_id` column.

- [ ] **Step 1: Failing test** — read `tests/sessions.rs` for its `setup()`/helpers, then append:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn delete_for_user_removes_all_their_sessions() {
    let (store, user_id, ws_id) = /* setup as the file does */;
    let exp = chrono::Utc::now() + chrono::Duration::hours(1);
    store.create(b"tok-a", user_id, ws_id, exp, None, None).await.unwrap();
    store.create(b"tok-b", user_id, ws_id, exp, None, None).await.unwrap();
    assert!(store.find_active(b"tok-a").await.unwrap().is_some());

    store.delete_for_user(user_id).await.unwrap();

    assert!(store.find_active(b"tok-a").await.unwrap().is_none());
    assert!(store.find_active(b"tok-b").await.unwrap().is_none());
}
```

Run: `cargo nextest run -p knot-storage --test sessions delete_for_user` → FAIL (no method).

- [ ] **Step 2: Add the trait method** (after `delete`):
```rust
    /// Delete every session belonging to a user (e.g. on password change).
    async fn delete_for_user(&self, user_id: Uuid) -> Result<(), SessionStoreError>;
```

- [ ] **Step 3: Implement on `PgSessionStore`**:
```rust
    async fn delete_for_user(&self, user_id: Uuid) -> Result<(), SessionStoreError> {
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
```

- [ ] **Step 4:** `cargo nextest run -p knot-storage --test sessions` → PASS; clippy clean. Commit:
```bash
git add crates/knot-storage/src/session_store.rs crates/knot-storage/tests/sessions.rs
git commit -m "feat(sessions): SessionStore::delete_for_user"
```

---

## Task 2: Keyed at-rest session hashing

**Files:** `crates/knot-auth/src/csrf.rs` (helper); `crates/knot-server/src/auth/session_loader.rs`; `crates/knot-server/src/lib.rs`; `crates/knot-server/src/routes/auth/local.rs`. Test: `crates/knot-server/tests/auth_local_integration.rs`.

Context: the token round-trips as raw bytes today: login `sessions.create(token.as_bytes(), ...)` (`local.rs:130`); loader `find_active(decoded.as_bytes())` + `touch(&id)` (`session_loader.rs:28,43`); logout `delete(token.as_bytes())` (`local.rs:156`). We insert an HMAC at each store boundary. `knot-auth/src/csrf.rs` already imports `hmac`/`sha2`. `SessionDeps { sessions, workspaces }` (`session_loader.rs:16`); `session_deps()` builds it (`lib.rs:171`). `AppState` has `session_key: Vec<u8>`.

- [ ] **Step 1: Add the HMAC helper** in `crates/knot-auth/src/csrf.rs` (re-export from `knot_auth` if the crate root re-exports csrf items; otherwise reference as `knot_auth::csrf::hash_session_id`):
```rust
/// Derive the at-rest session id from the raw cookie token, keyed by the
/// server secret. Storing this (not the raw token) means a leaked `sessions`
/// table is useless without the externally-held key, and rotating the key
/// invalidates every session.
pub fn hash_session_id(key: &[u8], token: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(token);
    mac.finalize().into_bytes().to_vec()
}
```
Add a unit test: `hash_session_id(k, t)` is deterministic, 32 bytes, and differs for a different key.

- [ ] **Step 2: Thread `session_key` into `SessionDeps`** — add `pub session_key: Vec<u8>,` to the struct (`session_loader.rs:16`), and in `session_deps()` (`lib.rs:172`) add `session_key: self.session_key.clone(),`.

- [ ] **Step 3: Hash on lookup + touch** in `session_loader.rs` — change the find/touch to hash first:
```rust
        && let Ok(decoded) = SessionToken::decode(&token)
        && let id = knot_auth::csrf::hash_session_id(&deps.session_key, decoded.as_bytes())
        && let Ok(Some(s)) = deps.sessions.find_active(&id).await
```
(If the `let id = ...` chained binding doesn't fit the existing `&&`-let chain, compute `id` just above the chain and use it in both `find_active(&id)` and the fire-and-forget `touch(&id)`. The `id` captured for `touch` must be this hashed value, not the raw token.)

- [ ] **Step 4: Hash on create + delete** in `local.rs`:
- login (`:130`): before `sessions.create(...)`, compute `let sid_hash = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());` and pass `&sid_hash` as the first arg instead of `token.as_bytes()`. The cookie still uses the raw `token` (`build_session_cookies(&state, &token)`) — unchanged.
- logout (`:156`): `sessions.delete(&knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes())).await`.

- [ ] **Step 5: Test the round-trip + key-sensitivity** — in `auth_local_integration.rs`, add a test that logs in, then asserts the session cookie still authenticates a follow-up request (round-trips through the HMAC), AND that the stored `sessions.id` is NOT the raw token. Use a direct query: decode the `sid` cookie, and assert `find_active(raw_token_bytes)` returns None but the app still authorizes (because the loader hashes). Concretely:
```rust
// after login, capture the sid cookie value; GET /auth/session with it → 200.
// Then via the store directly: SessionToken::decode(sid).as_bytes() looked up
// raw returns None (it's stored hashed); a GET with the cookie still works.
```
Keep it behavioral where simpler: the key assertion is that login→authenticated-request works end-to-end (proving create/find agree on the hash). Run `cargo nextest run -p knot-server --test auth_local_integration` → PASS.

- [ ] **Step 6:** clippy clean. Commit:
```bash
git add crates/knot-auth/src/csrf.rs crates/knot-server/src/auth/session_loader.rs crates/knot-server/src/lib.rs crates/knot-server/src/routes/auth/local.rs crates/knot-server/tests/auth_local_integration.rs
git commit -m "feat(sessions): store HMAC(secret, token) at rest, keyed by session_key"
```

---

## Task 3: Invalidate sessions on password change

**Files:** `crates/knot-server/src/routes/auth/local.rs` (`change_password` tail ~`:288`). Test: `crates/knot-server/tests/auth_password_integration.rs`.

Context: after the `UPDATE users SET password_hash` succeeds (`local.rs:288-296`), no sessions are invalidated. We delete all the user's sessions then mint a fresh one for the current request so the changer stays logged in while every other (and any stolen) cookie dies.

- [ ] **Step 1: Failing test** — append to `auth_password_integration.rs` (reuse its login + change helpers): log in (capture `sid` cookie A), change the password successfully, then assert: (a) a request with the **old** cookie A is now unauthorized (`GET /auth/session` → 401), and (b) the change_password response set a **new** `sid` cookie that authorizes.

```rust
// 1. login → sid_a. 2. POST /auth/password {current,new} with sid_a + csrf → 204
//    and capture the new Set-Cookie sid_b.
// 3. GET /auth/session with sid_a → 401 (old session killed).
// 4. GET /auth/session with sid_b → 200.
```
Run → FAIL (old cookie still valid; no new cookie).

- [ ] **Step 2: Implement** — replace the change_password tail after the successful UPDATE:
```rust
    // Kill every existing session for this user (logs out other devices and
    // any stolen cookie), then mint a fresh session for the current request.
    if let Some(sessions) = state.sessions.clone() {
        if let Err(e) = sessions.delete_for_user(ctx.user_id).await {
            tracing::error!(error=?e, "change_password: delete_for_user");
            return internal();
        }
        let token = knot_auth::SessionToken::generate();
        let exp = chrono::Utc::now() + chrono::Duration::from_std(crate::auth::cookies::SESSION_TTL).unwrap();
        let sid_hash = knot_auth::csrf::hash_session_id(&state.session_key, token.as_bytes());
        if let Err(e) = sessions
            .create(&sid_hash, ctx.user_id, ctx.workspace_id, exp, None, None)
            .await
        {
            tracing::error!(error=?e, "change_password: re-create session");
            return internal();
        }
        state.throttle.reset(&ip_key);
        state.throttle.reset(&user_key);
        let (sid, csrf) = crate::auth::cookies::build_session_cookies(&state, &token);
        let mut resp = StatusCode::NO_CONTENT.into_response();
        crate::auth::cookies::append_session_cookies(&mut resp, &sid, &csrf);
        return resp;
    }
    state.throttle.reset(&ip_key);
    state.throttle.reset(&user_key);
    StatusCode::NO_CONTENT.into_response()
```
Confirm `ctx.workspace_id` and `ctx.user_id` are in scope (the handler has the `AuthContext`); confirm `SESSION_TTL` import path. Adjust the exact symbol paths to match the file's existing imports (it already imports several `cookies::` items at `local.rs:20`).

- [ ] **Step 3:** `cargo nextest run -p knot-server --test auth_password_integration` → PASS. clippy clean. Commit:
```bash
git add crates/knot-server/src/routes/auth/local.rs crates/knot-server/tests/auth_password_integration.rs
git commit -m "feat(auth): invalidate all sessions on password change; re-mint current"
```

---

## Task 4: Mandatory session secret + constant-time login

**Files:** `crates/knot-config/src/lib.rs` (validate); `crates/knot-server/src/routes/auth/local.rs` (login). Tests: config unit; `auth_local_integration.rs`.

- [ ] **Step 1: Require the secret in all envs** — in `config/src/lib.rs::validate`, replace the production-gated check:
```rust
        if self.session_key.is_empty() {
            return Err(ConfigError::Invalid(
                "KNOT_SESSION_KEY is required (set it in every environment)".into(),
            ));
        }
```
(Remove the `self.env == "production" &&` prefix; keep the existing ≥32-byte check below it.) Add/adjust a config test: `Config { session_key: "".into(), ..Default::default() }.validate()` is `Err` regardless of env. Ensure `.env.example` already sets `KNOT_SESSION_KEY` (it does) and dev/preflight provide it.

- [ ] **Step 2: Login dummy-hash (constant-time)** — in `local.rs`, add near the top:
```rust
/// A fixed Argon2 hash so login runs the (expensive) verify even when the user
/// is absent or has no password, removing the existing-user timing oracle.
fn timing_dummy_hash(hasher: &knot_auth::Hasher) -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| hasher.hash("knot-timing-dummy").unwrap_or_default())
}
```
In `login`, at the two early-return-`invalid_credentials` points where the user is absent (`:96`) or has no `password_hash` (`:106`), run a throwaway verify first:
```rust
        // user not found:
        let _ = state.hasher.verify(timing_dummy_hash(&state.hasher), &body.password);
        return invalid_credentials();
```
and likewise in the `let Some(hash) = user.password_hash ... else { ... }` branch. (The fixed 1s sleep, if present, stays as belt-and-suspenders.)

- [ ] **Step 3:** `cargo nextest run -p knot-config -p knot-server` → PASS (existing login tests still green: unknown-user and wrong-password both `auth.invalid_credentials`). clippy clean. Commit:
```bash
git add crates/knot-config/src/lib.rs crates/knot-server/src/routes/auth/local.rs
git commit -m "feat(auth): require KNOT_SESSION_KEY in all envs; constant-time login"
```

---

## Task 5: Full verification
- [ ] `cargo nextest run -p knot-storage -p knot-server -p knot-config -p knot-auth` → all pass.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` → clean.
- [ ] Manual: `make dev` (with `KNOT_SESSION_KEY` set), log in, reload (session persists via HMAC), change password (other tabs log out, current stays), `psql` the `sessions` table and confirm `id` is not the cookie token.

---

## Self-Review notes
- **Spec coverage (Branch 2):** keyed at-rest hashing (Task 2) ✓; delete_for_user + password-change invalidation (Tasks 1,3) ✓; require secret all-envs (Task 4) ✓; login dummy-hash (Task 4) ✓. (cookie_secure landed in Branch 1.)
- **Keying location:** auth layer (reads `state`/`deps` session_key per request), NOT the store — avoids the store-built-before-key-set ordering issue and supports rotation=global-logout.
- **Naming:** `hash_session_id`, `delete_for_user`, `timing_dummy_hash` consistent across tasks; all three store boundaries (create/find/delete) + touch use the same hash.
- **Deploy note:** first deploy invalidates existing sessions (one-time re-login) — expected.
