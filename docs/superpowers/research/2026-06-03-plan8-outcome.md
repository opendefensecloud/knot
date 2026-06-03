# Plan 8 Outcome — Auth Completion

**Status:** GO. All 12 tasks landed; all gates green.

**Verdict:** Continue to Plan 7 (UI polish) or Plan 9 (deployment). Auth is now feature-complete for v0.1 — workspace owners can independently onboard collaborators with passwords, users can rotate their own passwords, and the OIDC flow via Dex round-trips a session.

## What landed

Plan 8 commits, oldest to newest (HEAD `a4077fc`):

| Commit | Task | Subject |
|---|---|---|
| 825938c | T1  | POST /auth/password — change own password handler |
| 1718461 | T2  | change-password integration tests (5 cases) |
| f58c52b | T3  | invite-with-password creates a fresh user if missing |
| 850a13f | T4  | invite-with-password integration tests (3 cases) |
| 1dacc9f | T5  | frontend: authApi.changePassword + invite-with-password signature |
| f3adba1 | T6  | MembersPage invite form — optional initial password |
| f99df0e | T7  | SettingsPage — change password form |
| 9d3313f | T9  | e2e: Dex OIDC round-trip — lands a session |
| 62b004a | T10 | e2e: two-users-converge — real two-user editing under invite-with-password |
| a4077fc | T11 | e2e: invite-with-password + change-password flows |

T8 was a research task (manual OIDC drift check — no commit; verified the existing OIDC flow against Dex is correct and recorded the env-var contract for T9). T12 is this outcome doc.

## Gates

- `cargo test --workspace` — green (8 new integration tests across the two test files)
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — clean
- `pnpm tsc` + `pnpm lint` + `pnpm test` — clean
- `pnpm playwright test` — **15 / 15** pass:
  - existing: auth × 2, collab, docs, editor, health, login × 3, two-users-converge
  - new: oidc, invite-password × 2, change-password × 2
- Manual smoke: invite Bob with password → Bob signs in → Alice + Bob edit concurrently → both see both updates → Bob rotates his password → old fails, new succeeds

## What was non-obvious

**The CRDT room actor wasn't framing y-sync messages.** Plan 6 replaced `y-websocket` with a custom `KnotProvider` that decodes `[MSG_SYNC, SYNC_UPDATE, varuint_len, payload]` frames. The Plan 5 room actor, however, was still emitting raw yrs update bytes (the old format y-websocket spoke). The Plan 6 single-user test reload-persisted-fine because reload re-syncs from snapshot via SyncStep2, but the new two-user test in T10 caught it: Bob's edits arrived at Alice but were silently dropped by Alice's provider because the first byte wasn't a known message type. Fixed in `crates/knot-crdt/src/room.rs` by adding `wrap_sync_update()` to all three fan-out paths (`on_inbound`, `Event::ApplyUpdate`, `replay_since_watermark`). Awareness frames were already correctly forwarded verbatim.

**Dex's bcrypt hash in dev didn't match the password it claimed.** `deploy/compose/dex/config.yaml` shipped a comment saying `password = "password"` but the bcrypt hash was for a different value. Discovered during T9 — Dex returned 401 to every login attempt. Regenerated with `htpasswd -nbB -C 10 user password` and committed alongside the OIDC e2e.

**Tiptap collab cursor decorations pollute `textContent()`.** When asserting on convergence text in T10, the `collaboration-cursor__label` spans rendered the peer's name inline. Solved by walking only bare text nodes via a `page.evaluate` helper that filters `.collaboration-cursor__label`. Documented inline in the spec.

**Figment expects `KNOT_OIDC_ENABLED=true` not `=1`.** The dev `make compose.up` flow doesn't expose OIDC, so this was first hit during T8. Documented in playwright.config.ts.

## What's still deferred

- **Password reset via email token** — needs SMTP + email templates. Workaround for v0.1: workspace owner can re-invite-with-password (which currently fails with 409 if the user already exists; a small `POST /api/workspace/members/:user_id/reset-password` would close this, but deliberately out of scope).
- **`must_change_password` flag on invite** — Owner shares the initial password out-of-band; first-login forcing is a UX nicety.
- **OIDC group → workspace role sync** — `KNOT_OIDC_AUTO_PROVISION=always` exists; group-based role mapping (`KNOT_OIDC_ROLE_FROM_GROUPS`) wires through, but no test coverage and no UI for managing the mapping.
- **Rate-limiting on `/auth/password`** — leans on the existing global throttle. A bespoke per-user rate limit + lockout policy is hardening, not v0.1.

## Carryforward for the next plan

Recommendations, in priority order:

1. **Plan 9 — Deployment** (Helm chart + multi-arch image build). v0.1 is feature-complete now; getting it deployable is the highest-leverage next step. The Helm chart needs at minimum: knot-server Deployment + Service + Ingress, Postgres + Dex sidecars or external refs, config secrets, the Plan 5 `make db.cleanup`-style migration job, multi-arch (amd64/arm64) image build with mimalloc per the project's memory.

2. **Plan 7 — UI polish.** Drag-drop tree move (server `POST /api/docs/:id/move` already exists; needs UI), command palette (Zustand slot is wired), per-doc effective-role-aware editor toolbar, mobile responsive pass. Independent of Plan 9; can interleave.

3. **Plan 10 — Observability.** OTLP traces + metrics endpoints are stubbed; need wiring + sample Grafana dashboards.

The two-user convergence test now actively guards the CRDT wire contract — any future provider refactor will trip it. Worth keeping as a canary even after Plan 9.

## Files of interest

| Path | Role |
|---|---|
| `crates/knot-server/src/routes/auth/local.rs` | POST /auth/password handler |
| `crates/knot-server/src/routes/api/workspace.rs` | invite-with-password branching |
| `crates/knot-server/tests/auth_password_integration.rs` | 5 change-password cases |
| `crates/knot-server/tests/workspace_invite_password_integration.rs` | 3 invite-with-password cases |
| `crates/knot-crdt/src/room.rs` | y-sync framing fix (`wrap_sync_update`) |
| `web/src/features/workspace/MembersPage.tsx` | invite form with password |
| `web/src/features/workspace/SettingsPage.tsx` | change-password form |
| `e2e/flows/oidc.spec.ts` | new — Dex round-trip |
| `e2e/flows/two-users-converge.spec.ts` | rewrite — real two-user editing |
| `e2e/flows/invite-password.spec.ts` | new — owner onboards Bob |
| `e2e/flows/change-password.spec.ts` | new — rotate own password |
| `deploy/compose/dex/config.yaml` | corrected bcrypt hash for dev users |
