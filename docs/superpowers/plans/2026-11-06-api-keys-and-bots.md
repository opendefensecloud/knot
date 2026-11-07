# Scoped API keys and bot accounts (implementation plan)

**Goal:** `/api/*` accepts `Authorization: Bearer knot_…` as an alternative to
the `sid` cookie. A key carries any mix of the `read`, `create`, `update` and
`delete` scopes, is shown once, is stored only as an HMAC, and can never mint
another credential or outlive the password reset that revoked it. Workspace
owners can add credential-less bot accounts and manage their keys.

**Architecture:** Bearer resolution lives in the existing session loader
(`auth/api_key_loader.rs`), so every `/api` handler keeps its single
`AuthContext` extractor and the layer order stays `require_session → scope →
csrf`. Scope enforcement is a new middleware with two tables and no per-route
annotations. The invariants that a second writer could bypass (a migration, a
direct `UPDATE users`) are PL/pgSQL triggers. The web adds a Settings section
and a bot column to Members.

**Tech Stack:** Rust (axum 0.8, sqlx 0.9, tokio, hmac/sha2, rand), Postgres 18
(PL/pgSQL triggers, custom SQLSTATEs `KB001`–`KB004`), React + TanStack Query +
Vitest, Playwright.

**Spec:** [specs/2026-11-06-api-keys-and-bots-design.md](../specs/2026-11-06-api-keys-and-bots-design.md)
— the threat table `S1`–`S9` cited throughout the code lives there.

## Global Constraints

- **One additive migration**, `migrations/20261106120000_api_keys_and_bots.sql`,
  forward-only, run by sqlx in one transaction (and by the Helm `pre-upgrade`
  Job). `SET LOCAL lock_timeout = '5s'` so a long transaction makes the
  migration fail fast instead of queueing sign-ins; the upgrade note says to
  re-run it. No table rewrite: `users.kind` is added with a constant default.
- **No plaintext at rest.** `key_hash = HMAC-SHA256(KNOT_SESSION_KEY, secret)`,
  deterministic so the `UNIQUE` index is the lookup. The same key as the
  session HMAC, different input shape and different table, so a value from one
  can never match in the other. Consequence, documented in the upgrade note:
  rotating `KNOT_SESSION_KEY` revokes every key too.
- **One 401 for every unusable key** (`api_key.invalid`): malformed, unknown,
  expired, revoked, user no longer a member. No oracle, no sleep (256 bits of
  entropy behind a unique index).
- **No fallback.** A Bearer header on `/api/*` is judged, never ignored in
  favour of the cookie; a Bearer next to another `Authorization` header is
  refused. Non-Bearer schemes and paths outside `/api/*` fall through to the
  cookie exactly as before this change.
- **The scope tables are the only place scope exceptions live.** Adding a row
  requires a dedicated test: both tables assert their row count, so a silent
  addition fails the build.
- **Bots are refused in the app *and* in the database.** The app answers a
  clean 4xx before the store call; the trigger is the backstop for any path
  the app doesn't cover.
- **Every revoker keeps its timestamp.** `revoked_at` is only set when it is
  `NULL`, so a password change never rewrites an earlier revocation.
- Error codes are stable API surface: `api_key.invalid`,
  `api_key.scope_denied` (with `details.scope`), `api_key.session_required`,
  `api_key.foreign_forbidden`, `api_key.not_found`, `api_key.scopes_invalid`,
  `api_key.name_invalid`, `api_key.limit_reached`, `api_key.ttl_too_short`,
  `api_key.ttl_too_long`, `bot.forbidden`, `bot.owner_forbidden`,
  `bot.restricted_forbidden`, `member.kind_invalid`.

---

### Task 1: `knot-auth` key format, HMAC and TTL policy

`crates/knot-auth/src/api_key.rs` (new), `lib.rs`:

- `ApiKeySecret::generate()` — `knot_` + base64url-no-pad of 32 CSPRNG bytes;
  panics rather than mint a predictable key if the OS CSPRNG fails, like
  `SessionToken::generate`. `Debug` redacts.
- `looks_like_api_key` — prefix plus exact 43-char base64url body, no
  allocation, no decode: a cheap first filter before an HMAC.
- `hash(secret, session_key)` — reuses `csrf::hash_session_id`.
- `PrincipalKind` (`Local` / `Oidc` / `Bot`), `max_ttl_days`,
  `validate_ttl_days`: minimum 30 days for everyone; 90 days *and* an expiry
  for pure-OIDC humans; unbounded allowed for bots and local humans.

Tests: prefix/length, decode length, round-trip hash determinism, redacted
`Debug`, TTL matrix (29 → too short, exactly 90 for OIDC → allowed, `None` for
OIDC → too long, `None` for bot → allowed).

### Task 2: Storage and the migration

`crates/knot-storage/src/api_key_store.rs` (new), `lib.rs`, `audit.rs`,
`user_store.rs` (`kind`), `workspace_store.rs`:

- `ApiKeyStore` trait: `create`/`find_live_by_hash`/`list_for_user`/`revoke`/
  `touch`, with `ApiKeyStoreError::{SessionEnded, LimitReached, TtlRejected,
  Sqlx, UnknownScope}`.
- `create` runs in one transaction: `FOR NO KEY UPDATE` on the user rows (in
  id order, so two mints never wait crosswise), re-check of the minting
  session, `FOR SHARE` on the membership rows with the role re-check, the
  50-live-key count, then the insert. `KB003` maps to `TtlRejected`.
- `find_live_by_hash` filters `revoked_at IS NULL` and expiry; `touch` is
  debounced to once a minute and skips revoked rows.
- `list_for_user` returns live keys plus those revoked or expired in the last
  30 days; the column list never includes `key_hash`.
- `audit.rs`: `api_key.create` / `api_key.revoke` rows carry the key id, the
  scopes and the expiry — never the secret or a hash.
- Migration: `users.kind`, the credentials CHECK, the `api_keys` table and its
  indexes, and the six triggers with their functions (`KB001` bot not
  workspace owner, `KB002` bot never a doc owner under any UUID spelling,
  `KB003` SSO TTL cap, `KB004` kind immutable, S5 password revocation, S8
  membership revocation). Triggers are `DROP`+`CREATE` so a later
  `CREATE OR REPLACE FUNCTION` takes effect.

Tests: `crates/knot-storage/tests/api_keys.rs` (mint/revoke/list/touch, the
50-key cap, the session-ended and membership races under real row locks,
`revoked_at` preserved) and `crates/knot-storage/tests/migrations_apply.rs`
(every column, constraint and trigger, including all four SQLSTATEs and the
"S5 revokes the bot keys the user minted" case).

### Task 3: Bearer authentication and the scope middleware

`crates/knot-server/src/auth/api_key_loader.rs`, `scope.rs` (both new),
`session_loader.rs`, `context.rs`, `csrf.rs`, `mod.rs`,
`routes/api/mod.rs`:

- `authenticate` returns `Option<(AuthContext, ApiPrincipal)>`, `Ok(None)`
  meaning "fall through to the cookie"; it reads the header synchronously and
  returns a future so it can run inside the `!Sync` request the session loader
  already holds.
- `scope_mw` reads `MatchedPath` (the route template, not the URI) so the
  override table can't be fooled by an id that looks like a segment; with no
  matched path only the method default applies.
- `csrf_mw` skips the double-submit check only for a request carrying an
  `ApiPrincipal`.

Tests: `scope.rs` unit tests (one per override row, the row-count guards, the
layer-order tests proving a key with the scope passes and a cookie session
still needs CSRF) and `auth/api_key_loader.rs` (malformed header, second
`Authorization`, revoked key, non-member, non-`/api/` path, non-Bearer scheme).

### Task 4: Key management HTTP routes and bot accounts

`routes/api/api_keys.rs` (new), `bots.rs` (new), `routes/api/workspace.rs`,
`routes/api/grants.rs`, `routes/api/duplicate.rs` → `duplicate.rs`,
`restricted.rs`, `docs.rs`, `export_import.rs`, `routes/auth/local.rs`,
`oidc.rs`:

- `GET`/`POST`/`DELETE /api/auth/api-keys` — session-only (`S2`), with
  `?user_id=` for an owner acting on a bot of their workspace; a human target
  is `api_key.foreign_forbidden`, an unknown id `api_key.not_found`.
- `POST /api/workspace/members` gains `"kind": "bot"`: no password, editor or
  viewer only, refused if the workspace role would be owner.
- Bots are refused `restrict`, owner grants, creating under a restricted
  parent, templates, duplicates, restricted imports (`bot.restricted_forbidden`
  / `bot.owner_forbidden`), and where a human creator becomes page owner a bot
  gets `editor`.
- `GET /api/workspace/members` exposes `kind`.
- `local.rs`: a bot can't log in (the NULL `password_hash` takes the timing
  dummy path), and `change_password` sets the hash and deletes every session
  in one transaction so the S5 trigger and the session purge commit together.
- `oidc.rs`: a bot is refused before any provisioning, and an OIDC account
  cannot claim a bot's email.
- `grants.rs`: a `user:<id>` principal is stored under the canonical UUID
  whatever spelling the request used, and removed by any of them, so one
  person never holds several rows that member removal would miss.

Tests: `crates/knot-server/tests/api_keys_integration.rs` — scope matrix over
every route, session-only matrix, foreign/unknown key oracles, TTL and cap
error codes, revocation paths (password change, direct hash update, removal,
demotion, expiry), the mint races (`S5`, `S8`), bots vs. restricted subtrees,
grants canonicalisation.

### Task 5: Web UI and e2e

`web/src/features/apiKeys/*` (new), `features/workspace/{MembersPage,
SettingsPage,workspace.api}`, `permissions/PermissionsDialog.tsx`,
`auth/{SessionContext,session.api}`, `lib/validators.ts`:

- Settings → API keys: create with a name, scopes and a lifetime; the secret
  is shown once in a copyable panel and never again; list with
  created/last used/revoked state; revoke with confirmation.
- Members: bots are labelled as such, can be added with the bot toggle, and
  the owner gets a "Manage keys" action on a bot's row.
- The permissions dialog hides ownership for a bot.
- `e2e/flows/api-keys.spec.ts`: read-only key reads but can't create, revoked
  key is rejected, changing the password revokes, a bot key works but can't
  reach owner-only actions, and a TTL below 30 days is refused.

### Task 6: Operator documentation

`ARCHITECTURE.md` (middleware chain, the `/api` bearer path), `CHANGELOG.md`
(Unreleased: upgrading, added, security), the Helm README's upgrade notes,
and `.claude/skills/knot-api/SKILL.md` (recipes a script or agent can copy).

## Review Focus

1. **A key that renames a route out of the scope table.** `scope_mw` uses
   `MatchedPath`, and both tables are pinned to real route templates by tests
   that build the real router and hit it (`scope.rs`,
   `api_keys_integration.rs`).
2. **A mint racing a password change, a removal or a demotion.** Row locks in
   `PgApiKeyStore::create`, the S5/S8 triggers, and four integration tests that
   start the race in a second connection and hold it open.
3. **A leaked key that outlives the reset.** The password trigger revokes the
   user's keys *and the bot keys the user minted*; a regression test in both
   the migration tests and the HTTP tests.
4. **A bot reaching ownership.** Every app path that grants page ownership is
   guarded, the DB triggers backstop them, and restricting a page as a bot is
   refused before the grant is written.
5. **The plaintext leaking.** No store column, no `Debug`, no log line, no
   audit row, no sqlx error carries it; the HMAC is dropped from memory before
   the lookup.
