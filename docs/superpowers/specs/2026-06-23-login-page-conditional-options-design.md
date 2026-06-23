# Login page: conditional setup/SSO options + prominent SSO — design

**Date:** 2026-06-23
**Status:** Approved (brainstorm)

## Problem

The login page (`web/src/features/auth/LoginPage.tsx`) renders two footer links
**unconditionally**:

```tsx
<a href="/auth/oidc/login" ...>Sign in with SSO</a>
<a href="/setup" ...>First-run setup</a>
```

Both are misleading once the instance is past first-run or has no SSO:

- **First-run setup** stays visible forever. After the first user exists, the
  backend `POST /auth/setup` returns `410 auth.setup_closed`, so the link leads to
  a dead end.
- **Sign in with SSO** shows even when OIDC isn't configured. `GET /auth/oidc/login`
  returns `503 auth.oidc.disabled`, so the link also dead-ends.
- When SSO *is* configured it's only a small text link — not the prominent,
  preferred path it should be for SSO-first deployments.

The frontend makes **no API calls on load** and there is **no config endpoint**, so
the page currently cannot know which options are valid.

## Goal

1. Show **First-run setup** only when setup is still available (no users yet).
2. Show **Sign in with SSO** only when OIDC is configured, and make it the
   **prominent, primary** action when present.
3. Keep password login always available for now, but structure the config so a
   future flag can hide it without a frontend change.

## Design

### Backend — `GET /auth/config` (unauthenticated)

A new, public, read-only endpoint that the login page fetches on load.

```
GET /auth/config  →  200
{
  "setup_available": bool,        // true when zero users exist
  "oidc_enabled": bool,           // true when OIDC client is configured
  "password_login_enabled": bool  // true for now (future-proofing)
}
```

- New file `crates/knot-server/src/routes/auth/config.rs` with
  `router() -> Router<AppState>` registering `GET /auth/config`, merged in
  `routes/auth/mod.rs` (`.merge(config::router())`).
- It is mounted under `/auth/*`, which the module doc notes is **not** CSRF- or
  auth-gated — correct for a pre-login probe.
- `setup_available`: if `state.users` is present, `users.count().await == 0`; if
  the store is absent (storage unavailable) → `false` (setup can't run anyway). On
  a count error, log and return `false` rather than 500 — this is a hint, not a
  gate, and the real gate stays on `POST /auth/setup`.
- `oidc_enabled`: read `state.oidc_enabled` (already on `AppState`).
- `password_login_enabled`: hardcode `true`. (Backend-only switch later.)

Response serialized via a `#[derive(Serialize)]` struct `AuthConfig`.

**Security note:** both booleans are low-sensitivity. `setup_available` is only ever
true on a fresh, user-less instance; `oidc_enabled` mirrors what the SSO button
already revealed. No user data is exposed.

### Frontend

**API client** (`web/src/auth/session.api.ts`): add

```ts
async config() {
  const r = await apiFetch<unknown>("/auth/config");
  if ("error" in r) return r;
  return { ok: parse(AuthConfigSchema, r.ok) satisfies AuthConfig };
}
```

**Validator** (`web/src/lib/validators.ts`): add

```ts
const authConfigSchema = v.object({
  setup_available: v.boolean(),
  oidc_enabled: v.boolean(),
  password_login_enabled: v.boolean(),
});
export type AuthConfig = v.InferOutput<typeof authConfigSchema>;
export const AuthConfig = authConfigSchema;
```

**LoginPage** fetches config with TanStack Query:

```ts
const cfg = useQuery({ queryKey: ["auth-config"], queryFn: () => authApi.config() });
const config = cfg.data && "ok" in cfg.data ? cfg.data.ok : undefined;
const oidc = config?.oidc_enabled ?? false;
const setupAvailable = config?.setup_available ?? false;
const passwordLogin = config?.password_login_enabled ?? true;
```

Rendering rules (layout = "SSO primary on top"):

- **SSO block** (only when `oidc`): a full-width **accent** anchor button
  `data-testid="login-sso"` → `href="/auth/oidc/login"`, label **"Continue with
  SSO"**, styled like the current primary submit (`bg-accent text-accent-fg`,
  `h-9 w-full rounded`). Below it, an **"or"** divider (two `border-t` rules with
  centered muted "or").
- **Password form** (only when `passwordLogin`): unchanged fields. The submit
  button styling is **conditional**:
  - when `oidc` is true → secondary/outlined (`border border-border bg-bg text-fg
    hover:bg-muted`), since SSO is the primary action;
  - when `oidc` is false → keep today's primary accent styling.
- **Footer**: render the **First-run setup** link (`data-testid="login-setup"`)
  only when `setupAvailable`. The old unconditional SSO text link is removed (the
  SSO block replaces it). When neither footer item applies, omit the divider/footer
  entirely.

**Loading / no-flicker:** while `cfg.isLoading`, render the password form (the
common case) but **defer** the SSO block and setup link until config resolves, so
options never appear and then vanish. Defaults above (`oidc=false`,
`setupAvailable=false`, `passwordLogin=true`) make the loading state render exactly
the password form. If the config request errors, the page degrades to
password-only — still fully usable.

## Files

- Create: `crates/knot-server/src/routes/auth/config.rs`
- Modify: `crates/knot-server/src/routes/auth/mod.rs` (declare + merge)
- Create: `crates/knot-server/tests/auth_config_integration.rs`
- Modify: `web/src/auth/session.api.ts` (add `config()`)
- Modify: `web/src/lib/validators.ts` (add `AuthConfig`)
- Modify: `web/src/features/auth/LoginPage.tsx` (fetch + conditional render + restyle)
- Create: `web/src/features/auth/LoginPage.test.tsx`

## Testing

**Backend** (`auth_config_integration.rs`, mirrors `auth_setup_integration.rs`:
`fresh_db` → `AppState::with_pool` → `router_with_state` → `oneshot`):

- Fresh DB: `GET /auth/config` → 200, `setup_available == true`,
  `oidc_enabled == false` (default), `password_login_enabled == true`.
- After a successful `POST /auth/setup`: `GET /auth/config` → `setup_available ==
  false`.
- With `state.oidc_enabled = true` before building the router: `oidc_enabled ==
  true`.

**Frontend** (`LoginPage.test.tsx`, vitest + @testing-library/react, wrapped in
`QueryClientProvider` + `MemoryRouter`; mock `authApi.config`):

- `oidc_enabled: true` → `login-sso` present and is an accent (prominent) control;
  password submit is the secondary style.
- `oidc_enabled: false` → no `login-sso`; password submit is primary accent.
- `setup_available: true` → `login-setup` present; `false` → absent.
- Config error/loading → password form renders, no `login-sso`, no `login-setup`.

## Risks / Notes

- One extra unauthenticated DB `count()` per login-page load. Negligible; no caching
  (YAGNI). The real authorization gates remain on `POST /auth/setup` and
  `GET /auth/oidc/login`.
- No e2e change required; the dev stack runs Dex (OIDC on), so the SSO button would
  appear there — existing `/setup`-based e2e flows are unaffected because they don't
  assert on login-page footer contents.
