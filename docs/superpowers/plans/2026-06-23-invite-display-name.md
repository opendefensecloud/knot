# Invite Display Name Implementation Plan (item B)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** The invite flow can optionally set a new user's display name end-to-end; omitting it keeps the email-prefix fallback.

**Tech Stack:** Rust (axum) backend, React+TS frontend. Backend tests: `cargo nextest run -p knot-server` (dev-compose Postgres; never testcontainers). Frontend: `cd web && pnpm tsc --noEmit`.

**Spec:** `docs/superpowers/specs/2026-06-23-invite-display-name-design.md`

---

## Task 1: Backend — accept display_name on invite

**Files:**
- Modify: `crates/knot-server/src/routes/api/workspace.rs`
- Test: `crates/knot-server/tests/workspace_invite_password_integration.rs`

Context: `InviteRequest` (~line 103) is `{ email, role, password: Option<String> }`. In `invite_member`, the new-user branch computes `let display = body.email.split('@').next().unwrap_or(&body.email);` then `users.create_local(&body.email, display, &hash)`. The test file has helpers `state_with_seeded_user`, `login`, and exercises invite over HTTP with sid+csrf cookies; `state.users.find_by_email(email)` returns the user for assertions.

- [ ] **Step 1: Write the failing test**

Append to `crates/knot-server/tests/workspace_invite_password_integration.rs` a test that invites a new user with a `display_name` and asserts it sticks, plus one asserting the fallback. Mirror the existing invite test's request-building (owner login → POST `/api/workspace/members` with sid + `X-CSRF-Token`). Skeleton (adapt header/cookie plumbing to match the file's existing invite test exactly):

```rust
#[tokio::test(flavor = "multi_thread")]
async fn invite_sets_display_name_when_provided() {
    let state = state_with_seeded_user("owner@x.test", "owner-pass-1").await;
    let app = router_with_state(state.clone());
    let (sid, csrf) = login(app.clone(), "owner@x.test", "owner-pass-1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", &sid)
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    serde_json::json!({
                        "email": "newbie@x.test",
                        "role": "editor",
                        "password": "newbie-pass-1",
                        "display_name": "Ada Lovelace"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let u = state.users.as_ref().unwrap().find_by_email("newbie@x.test").await.unwrap().unwrap();
    assert_eq!(u.display_name, "Ada Lovelace");
}

#[tokio::test(flavor = "multi_thread")]
async fn invite_falls_back_to_email_prefix_without_display_name() {
    let state = state_with_seeded_user("owner2@x.test", "owner-pass-1").await;
    let app = router_with_state(state.clone());
    let (sid, csrf) = login(app.clone(), "owner2@x.test", "owner-pass-1").await;

    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workspace/members")
                .header("content-type", "application/json")
                .header("cookie", &sid)
                .header("x-csrf-token", &csrf)
                .body(Body::from(
                    serde_json::json!({"email": "plain@x.test", "role": "viewer", "password": "plain-pass-1"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);

    let u = state.users.as_ref().unwrap().find_by_email("plain@x.test").await.unwrap().unwrap();
    assert_eq!(u.display_name, "plain");
}
```

Note: confirm the exact cookie/CSRF header names and the `(sid, csrf)` shape against the file's existing invite test and match them precisely (the helper returns `(sid_kv, csrf_val)`; the existing test shows how they're attached — reuse that verbatim).

- [ ] **Step 2: Run to verify failure**

Run: `cargo nextest run -p knot-server --test workspace_invite_password_integration invite_sets_display_name invite_falls_back`
Expected: FAIL — `display_name` ignored (first test sees "newbie").

- [ ] **Step 3: Implement**

In `crates/knot-server/src/routes/api/workspace.rs`:

Add the field to `InviteRequest`:

```rust
#[derive(Deserialize)]
struct InviteRequest {
    email: String,
    role: String,
    password: Option<String>,
    display_name: Option<String>,
}
```

Replace the display derivation in the `create_local` branch:

```rust
                let display = body
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| body.email.split('@').next().unwrap_or(&body.email));
                match users.create_local(&body.email, display, &hash).await {
```

- [ ] **Step 4: Run to verify pass + clippy**

Run: `cargo nextest run -p knot-server --test workspace_invite_password_integration` then `cargo clippy -p knot-server --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/knot-server/src/routes/api/workspace.rs crates/knot-server/tests/workspace_invite_password_integration.rs
git commit -m "feat(workspace): accept display_name on invite for new users"
```

---

## Task 2: Frontend — display-name field in invite

**Files:**
- Modify: `web/src/features/workspace/workspace.api.ts`
- Modify: `web/src/features/workspace/MembersPage.tsx`

- [ ] **Step 1: Extend the API client**

In `workspace.api.ts`, replace the `invite` method:

```ts
  invite(
    email: string,
    role: "owner" | "editor" | "viewer",
    password?: string,
    displayName?: string,
  ) {
    const body: Record<string, unknown> = { email, role };
    if (password) body.password = password;
    const name = displayName?.trim();
    if (name) body.display_name = name;
    return apiFetch<void>("/api/workspace/members", { method: "POST", body });
  },
```

- [ ] **Step 2: Add the form field + state**

In `MembersPage.tsx`:
- Add state after `invitePassword` (line ~22): `const [inviteName, setInviteName] = useState("");`
- Update the mutation call (line ~26):
  `workspaceApi.invite(inviteEmail, inviteRole, invitePassword || undefined, inviteName || undefined)`
- Clear it on success alongside the others (after line ~39): `setInviteName("");`
- Add the input to the form, right after the email `<input>` (after line 88) and before the role `<select>`:

```tsx
            <input
              data-testid="invite-display-name"
              type="text"
              value={inviteName}
              onChange={(e) => setInviteName(e.target.value)}
              placeholder="Display name (optional)"
              className={`${inputCls} flex-1 min-w-[160px]`}
            />
```

- [ ] **Step 2: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add web/src/features/workspace/workspace.api.ts web/src/features/workspace/MembersPage.tsx
git commit -m "feat(web): display-name field in member invite form"
```

---

## Task 3: Verification

- [ ] **Step 1:** `cargo nextest run -p knot-server --test workspace_invite_password_integration` → PASS; `cargo clippy -p knot-server --all-targets -- -D warnings` → clean.
- [ ] **Step 2:** `cd web && pnpm test && pnpm tsc --noEmit` → PASS.
- [ ] **Step 3 (manual, optional):** `make dev`, invite a new email with a display name + password; confirm the member appears with that name; invite one without a name and confirm the email prefix is used.

---

## Self-Review notes

- Spec coverage: backend optional display_name + fallback (Task 1) ✓; API client (Task 2) ✓; form field (Task 2) ✓; backend tests for set + fallback (Task 1) ✓.
- Field name `display_name` consistent across backend struct, API body, and tests. Only the new-user (`create_local`) path is affected; existing users are untouched, matching the spec.
