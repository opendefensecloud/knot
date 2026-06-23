# Login Page Conditional Options Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The login page shows "First-run setup" only when setup is available and "Sign in with SSO" only when OIDC is configured, with SSO promoted to the prominent primary action.

**Architecture:** Add an unauthenticated `GET /auth/config` endpoint returning `{ setup_available, oidc_enabled, password_login_enabled }`. `LoginPage` fetches it on load via TanStack Query and conditionally renders the SSO button (prominent, on top), an "or" divider, the password form (secondary submit when SSO present), and the setup link.

**Tech Stack:** Rust (axum, sqlx) backend; React + TypeScript + TanStack Query + valibot frontend. Backend tests: `cargo nextest run -p knot-server` (uses `knot_test_support::fresh_db` against dev-compose Postgres — do NOT use testcontainers). Frontend tests: `cd web && pnpm test` (vitest + @testing-library/react). Typecheck: `cd web && pnpm tsc --noEmit`.

**Spec:** `docs/superpowers/specs/2026-06-23-login-page-conditional-options-design.md`

**Preconditions:** dev-compose Postgres must be healthy for backend tests (`make compose.up`).

---

## File Structure

- Create: `crates/knot-server/src/routes/auth/config.rs` — the `GET /auth/config` handler + `AuthConfig` response struct + `router()`.
- Modify: `crates/knot-server/src/routes/auth/mod.rs` — declare `pub mod config;` and `.merge(config::router())`.
- Create: `crates/knot-server/tests/auth_config_integration.rs` — endpoint behavior.
- Modify: `web/src/lib/validators.ts` — `AuthConfig` valibot schema + type.
- Modify: `web/src/auth/session.api.ts` — `config()` method.
- Modify: `web/src/features/auth/LoginPage.tsx` — fetch config; conditional render + restyle.
- Create: `web/src/features/auth/LoginPage.test.tsx` — conditional-render unit tests.

---

## Task 1: Backend `GET /auth/config` endpoint

**Files:**
- Create: `crates/knot-server/src/routes/auth/config.rs`
- Modify: `crates/knot-server/src/routes/auth/mod.rs`
- Test: `crates/knot-server/tests/auth_config_integration.rs`

Context: `AppState` has `oidc_enabled: bool` and `users: Option<Arc<dyn ...>>` where the user store exposes `count() -> Result<i64/u64, _>` (see `routes/auth/setup.rs:72` — `users.count().await` returns a numeric count; match its exact integer type when comparing to 0). `AppState::with_pool(pool)` + `router_with_state(state)` build a testable app. The `/auth/*` group is intentionally unauthenticated (see `mod.rs` doc comment).

- [ ] **Step 1: Write the failing test**

Create `crates/knot-server/tests/auth_config_integration.rs`:

```rust
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use knot_auth::Hasher;
use knot_server::{AppState, router_with_state};
use tower::ServiceExt;

async fn fresh_state() -> AppState {
    let pool = knot_test_support::fresh_db().await.pool;
    let mut s = AppState::with_pool(pool);
    s.hasher = Arc::new(Hasher::fast_for_tests());
    s.session_key = b"test-key-32-bytes-aaaaaaaaaaaaaa".to_vec();
    s
}

async fn get_config(app: &axum::Router) -> serde_json::Value {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/auth/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reports_setup_available_until_first_user() {
    let state = fresh_state().await;
    let app = router_with_state(state);

    // Fresh DB: setup available, oidc off, password login on.
    let v = get_config(&app).await;
    assert_eq!(v["setup_available"], true);
    assert_eq!(v["oidc_enabled"], false);
    assert_eq!(v["password_login_enabled"], true);

    // Create the first user via setup.
    let body = serde_json::json!({
        "email": "admin@example.com",
        "password": "hunter2!hunter2",
        "display_name": "Admin",
    })
    .to_string();
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    // Now setup is closed.
    let v = get_config(&app).await;
    assert_eq!(v["setup_available"], false);
}

#[tokio::test(flavor = "multi_thread")]
async fn config_reports_oidc_enabled_from_state() {
    let mut state = fresh_state().await;
    state.oidc_enabled = true;
    let app = router_with_state(state);

    let v = get_config(&app).await;
    assert_eq!(v["oidc_enabled"], true);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo nextest run -p knot-server auth_config`
Expected: FAIL — route `/auth/config` returns 404 (or compile error if `oidc_enabled` field name differs; if so, correct the field reference to match `AppState`).

- [ ] **Step 3: Implement the handler**

Create `crates/knot-server/src/routes/auth/config.rs`:

```rust
//! GET /auth/config — public, pre-login probe.
//!
//! Tells the login page which options to show: whether first-run setup is
//! still available (no users yet), whether OIDC/SSO is configured, and
//! whether password login is enabled. Unauthenticated by design — it only
//! exposes low-sensitivity booleans, and the real gates stay on
//! `POST /auth/setup` and `GET /auth/oidc/login`.

use axum::{Json, Router, extract::State, response::IntoResponse, routing::get};
use serde::Serialize;

use crate::AppState;

#[derive(Serialize)]
pub struct AuthConfig {
    pub setup_available: bool,
    pub oidc_enabled: bool,
    pub password_login_enabled: bool,
}

pub fn router() -> Router<AppState> {
    Router::new().route("/auth/config", get(config))
}

async fn config(State(state): State<AppState>) -> impl IntoResponse {
    // Setup is available only when the user store is reachable and empty.
    let setup_available = match state.users.clone() {
        Some(users) => match users.count().await {
            Ok(n) => n == 0,
            Err(e) => {
                tracing::error!(error=?e, "auth config: user count failed");
                false
            }
        },
        None => false,
    };

    Json(AuthConfig {
        setup_available,
        oidc_enabled: state.oidc_enabled,
        password_login_enabled: true,
    })
}
```

Note: `users.count()` returns a numeric type — `n == 0` must compare against the same integer type it yields (e.g. `0i64`). If the compiler complains, write `n == 0` with the literal inferred, or cast as needed; do NOT change the store API.

- [ ] **Step 4: Register the route**

In `crates/knot-server/src/routes/auth/mod.rs`, add the module declaration alongside the others and merge its router:

```rust
pub mod config;
pub mod local;
pub mod oidc;
pub mod setup;

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(setup::router())
        .merge(config::router())
        .merge(local::router())
        .merge(oidc::router())
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo nextest run -p knot-server auth_config`
Expected: PASS (2 tests).

- [ ] **Step 6: Format + clippy**

Run: `cargo fmt -p knot-server && cargo clippy -p knot-server --all-targets -- -D warnings`
Expected: no changes needed / no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/knot-server/src/routes/auth/config.rs \
        crates/knot-server/src/routes/auth/mod.rs \
        crates/knot-server/tests/auth_config_integration.rs
git commit -m "feat(auth): add GET /auth/config probe for login-page options"
```

---

## Task 2: Frontend config schema + API client

**Files:**
- Modify: `web/src/lib/validators.ts`
- Modify: `web/src/auth/session.api.ts`

- [ ] **Step 1: Add the validator**

In `web/src/lib/validators.ts`, after the `sessionSchema`/`Session` block, add:

```ts
const authConfigSchema = v.object({
  setup_available: v.boolean(),
  oidc_enabled: v.boolean(),
  password_login_enabled: v.boolean(),
});
export type AuthConfig = v.InferOutput<typeof authConfigSchema>;
export const AuthConfig = authConfigSchema;
```

- [ ] **Step 2: Add the API method**

In `web/src/auth/session.api.ts`, update the imports to include the new schema and type, and add a `config()` method. The existing import line is:

```ts
import { type Session, parse, Session as SessionSchema } from "../lib/validators";
```

Replace it with:

```ts
import {
  type AuthConfig,
  AuthConfig as AuthConfigSchema,
  type Session,
  parse,
  Session as SessionSchema,
} from "../lib/validators";
```

Then add this method inside the `authApi` object (e.g. right after `session()`):

```ts
  async config() {
    const r = await apiFetch<unknown>("/auth/config");
    if ("error" in r) return r;
    return { ok: parse(AuthConfigSchema, r.ok) satisfies AuthConfig };
  },
```

- [ ] **Step 3: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add web/src/lib/validators.ts web/src/auth/session.api.ts
git commit -m "feat(web): add authApi.config + AuthConfig validator"
```

---

## Task 3: LoginPage conditional render + prominent SSO

**Files:**
- Modify: `web/src/features/auth/LoginPage.tsx`
- Test: `web/src/features/auth/LoginPage.test.tsx`

Context: `LoginPage` is the default export. It already imports `authApi` from `../../auth/session.api` and `useQueryClient`. It currently renders a footer with two unconditional `<a>` links that this task replaces.

- [ ] **Step 1: Write the failing test**

Create `web/src/features/auth/LoginPage.test.tsx`:

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, cleanup } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { authApi } from "../../auth/session.api";
import LoginPage from "./LoginPage";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function renderLogin() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <LoginPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function mockConfig(cfg: {
  setup_available: boolean;
  oidc_enabled: boolean;
  password_login_enabled: boolean;
}) {
  vi.spyOn(authApi, "config").mockResolvedValue({ ok: cfg });
}

describe("LoginPage conditional options", () => {
  it("shows a prominent SSO button only when oidc is enabled", async () => {
    mockConfig({ setup_available: false, oidc_enabled: true, password_login_enabled: true });
    renderLogin();
    const sso = await screen.findByTestId("login-sso");
    expect(sso).toBeInTheDocument();
    // Prominent = accent background utility class present.
    expect(sso.className).toContain("bg-accent");
  });

  it("hides the SSO button when oidc is disabled", async () => {
    mockConfig({ setup_available: false, oidc_enabled: false, password_login_enabled: true });
    renderLogin();
    // Password form is always there; wait for config to resolve, then assert no SSO.
    await screen.findByTestId("login-form");
    await waitFor(() => {
      expect(screen.queryByTestId("login-sso")).toBeNull();
    });
  });

  it("shows the setup link only when setup is available", async () => {
    mockConfig({ setup_available: true, oidc_enabled: false, password_login_enabled: true });
    renderLogin();
    expect(await screen.findByTestId("login-setup")).toBeInTheDocument();
  });

  it("hides the setup link when setup is unavailable", async () => {
    mockConfig({ setup_available: false, oidc_enabled: false, password_login_enabled: true });
    renderLogin();
    await screen.findByTestId("login-form");
    await waitFor(() => {
      expect(screen.queryByTestId("login-setup")).toBeNull();
    });
  });
});
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cd web && pnpm test LoginPage`
Expected: FAIL — no `login-sso` / `login-setup` testids; SSO not wired to config.

- [ ] **Step 3: Implement the conditional render**

Edit `web/src/features/auth/LoginPage.tsx`. Add `useQuery` to the imports:

```ts
import { useQuery, useQueryClient } from "@tanstack/react-query";
```

Inside the component (after the existing `useState`/`nav`/`loc`/`qc` hooks), add:

```ts
  const cfgQuery = useQuery({ queryKey: ["auth-config"], queryFn: () => authApi.config() });
  const config = cfgQuery.data && "ok" in cfgQuery.data ? cfgQuery.data.ok : undefined;
  const oidc = config?.oidc_enabled ?? false;
  const setupAvailable = config?.setup_available ?? false;
  const passwordLogin = config?.password_login_enabled ?? true;
```

Replace the returned JSX `<div className="w-full max-w-sm ...">...</div>` inner content so it reads (keep the outer `<main>` wrapper as-is):

```tsx
      <div className="w-full max-w-sm bg-surface border border-border rounded-lg shadow-sm p-6">
        <h1 className="text-xl font-semibold text-fg mb-1">Sign in to knot</h1>
        <p className="text-sm text-fg-muted mb-6">Welcome back</p>

        {oidc && (
          <div className="mb-4">
            <a
              data-testid="login-sso"
              href="/auth/oidc/login"
              className="flex items-center justify-center h-9 w-full rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity"
            >
              Continue with SSO
            </a>
            {passwordLogin && (
              <div className="flex items-center gap-3 my-4" aria-hidden>
                <span className="flex-1 border-t border-border" />
                <span className="text-[12px] text-fg-muted">or</span>
                <span className="flex-1 border-t border-border" />
              </div>
            )}
          </div>
        )}

        {passwordLogin && (
          <form data-testid="login-form" onSubmit={(e) => { void onSubmit(e); }} className="space-y-4">
            <label className="block">
              <span className="block text-[13px] font-medium text-fg mb-1">Email</span>
              <input
                data-testid="login-email"
                type="email"
                autoComplete="email"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                required
                className="h-9 w-full px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm"
              />
            </label>
            <label className="block">
              <span className="block text-[13px] font-medium text-fg mb-1">Password</span>
              <input
                data-testid="login-password"
                type="password"
                autoComplete="current-password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                required
                className="h-9 w-full px-3 rounded border border-border bg-bg text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent text-sm"
              />
            </label>
            {error && (
              <p data-testid="login-error" role="alert" className="text-destructive text-[13px]">
                {error}
              </p>
            )}
            <button
              data-testid="login-submit"
              type="submit"
              disabled={busy}
              className={
                oidc
                  ? "w-full h-9 rounded border border-border bg-bg text-fg text-sm font-medium hover:bg-muted transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
                  : "w-full h-9 rounded bg-accent text-accent-fg text-sm font-medium hover:opacity-90 transition-opacity disabled:opacity-50 disabled:cursor-not-allowed"
              }
            >
              {busy ? "Signing in…" : "Sign in"}
            </button>
          </form>
        )}

        {setupAvailable && (
          <div className="mt-6 pt-4 border-t border-border text-center">
            <a
              data-testid="login-setup"
              href="/setup"
              className="block text-[13px] text-fg-muted hover:text-fg transition-colors"
            >
              First-run setup
            </a>
          </div>
        )}
      </div>
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cd web && pnpm test LoginPage`
Expected: PASS (4 tests).

- [ ] **Step 5: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add web/src/features/auth/LoginPage.tsx web/src/features/auth/LoginPage.test.tsx
git commit -m "feat(web): login page shows setup/SSO conditionally; prominent SSO"
```

---

## Task 4: Full verification

- [ ] **Step 1: Backend tests + clippy**

Run: `cargo nextest run -p knot-server auth_config` then `cargo clippy -p knot-server --all-targets -- -D warnings`
Expected: tests PASS, no clippy warnings.

- [ ] **Step 2: Frontend unit tests + typecheck**

Run: `cd web && pnpm test && pnpm tsc --noEmit`
Expected: all PASS (including `LoginPage` suite).

- [ ] **Step 3: Manual smoke (recommended)**

`make dev` (dev stack runs Dex, so OIDC is on). Open the login page:
- Confirm "Continue with SSO" is the prominent accent button on top, with an "or" divider above the password form, and the password "Sign in" rendered as the secondary outlined button.
- On an instance that already has a user, confirm the "First-run setup" link is absent. (Fresh DB / `make db.nuke` then reload to see it present.)

---

## Self-Review notes

- **Spec coverage:** `GET /auth/config` with all three flags (Task 1) ✓; validator + API client (Task 2) ✓; conditional SSO/setup render + prominent SSO + secondary password submit + no-flicker defaults (Task 3) ✓; backend + frontend tests (Tasks 1,3) ✓.
- **Naming consistency:** `AuthConfig` / `authConfigSchema` / `AuthConfigSchema` used consistently between validators and api client; testids `login-sso` / `login-setup` consistent between Task 3 implementation and test; field names `setup_available` / `oidc_enabled` / `password_login_enabled` identical across backend struct, validator, and tests.
- **No backend authz regression:** `/auth/config` is intentionally public (mounted in the already-unauthenticated `/auth/*` group); real gates remain on `POST /auth/setup` and `GET /auth/oidc/login`.
