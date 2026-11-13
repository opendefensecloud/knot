# Outcome: scoped API keys and bot accounts

**Date:** 2026-11-06
**Spec:** [specs/2026-11-06-api-keys-and-bots-design.md](../specs/2026-11-06-api-keys-and-bots-design.md)
**Plan:** [plans/2026-11-06-api-keys-and-bots.md](../plans/2026-11-06-api-keys-and-bots.md)
**Goal:** Let scripts and AI agents call `/api/*` with `Authorization: Bearer
knot_…`, narrowed by `read`/`create`/`update`/`delete` scopes, and give a
workspace credential-less bot members. A key can't mint another credential,
widen its own access, or outlive the password reset that revoked it.

## What landed

- **Key format and at-rest form.** `knot_` + 32 CSPRNG bytes, base64url; stored
  only as `HMAC-SHA256(KNOT_SESSION_KEY, secret)`, deterministic so the
  `UNIQUE` index is the lookup. `ApiKeySecret`'s `Debug` is redacted; the
  plaintext exists in the 201 response and nowhere else.
- **Bearer authentication in the session loader**, for `/api/*` only, so every
  handler keeps one `AuthContext` extractor and the existing layer order. One
  indistinguishable `401 api_key.invalid` for malformed, unknown, expired,
  revoked and non-member keys; a Bearer header is never retried as a cookie,
  and a second `Authorization` header is refused.
- **Scope middleware** (`auth/scope.rs`) between `require_session` and CSRF.
  Method → scope, an `OVERRIDES` table for the fourteen POSTs that aren't
  creates, and a `SESSION_ONLY` table for key management, member management and
  creating/listing public share links. Both tables are pinned by tests that
  build the real router and hit it, and both assert their row count so a new
  row without its test fails the build. CSRF is skipped only for a request
  carrying an `ApiPrincipal`.
- **Bots.** `users.kind`, a credentials CHECK that makes a bot
  password-less and SSO-less, and triggers that keep a bot from ever becoming
  a workspace or page owner (`KB001`, `KB002`) and `kind` from changing
  (`KB004`). The app refuses the same paths with a clean 4xx before writing
  (`bots.rs`), and a bot is refused restrict, owner grants, creating under a
  restricted parent, templates, duplicates and restricted imports.
- **Revocation.** Revoked by the user, by an owner for a bot's keys, by
  expiry, by any change of the user's password hash (`S5`), by removal from the
  workspace and by owner demotion (`S8`), plus `KNOT_SESSION_KEY` rotation.
  Membership is also re-read on every request, so a key of a non-member is
  inert even if a trigger were missed. Every revoker sets `revoked_at` only
  when it is `NULL`, so it never rewrites an earlier timestamp.
- **Mint races.** `PgApiKeyStore::create` locks the user rows `FOR NO KEY
  UPDATE` in id order, re-checks the session that asked, share-locks the
  membership rows and counts the live keys before the insert, so a concurrent
  password change, removal or demotion either refuses the mint or revokes the
  committed key. `change_password` sets the new hash and deletes every session
  in one transaction, so the trigger and the session purge commit together.
- **Web.** Settings → API keys (create with name, scopes and lifetime; reveal
  once; list with created/last used/revoked; revoke with confirmation) and a
  Members page that labels bots, adds one with a toggle, and gives an owner a
  "Manage keys" action on a bot's row.
- **Docs.** `ARCHITECTURE.md` describes the bearer path and the two tables;
  `CHANGELOG.md` has the upgrading/added/security entries; the
  `knot-api` skill has copy-paste recipes a script or agent can follow.

## What was non-obvious

- **`MatchedPath`, not the URI.** The override table keys on axum's route
  template. Matching the URI would let a caller invent a segment that looks
  like an override (`/api/docs/x/purge/y`) and dodge the scope; with no
  matched route only the conservative method default applies.
- **The password trigger has to revoke `created_by` too.** A stolen owner
  session can mint a bot key just before the owner resets the password; the
  bot key is `user_id = bot`, so revoking only `user_id = NEW.id` would let it
  outlive the reset. The trigger now revokes `user_id = NEW.id OR created_by =
  NEW.id`, matching what the removal and demotion triggers already did.
  Trade-off, documented: an ordinary password rotation also kills the bot keys
  that user minted, and an owner re-mints them.
- **`ApiPrincipal` is what tells CSRF to stand down.** A key is a header, not
  a cookie a browser attaches on its own, so the double-submit check is
  pointless for it — but the skip must be keyed on the principal, not on the
  presence of an `Authorization` header, or any client could send a junk
  Bearer and skip CSRF. A junk Bearer is a 401 instead.
- **`touch` skipped revoked rows.** The fire-and-forget `last_used_at` update
  could land after a concurrent revoke and make the UI show "last used" newer
  than "revoked". One `revoked_at IS NULL` in the WHERE clause.
- **`session_loader` needed a non-`async fn` shape.** The `Body` is not `Sync`,
  so holding `&Request<Body>` across an await makes the middleware `!Send`;
  `authenticate` reads the header synchronously and returns a future that
  borrows only the deps.
- **A second `Authorization` header is ambiguous.** Two headers with one Bearer
  is refused rather than picking one, so a proxy-added `Basic` header can't
  silently change which credential was judged.

## Gates

- `cargo fmt --all -- --check`: clean.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  clean.
- `cargo test --workspace --all-features` against the dev-compose Postgres:
  1332 tests passing, none failing.
- `web`: `tsc --noEmit` and `eslint` clean; Vitest 100 files, 1047 tests
  passing.
- Playwright: 128 passing. `ws-reconnect.spec.ts` (2 tests) needs toxiproxy's
  host networking, which Docker Desktop on macOS doesn't provide; the specs
  pass in CI's Linux runner. The new `flows/api-keys.spec.ts` (5 tests) passes.

## Deferred / carryforward

- **Per-page scoping of a key.** A key acts with its user's full page access,
  narrowed only by HTTP scope. Restricting a key to a subtree would need a
  grant model per key; not designed.
- **Key rotation.** No "rotate in place"; revoke and mint, and the listing
  keeps the revoked row visible for 30 days.
- **Owner-minted bot keys and routine password rotation.** As above, a
  password change kills them. If that turns out to bite CI-heavy workspaces,
  the alternative is to revoke only on a *forced* reset, which needs a signal
  the server doesn't have today.
- **Rate limiting on key authentication.** Deliberately none: 256 bits of
  entropy behind a unique index. The login/password endpoints' throttling is
  unaffected.
