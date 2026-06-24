# Security & robustness hardening — design

**Date:** 2026-06-24
**Status:** Approved (brainstorm)

## Context

A thorough review (security, idiomatic/safe Rust, tests) found the codebase
fundamentally sound — zero `unsafe`, clean `cargo-deny`, parameterized SQL, careful
actor model, consistent tenant isolation — with a set of real but fixable issues
clustered at the HTTP edge, in breach-resilience, and a few latent-correctness/test
gaps. This spec captures the agreed fixes and the cross-cutting decisions, then
decomposes the work into four implementation branches, each with its own plan.

## Cross-cutting decisions

- **Session secret — always explicit.** `KNOT_SESSION_KEY` is **required in every
  environment** (≥32 bytes), provided via env var. The app never auto-generates or
  persists a secret (not in the DB, not on disk). Tighten config validation (today
  it's gated on `env == "production"`) to require it unconditionally; dev sets it
  statically in `.env.example`/dev-compose. (See memory: explicit-secrets.)
- **At-rest session hashing — keyed.** Store `HMAC-SHA256(session_key, token)` as the
  `sessions.id` instead of the raw token. A stolen `sessions` table is then useless
  without the externally-held secret, **and** rotating `KNOT_SESSION_KEY` becomes a
  clean global-logout lever. On the deploy that ships this, existing rows stop
  matching → one-time forced re-login (no SQL migration; the secret isn't available
  to plain SQL). Document the rotation semantics.
- **Secure cookies — default on.** Add a `KNOT_COOKIE_SECURE` config flag,
  **default `true`**, decoupled from the `base_url` scheme. dev sets it `false`
  (cookies work over plain-HTTP localhost). The flag also gates HSTS emission.
- **CSP — enforced, validated.** Ship an enforcing `Content-Security-Policy`,
  validated against the Playwright suite + a manual smoke of the editor/boards/
  mermaid/public-doc, relaxing minimally (expected: `style-src 'unsafe-inline'` for
  Tiptap/Excalidraw injected styles) only where proven necessary, each relaxation
  documented inline.

## Branch 1 — HTTP-edge hardening

Goal: close the DoS surface and the stored-XSS path at the HTTP boundary.

- **Global tower layers** on the main router (`crates/knot-server/src/lib.rs:183`
  `router_with_state`): `DefaultBodyLimit` (a sane default, e.g. 2 MB; per-route
  override kept for blob upload's existing 10 MB and any import endpoint),
  `tower_http::timeout::TimeoutLayer`, and a response-header layer setting on **every**
  response: `Content-Security-Policy` (enforced, see decision), `X-Content-Type-Options:
  nosniff`, `X-Frame-Options: DENY` + CSP `frame-ancestors 'none'`, `Referrer-Policy:
  strict-origin-when-cross-origin`, and `Strict-Transport-Security` **only when**
  `cookie_secure`/TLS. A global response layer covers both the SPA HTML and API JSON
  uniformly, so we don't depend on locating the static handler.
- **Blob download hardening** (`routes/api/blobs.rs:203` and the public mirror
  `routes/public.rs:243`): add `X-Content-Type-Options: nosniff` and
  `Content-Disposition: attachment`; serve `image/svg+xml` (and any `text/html`) as
  `application/octet-stream` (coerce), or inline only for an image/PDF allowlist.
  This closes the editor-reachable stored-XSS via uploaded SVG/HTML.
- **WS frame cap:** set an explicit `max_message_size`/`max_frame_size` (a few MiB)
  on the collab + board `WebSocketUpgrade` (`lib.rs:204`, `:251`) instead of the
  ~64 MiB default.
- **Varuint overflow fix** (`crates/knot-server/src/protocol.rs:86`,
  `read_var_bytes`): replace `consumed + len as usize` with
  `consumed.checked_add(usize::try_from(len)…)` returning `DecodeError::Truncated` on
  overflow/oversize; optionally cap `len` against a max-frame constant. This removes
  the release-mode panic reachable from any authenticated collaborator.

Testing: unit test `read_var_bytes` with a huge varuint length → `Err(Truncated)` (no
panic). Integration: a blob download asserts `nosniff` + `attachment` and that an
uploaded `image/svg+xml` is served as a non-inline/neutralized type. The CSP/header
layer: assert headers present on an API response and on a static/HTML response; run
the full Playwright suite with CSP enabled and confirm green (the real validation).

## Branch 2 — Session & cookie hardening

Goal: breach-resilient sessions and correct cookie security.

- **Keyed at-rest hashing** (`crates/knot-storage/src/session_store.rs`): the
  `SessionStore::create`/`find_active` boundary stores/looks up
  `HMAC-SHA256(session_key, token)` rather than the raw token. The cookie still
  carries the raw token. The store needs the key — pass it in (the store is
  constructed where `session_key` is available, or the hashing happens in the auth
  layer before calling the store; choose whichever keeps the store cohesive — the
  plan decides). Reuse the existing HMAC pattern from `knot-auth::csrf`.
- **Invalidate sessions on password change** (`routes/auth/local.rs:288`): add
  `SessionStore::delete_for_user(user_id)` and call it after a successful password
  update (optionally excepting the current session), then re-issue the current
  session cookie. Gives "log out other devices" semantics on password change.
- **`KNOT_COOKIE_SECURE` flag** (`crates/knot-config/src/lib.rs` Config + validation;
  `crates/knot-server/src/auth/cookies.rs:35`): replace
  `secure = base_url.starts_with("https://")` with the config flag (default `true`).
  Wire `KNOT_COOKIE_SECURE` into `.env.example` (dev = `false`) and dev-compose.
- **Require `KNOT_SESSION_KEY` in all envs** (`config.rs:199`): drop the
  `env == "production"` gate; require non-empty ≥32 bytes always. Update
  `.env.example` (already sets it) and dev-compose to provide it.
- **Login-timing dummy-hash** (`routes/auth/local.rs:90`): when the user is absent or
  has no `password_hash`, still run an Argon2 verify against a fixed dummy hash so the
  expensive path runs unconditionally (closes the existing-user timing oracle); keep
  the existing fixed sleep as belt-and-suspenders.

Testing: `find_active(raw)` after `create(raw)` succeeds and the stored `id` ≠ raw
(it's the HMAC); a row stored under key K1 does not match under key K2 (rotation =
invalidation). Password-change integration: after change, the pre-change `sid` is
rejected (401) and a fresh login works. `KNOT_COOKIE_SECURE=false` omits `; Secure`,
default emits it. Config: missing `KNOT_SESSION_KEY` in dev now fails validation.
Login timing: an integration smoke that wrong-password and unknown-user both return
`auth.invalid_credentials` (behavioral; we won't assert wall-clock timing).

## Branch 3 — Latent correctness & cleanup

- **ACL cache key includes `workspace_id`** (`crates/knot-docs/src/cache.rs:20,51`):
  key on `(workspace_id, doc_id, user_id)`; widen `evict_doc`'s invalidation predicate
  accordingly. Closes the latent cross-tenant cache-poisoning for multi-workspace
  users. Add a test: same `(doc,user)` resolves differently per workspace.
- **`acquire()` graceful close** (`crates/knot-crdt/src/registry.rs:82,94`;
  `board_registry.rs`): return `Result`/`Option` instead of `.expect("bus subscribe"/
  "hydrate")`; the WS shims close cleanly on `Err` rather than panicking the
  per-connection task on a transient DB/bus blip.
- **Prune `inflight`** (`registry.rs:21`, `board_registry.rs`): remove the per-doc
  dedup `Mutex` entry once the room is in `map` (or in `evict`), fixing the slow
  unbounded growth.
- **Share-token revoke doc-scoping** (`routes/api/shares.rs:133` →
  `share_tokens.rs:138`): `revoke` takes `doc_id` and the SQL adds `AND doc_id = $2`;
  404 on no rows. Closes the cross-doc revoke IDOR.
- **Grant-inheritance CTE depth cap** (`crates/knot-storage/src/grant_store.rs:106`):
  add `AND c.depth < 10000` to the recursive arm as defense-in-depth against a
  reintroduced cycle DoS in ACL resolution.
- **OIDC `"always"` email-verified gate** (`crates/knot-auth/src/oidc.rs:244`): require
  `email_verified` for the `"always"` auto-provision policy too (matching `"domain"`),
  or at minimum document the footgun; we will add the gate.
- **`LazyLock` for the mention regex** (`routes/api/comments.rs:131`): hoist the
  per-request `Regex::new` into a `std::sync::LazyLock`.
- **Frontend defense-in-depth:** run mermaid SVG output through `sanitizeSvg`
  (`web/src/features/editor/nodes/MermaidCodeBlock.tsx:196`); restrict the Excalidraw
  library fetch to the `libraries.excalidraw.com` host (`ExcalidrawModal.tsx`); add a
  code comment on `PublicDoc.tsx`'s sandbox (security depends on omitting
  `allow-scripts`).
- **Stale `deny.toml` ignore** (`deny.toml:14`): drop the now-unmatched
  `RUSTSEC-2025-0111` testcontainers ignore.

Testing: ACL cache per-workspace test; share-revoke cross-doc → 404 integration;
unit/where-feasible for the registry graceful-close (or rely on existing room tests +
manual). The remaining items are covered by existing suites staying green + targeted
asserts where cheap.

## Branch 4 — Test gaps & tooling

- **`grants_integration.rs`** (new): owner can `PUT`/`DELETE` a grant (204); editor/
  viewer cannot (403 `acl.owner_required`); a `group:` principal is rejected (422);
  a cross-workspace doc id is denied.
- **Viewer-cannot-write over the collab socket:** un-`#[ignore]` / rewrite
  `crates/knot-server/tests/convergence.rs` against `fresh_db` + an authed viewer
  session: a viewer's inbound CRDT update does **not** mutate the doc, while an
  owner/editor's does. Verifies the core socket authz boundary.
- **OIDC callback negative paths:** state mismatch → 400 `auth.oidc.state_mismatch`;
  token-exchange failure → 400; auto-provision `off` for an unknown user → 403.
- **HTTP-level doc-move cycle rejection:** assert `POST /api/docs/:id/move` under a
  descendant returns `409 doc.move_cycle` (currently only storage-tested).
- **Tooling:** add `cargo llvm-cov` (or tarpaulin) + vitest `--coverage` invocations
  (Makefile targets; not necessarily CI-gated) and a `pnpm audit` step for the
  frontend supply chain.

## Sequencing & risk

Branches are independent and can land in any order; suggested order **1 → 2 → 3 → 4**
(edge hardening and session fixes are highest value). Each branch is its own
plan → subagent-driven implementation → review → merge, with build/clippy/tests/e2e
validated before merge. Branch 1's CSP is the only app-breakage risk and is gated on
a green Playwright run with CSP enabled. Branches 1 and 2 force a one-time re-login on
their first deploy (WS-cap is transparent; the session-hash change invalidates
existing sessions) — acceptable pre-1.0.

## Out of scope (noted, not done here)

- Per-replica → shared auth throttle (known multi-replica weakness; separate effort).
- Automatic/periodic key rotation with a keyset (manual rotation is sufficient now).
- Board catch-up/presence parity follow-ups already tracked elsewhere.
