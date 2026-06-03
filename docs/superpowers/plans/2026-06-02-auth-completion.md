# Auth Completion Implementation Plan (Plan 8)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the auth gaps left after Plans 3 + 6 so the workspace is independently usable by non-owner users. Specifically: (a) invited members can sign in (currently only the workspace owner created via `/auth/setup` has a password); (b) authenticated users can change their own password; (c) the existing OIDC flow against Dex is end-to-end verified with a passing e2e test.

**Architecture:** Three small, independent surfaces, each one server endpoint + one frontend touch + one or more tests.

- **Invite-with-password** — extend `POST /api/workspace/members` to accept an optional `password` field. If the email is new, create the user with the password hash; if it already exists, behave like today (just add to the workspace). The MembersPage invite form gains a password input.
- **Change-password** — new `POST /auth/password` taking `{ current, new }`, verifying the current hash and rotating to a new Argon2id hash + bumping `password_changed_at`. SettingsPage gains a form.
- **OIDC end-to-end** — Plans 3 already wired `/auth/oidc/login` + `/auth/oidc/callback` against Dex; add an e2e that drives the static dev user through the full handshake and asserts session creation. Identify and fix any drift along the way.

**Tech Stack (mostly unchanged):** axum 0.7 + sqlx + argon2 (already in `knot-auth`), Dex (already running in `deploy/compose/dev.yml`), TanStack Query mutations on the frontend.

**Predecessors:**
- Plan 3 (auth, outcome at `docs/superpowers/research/2026-06-01-plan3-outcome.md`)
- Plan 6 (frontend shell, outcome at `docs/superpowers/research/2026-06-02-plan6-outcome.md`, HEAD `244b6df`)

**Spec coverage:**

| Spec section | Tasks |
|---|---|
| §5.2 Invite-with-password — owner-set initial credentials | T1, T2, T6 |
| §5.3 Self-serve password change | T3, T4, T7 |
| §5.4 OIDC: Dex round-trip lands a sid cookie | T8, T9 |
| §6.3 Error envelope codes: `auth.invalid_credentials`, `auth.weak_password`, `auth.password_reuse` | T1, T3 |
| §10.5 Frontend uses typed API + CSRF | T5, T6, T7 |
| Plan 6 deferred: real two-user e2e once invite-with-password lands | T10 |

**Out of scope for this plan** (intentionally deferred):

- **Password reset via email token** — needs SMTP + email templates; defer until a deployment story exists. (Workspace owner can re-invite-with-password as a workaround.)
- **`must_change_password` flag** — for v0.1 the owner shares the initial password verbally / out-of-band; first-login forcing is a UX nicety, not a security boundary.
- **OIDC user provisioning into a non-default workspace** — Plan 3's OIDC code assumes the first workspace; this is still fine for v0.1.
- **OIDC group / role sync** — manual member adds only.
- **Rate-limiting on password endpoints** — leave at the existing global limits; tighten in a separate hardening plan.

---

## File map

```
crates/knot-server/
├── src/routes/auth/
│   ├── local.rs                                 (modify) +password change handler + route
│   └── mod.rs                                   (unchanged) re-exports
├── src/routes/api/workspace.rs                  (modify) invite handler accepts optional password
├── tests/auth_password.rs                       (new) change-password integration tests
├── tests/workspace_invite_password.rs           (new) invite-with-password integration tests
└── tests/auth_oidc_e2e.rs                       (new, optional) thin OIDC handshake guard

web/
├── src/auth/session.api.ts                      (modify) +authApi.changePassword
├── src/features/workspace/workspace.api.ts      (modify) inviteWithPassword
├── src/features/workspace/MembersPage.tsx       (modify) password field on invite form
├── src/features/workspace/SettingsPage.tsx      (modify) change-password form section
└── src/features/workspace/SettingsPage.test.tsx (new, optional) vitest smoke

e2e/flows/
├── invite-password.spec.ts                      (new) owner invites Bob with password; Bob signs in
├── change-password.spec.ts                      (new) authenticated user rotates own password
├── oidc.spec.ts                                 (new) Dex round-trip lands a session
└── two-users-converge.spec.ts                   (rewrite) real two-user convergence using invite
```

---

## Conventions

- Server endpoints follow the existing §6.3 error envelope. Codes used here: `auth.invalid_credentials`, `auth.weak_password`, `auth.password_reuse`, `auth.session_required`, `workspace.forbidden`.
- `knot_auth::hash_password()` + `knot_auth::verify_password()` are the only acceptable hashing API surfaces. Don't call argon2 directly from route handlers.
- Password policy: min 8 chars (matches the existing setup endpoint). Anything below → `auth.weak_password`.
- All new integration tests use `knot_test_support::fresh_db()` per the project's mandatory test-infra constraint (see `feedback_test_infra_no_testcontainers.md`).
- Frontend mutations follow the Plan 6 pattern: `useMutation` + invalidate queries + Zustand toast on error.
- OIDC e2e talks directly to Dex's web UI on port 5556. Dex's static dev user is preconfigured (verify the user/password in `deploy/compose/dex/config.yaml` before writing the test).

---

## Task overview

| # | Title | LOC ≈ |
|---|---|---|
| 1 | Server: POST /auth/password handler (TDD) | 140 |
| 2 | Server tests: change-password (4 cases) | 200 |
| 3 | Server: POST /api/workspace/members accepts optional password | 90 |
| 4 | Server tests: invite-with-password | 160 |
| 5 | Frontend: authApi.changePassword + workspaceApi.invite signature | 50 |
| 6 | Frontend: MembersPage password field on invite form | 80 |
| 7 | Frontend: SettingsPage change-password form | 130 |
| 8 | OIDC: verify Dex round-trip; identify drift if any | research |
| 9 | OIDC e2e: Dex static user → /auth/oidc/login → session | 150 |
| 10 | e2e: rewrite two-users-converge for real two-user editing | 180 |
| 11 | e2e: invite-password.spec + change-password.spec | 200 |
| 12 | Outcome doc + tag | 0 |

---

## Task 1: Server: POST /auth/password handler

**Files:**
- Modify: `crates/knot-server/src/routes/auth/local.rs`

- [ ] **Step 1: Write the failing handler signature + minimal route registration**

Edit `crates/knot-server/src/routes/auth/local.rs`. After the existing `logout` / `session` handlers, add:

```rust
#[derive(Deserialize)]
struct PasswordChange {
    current: String,
    new: String,
}

async fn change_password(
    State(state): State<AppState>,
    req: axum::extract::Request,
    Json(body): Json<PasswordChange>,
) -> axum::response::Response {
    let Some(ctx) = req.extensions().get::<crate::auth::AuthContext>().cloned() else {
        return error(StatusCode::UNAUTHORIZED, "auth.session_required", "session required");
    };
    if body.new.chars().count() < 8 {
        return error(StatusCode::BAD_REQUEST, "auth.weak_password", "password too short");
    }
    if body.new == body.current {
        return error(StatusCode::BAD_REQUEST, "auth.password_reuse", "new password must differ");
    }
    let pool = match state.pool.as_ref() {
        Some(p) => p,
        None => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "no pool"),
    };
    let row = sqlx::query!(
        "SELECT password_hash FROM users WHERE id = $1",
        ctx.user_id
    )
    .fetch_one(pool)
    .await;
    let row = match row {
        Ok(r) => r,
        Err(_) => return error(StatusCode::UNAUTHORIZED, "auth.invalid_credentials", "user gone"),
    };
    let hash = match row.password_hash {
        Some(h) => h,
        None => return error(StatusCode::BAD_REQUEST, "auth.invalid_credentials", "no local password (OIDC-only user)"),
    };
    if !knot_auth::verify_password(&hash, &body.current).unwrap_or(false) {
        return error(StatusCode::UNAUTHORIZED, "auth.invalid_credentials", "current password wrong");
    }
    let new_hash = match knot_auth::hash_password(&body.new) {
        Ok(h) => h,
        Err(_) => return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "hash failed"),
    };
    if let Err(_) = sqlx::query!(
        "UPDATE users SET password_hash = $1, password_changed_at = NOW() WHERE id = $2",
        new_hash,
        ctx.user_id
    )
    .execute(pool)
    .await
    {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "update failed");
    }
    StatusCode::NO_CONTENT.into_response()
}
```

> **If** `password_changed_at` column does NOT exist on `users`, drop it from the UPDATE. Check first: `grep -rn "password_changed_at\|password_hash" crates/knot-auth/ migrations/`. If you need to add the column, add a new migration `migrations/0xxx_users_password_changed_at.sql` with `ALTER TABLE users ADD COLUMN password_changed_at TIMESTAMPTZ;` rather than editing an existing migration.

> **`error()` helper:** Check the existing `local.rs` for the project's `error()` / envelope helper. Reuse it. If none, copy the pattern from `setup.rs` or `login` in the same file. Don't invent a new envelope shape.

- [ ] **Step 2: Register the route**

Find the existing `Router::new().route("/auth/login", ...)` chain (`local.rs:42-44` per the grep earlier) and add:

```rust
.route("/auth/password", post(change_password))
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo check --workspace
```

Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/knot-server/
git commit -m "feat(knot-server): POST /auth/password — change own password"
```

---

## Task 2: Server tests for change-password

**Files:**
- Create: `crates/knot-server/tests/auth_password.rs`

- [ ] **Step 1: Write the integration tests**

Create `crates/knot-server/tests/auth_password.rs`:

```rust
//! Integration tests for POST /auth/password.
//!
//! Uses `knot_test_support::fresh_db()` (NO testcontainers — see
//! feedback memory).

use axum::{Router, body::Body};
use http::{Request, StatusCode, header};
use knot_server::AppState;
use knot_test_support::fresh_db;
use tower::ServiceExt;

async fn build_app() -> (Router, sqlx::PgPool) {
    let db = fresh_db().await;
    let mut state = AppState::with_pool(db.pool.clone());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let app = knot_server::router(state);
    (app, db.pool)
}

async fn setup_owner(app: &Router) -> (String, String) {
    let res = app.clone().oneshot(
        Request::post("/auth/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"email":"o@e.com","password":"first-password","display_name":"O"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let cookies = res.headers().get_all(header::SET_COOKIE).iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>();
    let sid = cookies.iter().find(|c| c.starts_with("sid=")).unwrap().clone();
    let csrf = cookies.iter().find(|c| c.starts_with("csrf=")).unwrap().clone();
    (sid, csrf)
}

#[tokio::test]
async fn happy_path_changes_hash_and_lets_user_log_in_with_new() {
    let (app, _pool) = build_app().await;
    let (sid, csrf) = setup_owner(&app).await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();

    let res = app.clone().oneshot(
        Request::post("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"current":"first-password","new":"second-password"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // Old password no longer works.
    let res = app.clone().oneshot(
        Request::post("/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"email":"o@e.com","password":"first-password"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // New password works.
    let res = app.clone().oneshot(
        Request::post("/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"email":"o@e.com","password":"second-password"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn wrong_current_password_returns_401_invalid_credentials() {
    let (app, _pool) = build_app().await;
    let (sid, csrf) = setup_owner(&app).await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();
    let res = app.oneshot(
        Request::post("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"current":"wrong","new":"correct-horse-battery"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.invalid_credentials");
}

#[tokio::test]
async fn weak_new_password_returns_400_weak_password() {
    let (app, _pool) = build_app().await;
    let (sid, csrf) = setup_owner(&app).await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();
    let res = app.oneshot(
        Request::post("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"current":"first-password","new":"short"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.weak_password");
}

#[tokio::test]
async fn reusing_current_password_returns_400_password_reuse() {
    let (app, _pool) = build_app().await;
    let (sid, csrf) = setup_owner(&app).await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();
    let res = app.oneshot(
        Request::post("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"current":"first-password","new":"first-password"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(res.into_body(), 4096).await.unwrap();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["code"], "auth.password_reuse");
}

#[tokio::test]
async fn unauthenticated_returns_401_session_required() {
    let (app, _pool) = build_app().await;
    let res = app.oneshot(
        Request::post("/auth/password")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"current":"x","new":"correct-horse-battery"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
```

> **Note on the existing test helpers:** The codebase may already have a helper for the setup → cookies dance (`setup_owner` style). Check `crates/knot-server/tests/` for shared modules and reuse if present (DRY).

- [ ] **Step 2: Run tests**

```bash
make compose.up
cargo nextest run -p knot-server --test auth_password
```

Expected: 5 / 5 pass. If `auth.password_reuse` returns 400 with the wrong code, fix the handler order in Task 1 (the reuse check must run before the verify check, so we don't leak that the password was correct via timing).

- [ ] **Step 3: Commit**

```bash
git add crates/knot-server/tests/auth_password.rs
git commit -m "test(knot-server): change-password integration tests"
```

---

## Task 3: Server: POST /api/workspace/members accepts optional password

**Files:**
- Modify: `crates/knot-server/src/routes/api/workspace.rs`

- [ ] **Step 1: Locate the invite handler**

```bash
grep -n "fn invite\|members.*post\|InviteMember" crates/knot-server/src/routes/api/workspace.rs
```

The existing invite handler creates a `workspace_members` row for an existing user (per Plan 4). The current flow assumes the invited user already exists (e.g., from a prior OIDC login or another workspace). For Plan 8, the owner-typed password lets us create the user too.

- [ ] **Step 2: Extend the request payload**

Edit the InviteBody struct (whatever it's called in the file). Add an optional `password: Option<String>` field.

- [ ] **Step 3: Update the handler**

Pseudocode (adapt to existing structure):

```rust
async fn invite(/* ... */ Json(body): Json<InviteBody>) -> Response {
    // ...existing role check (owner only)...
    let user_id = match find_user_by_email(&body.email).await {
        Some(id) => {
            // existing-user path — unchanged from Plan 4
            id
        }
        None => match body.password.as_deref() {
            Some(pw) if pw.chars().count() >= 8 => {
                let hash = knot_auth::hash_password(pw)?;
                create_user(email = body.email, hash = Some(hash), display_name = body.email).await?
                // display_name defaults to the email until they edit it themselves
            }
            Some(_) => return error(BAD_REQUEST, "auth.weak_password", "password too short"),
            None => return error(NOT_FOUND, "workspace.user_not_found", "user not found"),
        },
    };
    insert_workspace_member(workspace_id, user_id, body.role).await?;
    StatusCode::NO_CONTENT.into_response()
}
```

The "user-not-found-and-no-password" path keeps the existing behavior (so Plan 4's tests still pass).

- [ ] **Step 4: Verify it compiles**

```bash
cargo check --workspace
```

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/src/routes/api/workspace.rs
git commit -m "feat(knot-server): invite-with-password creates a fresh user if missing"
```

---

## Task 4: Server tests for invite-with-password

**Files:**
- Create: `crates/knot-server/tests/workspace_invite_password.rs`

- [ ] **Step 1: Write three scenarios**

```rust
use axum::{Router, body::Body};
use http::{Request, StatusCode, header};
use knot_server::AppState;
use knot_test_support::fresh_db;
use tower::ServiceExt;

async fn build_app_with_owner() -> (Router, sqlx::PgPool, String, String) {
    let db = fresh_db().await;
    let mut state = AppState::with_pool(db.pool.clone());
    state.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    let app = knot_server::router(state);
    let res = app.clone().oneshot(
        Request::post("/auth/setup")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"email":"o@e.com","password":"owner-hunter22","display_name":"O"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::CREATED);
    let cookies = res.headers().get_all(header::SET_COOKIE).iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>();
    let sid = cookies.iter().find(|c| c.starts_with("sid=")).unwrap().clone();
    let csrf = cookies.iter().find(|c| c.starts_with("csrf=")).unwrap().clone();
    (app, db.pool, sid, csrf)
}

#[tokio::test]
async fn invite_with_password_creates_user_and_member() {
    let (app, pool, sid, csrf) = build_app_with_owner().await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();
    let res = app.clone().oneshot(
        Request::post("/api/workspace/members")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"email":"bob@e.com","role":"editor","password":"bob-hunter22"}"#))
            .unwrap(),
    ).await.unwrap();
    assert!(matches!(res.status(), StatusCode::NO_CONTENT | StatusCode::CREATED));

    // User row exists with a password hash.
    let row = sqlx::query!(
        "SELECT password_hash FROM users WHERE email = $1",
        "bob@e.com"
    ).fetch_one(&pool).await.unwrap();
    assert!(row.password_hash.is_some());

    // Bob can log in.
    let res = app.oneshot(
        Request::post("/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"email":"bob@e.com","password":"bob-hunter22"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn invite_existing_user_without_password_still_works() {
    // Verifies Plan 4 behavior is unchanged when no password is supplied.
    let (app, pool, sid, csrf) = build_app_with_owner().await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();
    // Pre-create Bob without a password (e.g., via direct SQL — simulates an OIDC-provisioned user).
    sqlx::query!(
        "INSERT INTO users (email, display_name) VALUES ($1, $2)",
        "bob@e.com",
        "Bob"
    ).execute(&pool).await.unwrap();
    let res = app.oneshot(
        Request::post("/api/workspace/members")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"email":"bob@e.com","role":"editor"}"#))
            .unwrap(),
    ).await.unwrap();
    assert!(matches!(res.status(), StatusCode::NO_CONTENT | StatusCode::CREATED));
}

#[tokio::test]
async fn invite_unknown_email_without_password_returns_404() {
    let (app, _pool, sid, csrf) = build_app_with_owner().await;
    let csrf_token = csrf.trim_start_matches("csrf=").to_string();
    let res = app.oneshot(
        Request::post("/api/workspace/members")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, format!("{sid}; {csrf}"))
            .header("X-CSRF-Token", &csrf_token)
            .body(Body::from(r#"{"email":"unknown@e.com","role":"editor"}"#))
            .unwrap(),
    ).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
```

- [ ] **Step 2: Run + commit**

```bash
cargo nextest run -p knot-server --test workspace_invite_password
git add crates/knot-server/tests/workspace_invite_password.rs
git commit -m "test(knot-server): invite-with-password integration tests"
```

---

## Task 5: Frontend API client additions

**Files:**
- Modify: `web/src/auth/session.api.ts`
- Modify: `web/src/features/workspace/workspace.api.ts`

- [ ] **Step 1: session.api**

Add to `authApi`:

```ts
async changePassword(current: string, next: string) {
  return apiFetch<void>("/auth/password", {
    method: "POST",
    body: { current, new: next },
  });
},
```

- [ ] **Step 2: workspace.api**

Update the `invite` method signature to accept an optional password:

```ts
invite(email: string, role: "owner" | "editor" | "viewer", password?: string) {
  return apiFetch<void>("/api/workspace/members", {
    method: "POST",
    body: password ? { email, role, password } : { email, role },
  });
},
```

- [ ] **Step 3: Verify + commit**

```bash
cd web && pnpm tsc && pnpm lint
git add web/
git commit -m "feat(web): authApi.changePassword + invite-with-password"
```

---

## Task 6: Frontend: MembersPage password field

**Files:**
- Modify: `web/src/features/workspace/MembersPage.tsx`

- [ ] **Step 1: Add a password input**

Inside the invite form, after the role select, add:

```tsx
<input
  data-testid="invite-password"
  type="password"
  value={invitePassword}
  onChange={(e) => setInvitePassword(e.target.value)}
  placeholder="Initial password (optional)"
  minLength={8}
  style={{ padding: 6 }}
/>
```

Add the corresponding `useState`: `const [invitePassword, setInvitePassword] = useState("");`.

Change the mutation to pass the password:

```ts
const invite = useMutation({
  mutationFn: async () =>
    workspaceApi.invite(inviteEmail, inviteRole, invitePassword || undefined),
  onSuccess: async (r) => {
    if ("error" in r) {
      const msg =
        r.error.code === "workspace.user_not_found"
          ? "User not found. Add a password to create the account."
          : r.error.code === "auth.weak_password"
            ? "Password must be at least 8 characters."
            : "Invite failed.";
      notify("error", msg);
      return;
    }
    setInviteEmail("");
    setInvitePassword("");
    await qc.invalidateQueries({ queryKey: ["members"] });
  },
});
```

- [ ] **Step 2: Verify + commit**

```bash
cd web && pnpm tsc && pnpm lint
git add web/
git commit -m "feat(web): MembersPage invite form supports initial password"
```

---

## Task 7: Frontend: SettingsPage change-password form

**Files:**
- Modify: `web/src/features/workspace/SettingsPage.tsx`

- [ ] **Step 1: Add a section above the Sign out button**

```tsx
import { useMutation } from "@tanstack/react-query";
import { useState } from "react";
// existing imports...

// inside SettingsPage:
const [pwCurrent, setPwCurrent] = useState("");
const [pwNew, setPwNew] = useState("");
const [pwBusy, setPwBusy] = useState(false);
const [pwError, setPwError] = useState<string | null>(null);
const [pwOk, setPwOk] = useState(false);

const changePw = useMutation({
  mutationFn: async () => authApi.changePassword(pwCurrent, pwNew),
  onMutate: () => { setPwBusy(true); setPwError(null); setPwOk(false); },
  onSettled: () => { setPwBusy(false); },
  onSuccess: (r) => {
    if ("error" in r) {
      setPwError(
        r.error.code === "auth.invalid_credentials" ? "Current password is wrong."
          : r.error.code === "auth.weak_password" ? "New password must be at least 8 characters."
          : r.error.code === "auth.password_reuse" ? "New password must differ from current."
          : "Couldn't change password.",
      );
      return;
    }
    setPwCurrent(""); setPwNew(""); setPwOk(true);
  },
});

// in the JSX, ABOVE the Sign out button:
<section data-testid="change-password" style={{ marginBottom: 24 }}>
  <h2>Change password</h2>
  <form
    onSubmit={(e) => { e.preventDefault(); changePw.mutate(); }}
    style={{ display: "grid", gap: 8, maxWidth: 320 }}
  >
    <input
      data-testid="pw-current"
      type="password"
      placeholder="Current password"
      value={pwCurrent}
      onChange={(e) => setPwCurrent(e.target.value)}
      required
      style={{ padding: 6 }}
    />
    <input
      data-testid="pw-new"
      type="password"
      placeholder="New password (≥ 8 chars)"
      value={pwNew}
      onChange={(e) => setPwNew(e.target.value)}
      required
      minLength={8}
      style={{ padding: 6 }}
    />
    {pwError && <p data-testid="pw-error" style={{ color: "#b00020" }}>{pwError}</p>}
    {pwOk && <p data-testid="pw-ok" style={{ color: "#1f7a1f" }}>Password updated.</p>}
    <button data-testid="pw-submit" type="submit" disabled={pwBusy} style={{ padding: 8 }}>
      {pwBusy ? "Updating…" : "Update password"}
    </button>
  </form>
</section>
```

- [ ] **Step 2: Verify + commit**

```bash
cd web && pnpm tsc && pnpm lint
git add web/
git commit -m "feat(web): SettingsPage — change password form"
```

---

## Task 8: OIDC drift check (research only)

**Files:** none (research)

- [ ] **Step 1: Manually drive the flow**

```bash
make compose.up
KNOT_DATABASE_URL="postgres://knot:knot@localhost:5432/knot" \
  KNOT_SESSION_KEY="test-key-32-bytes-aaaaaaaaaaaaaa" \
  KNOT_OIDC_ENABLED=1 \
  KNOT_OIDC_ISSUER_URL="http://localhost:5556" \
  KNOT_OIDC_CLIENT_ID="knot-dev" \
  KNOT_OIDC_CLIENT_SECRET="dev-secret" \
  KNOT_OIDC_REDIRECT_URL="http://localhost:3000/auth/oidc/callback" \
  cargo run --bin knot-server
```

In another terminal:
- `curl -v http://localhost:3000/auth/oidc/login` — expect a 302 to the Dex authorize URL.
- Open the redirect URL in a browser → Dex login screen → use the static dev user from `deploy/compose/dex/config.yaml`.
- Should land on `/auth/oidc/callback?...` which sets sid + csrf cookies and redirects to `/`.
- `curl -b cookiejar.txt http://localhost:3000/auth/session` — expect 200 with the user payload.

If anything fails, note the failure mode and either:
- Fix it inline (small) and commit `fix(knot-server): OIDC <whatever>`
- OR escalate as a blocker and replan.

If everything works, no commit needed; proceed to Task 9.

- [ ] **Step 2: Verify required env vars**

Confirm the exact env-var names the code reads:

```bash
grep -rn "KNOT_OIDC\|oidc_issuer\|oidc_client" crates/knot-config/ crates/knot-server/src/routes/auth/oidc.rs
```

Match these in Task 9's playwright env.

---

## Task 9: OIDC e2e test

**Files:**
- Create: `e2e/flows/oidc.spec.ts`
- Modify: `e2e/playwright.config.ts` — start the server with OIDC env vars (or note this as a precondition for THIS spec only and skip in CI without it)

- [ ] **Step 1: Read Dex static user**

```bash
grep -E "email|hash|userID|username" deploy/compose/dex/config.yaml
```

Note the credentials. Dex bcrypts passwords in config — the *cleartext* needs to be known (or in the dev config it's likely the same as in Plan 3's setup notes). If it's only the bcrypt hash, you may need to (a) recompute (`htpasswd -bnBC 10 "" "your-password" | tr -d ':\n'`) or (b) update the dev config with a known cleartext.

- [ ] **Step 2: Write the e2e**

```ts
import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations", "audit_events", "doc_markdown_cache",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

test.beforeAll(reset);

test.skip(!process.env.KNOT_OIDC_ENABLED, "OIDC not enabled in this test run");

test("Dex round-trip lands a session", async ({ page }) => {
  await page.goto("/login");
  await page.click("text=Sign in with SSO");
  // Dex login UI
  await page.waitForURL(/:5556\//);
  await page.fill('input[name="login"]', "admin@example.com"); // adjust per Dex config
  await page.fill('input[name="password"]', "password");
  await page.click("button[type=submit]");
  // Dex may ask to grant access — auto-approve in dev config; otherwise click Grant.
  if (await page.locator("text=Grant Access").isVisible({ timeout: 1000 }).catch(() => false)) {
    await page.click("text=Grant Access");
  }
  // Back at knot, authenticated.
  await page.waitForURL("http://localhost:5173/", { timeout: 15_000 });
  // Sidebar visible → we're in.
  await expect(page.getByTestId("sidebar")).toBeVisible();
});
```

- [ ] **Step 3: Run only when OIDC env is set**

```bash
cd e2e
KNOT_OIDC_ENABLED=1 \
  KNOT_OIDC_ISSUER_URL=http://localhost:5556 \
  KNOT_OIDC_CLIENT_ID=knot-dev \
  KNOT_OIDC_CLIENT_SECRET=dev-secret \
  KNOT_OIDC_REDIRECT_URL=http://localhost:3000/auth/oidc/callback \
  pnpm playwright test oidc.spec.ts
```

(The webServer env in playwright.config doesn't currently include these. Pass them via the shell so the spawned `cargo run` inherits them, OR edit `playwright.config.ts` to include them.)

- [ ] **Step 4: Commit**

```bash
git add e2e/
git commit -m "test(e2e): Dex OIDC round-trip — lands a session"
```

---

## Task 10: Rewrite two-users-converge with real two-user editing

**Files:**
- Rewrite: `e2e/flows/two-users-converge.spec.ts`

- [ ] **Step 1: Implement**

```ts
import { execSync } from "node:child_process";

import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations", "audit_events", "doc_markdown_cache",
    "doc_snapshots", "doc_updates", "document_grants", "documents",
    "sessions", "workspace_members", "users", "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

test.beforeAll(reset);

test("two users editing concurrently converge on both screens", async ({ browser }) => {
  // Alice sets up the workspace + creates a doc + invites Bob with password.
  const aliceCtx = await browser.newContext();
  const alice = await aliceCtx.newPage();
  await alice.goto("/setup");
  await alice.getByTestId("setup-email").fill("alice@example.com");
  await alice.getByTestId("setup-display-name").fill("Alice");
  await alice.getByTestId("setup-password").fill("alice-hunter22");
  await alice.getByTestId("setup-submit").click();
  await alice.getByTestId("new-doc").click();
  await alice.waitForURL(/\/doc\/.+/);
  const docUrl = alice.url();

  await alice.goto("/members");
  await alice.getByTestId("invite-email").fill("bob@example.com");
  await alice.getByTestId("invite-role").selectOption("editor");
  await alice.getByTestId("invite-password").fill("bob-hunter22");
  await alice.getByTestId("invite-submit").click();
  await expect(alice.getByTestId("member-")).toBeVisible({ timeout: 5_000 }).catch(() => null);
  // (Member row appears with a generated user_id; we won't lock down the row testid.)

  // Bob signs in in a separate context.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@example.com");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();
  await bob.waitForURL(/\/$/);

  // Both navigate to the doc.
  await alice.goto(docUrl);
  await bob.goto(docUrl);

  // Both reach connected.
  await expect(alice.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });
  await expect(bob.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  // Type from each side.
  const aliceEditor = alice.locator("[data-testid='editor-host'] .ProseMirror");
  const bobEditor = bob.locator("[data-testid='editor-host'] .ProseMirror");

  await aliceEditor.click();
  await alice.keyboard.type("Hello from Alice. ");

  await bobEditor.click();
  await bob.keyboard.type("And from Bob.");

  // Both screens see both contributions.
  await expect.poll(() => aliceEditor.textContent(), { timeout: 5_000 }).toMatch(/Hello from Alice\./);
  await expect.poll(() => aliceEditor.textContent(), { timeout: 5_000 }).toMatch(/And from Bob\./);
  await expect.poll(() => bobEditor.textContent(), { timeout: 5_000 }).toMatch(/Hello from Alice\./);
  await expect.poll(() => bobEditor.textContent(), { timeout: 5_000 }).toMatch(/And from Bob\./);

  await aliceCtx.close();
  await bobCtx.close();
});
```

- [ ] **Step 2: Run + commit**

```bash
cd e2e && pnpm playwright test two-users-converge.spec.ts
git add e2e/
git commit -m "test(e2e): two-users-converge — real two-user editing under invite-with-password"
```

---

## Task 11: Invite + change-password e2e

**Files:**
- Create: `e2e/flows/invite-password.spec.ts`
- Create: `e2e/flows/change-password.spec.ts`

- [ ] **Step 1: invite-password.spec.ts**

```ts
import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = ["acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots","doc_updates","document_grants","documents","sessions","workspace_members","users","workspaces"].join(", ");
  execSync(`docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`, { cwd: "..", stdio: "pipe" });
}

test.beforeAll(reset);

test("owner invites Bob with password; Bob signs in", async ({ browser }) => {
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("owner@example.com");
  await owner.getByTestId("setup-display-name").fill("Owner");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();

  await owner.goto("/members");
  await owner.getByTestId("invite-email").fill("bob@example.com");
  await owner.getByTestId("invite-role").selectOption("editor");
  await owner.getByTestId("invite-password").fill("bob-hunter22");
  await owner.getByTestId("invite-submit").click();
  await expect(owner.locator("[data-testid^='member-']").nth(1)).toBeVisible({ timeout: 5_000 });

  // Bob signs in in a fresh context.
  const bobCtx = await browser.newContext();
  const bob = await bobCtx.newPage();
  await bob.goto("/login");
  await bob.getByTestId("login-email").fill("bob@example.com");
  await bob.getByTestId("login-password").fill("bob-hunter22");
  await bob.getByTestId("login-submit").click();
  await bob.waitForURL(/\/$/);
  await expect(bob.getByTestId("sidebar")).toBeVisible();
});
```

- [ ] **Step 2: change-password.spec.ts**

```ts
import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = ["acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots","doc_updates","document_grants","documents","sessions","workspace_members","users","workspaces"].join(", ");
  execSync(`docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`, { cwd: "..", stdio: "pipe" });
}

test.beforeAll(reset);

test("user changes own password; old password fails, new succeeds", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("u@example.com");
  await page.getByTestId("setup-display-name").fill("U");
  await page.getByTestId("setup-password").fill("first-password");
  await page.getByTestId("setup-submit").click();

  await page.goto("/settings");
  await page.getByTestId("pw-current").fill("first-password");
  await page.getByTestId("pw-new").fill("second-password");
  await page.getByTestId("pw-submit").click();
  await expect(page.getByTestId("pw-ok")).toBeVisible({ timeout: 5_000 });

  // Sign out → old password fails.
  await page.getByTestId("logout").click();
  await page.waitForURL(/\/login/);
  await page.getByTestId("login-email").fill("u@example.com");
  await page.getByTestId("login-password").fill("first-password");
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("login-error")).toBeVisible();

  // New password works.
  await page.getByTestId("login-password").fill("second-password");
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/$/);
  await expect(page.getByTestId("sidebar")).toBeVisible();
});
```

- [ ] **Step 3: Run + commit**

```bash
cd e2e && pnpm playwright test invite-password.spec.ts change-password.spec.ts
git add e2e/
git commit -m "test(e2e): invite-with-password + change-password flows"
```

---

## Task 12: Plan 8 outcome doc

**Files:**
- Create: `docs/superpowers/research/2026-06-0X-plan8-outcome.md`

- [ ] **Step 1: Write the outcome doc**

Use the same template as `docs/superpowers/research/2026-06-02-plan6-outcome.md`:

- Status (GO / GO_WITH_CONCERNS / BLOCKED)
- Gates table (cargo test, cargo clippy, pnpm tsc/lint/test, playwright)
- What landed (commits oldest to newest)
- What's still deferred (password reset via email; must_change_password; OIDC role sync)
- Carryforward for Plan 9 (or next plan)

- [ ] **Step 2: Commit**

```bash
git add docs/
git commit -m "docs: Plan 8 outcome"
```

---

## Self-review checklist

Before declaring Plan 8 complete:

- [ ] `cargo test --workspace` green (new tests included)
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean
- [ ] `pnpm tsc`, `pnpm lint`, `pnpm test` (vitest) green
- [ ] `pnpm playwright test` green for all specs except `oidc.spec.ts` if OIDC env not set (skipped is OK)
- [ ] Manual smoke: invite a user with password → sign in as them → edit a doc → change their own password → log out → old password fails → new succeeds
- [ ] Manual smoke: kick off the Dex flow → land a session
