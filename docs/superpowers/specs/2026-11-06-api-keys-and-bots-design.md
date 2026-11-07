# Scoped API keys and bot accounts (design)

**Date:** 2026-11-06
**Plan:** [plans/2026-11-06-api-keys-and-bots.md](../plans/2026-11-06-api-keys-and-bots.md)

## Goal

Let a script or an AI agent call `/api/*` as a user or a bot with
`Authorization: Bearer knot_…`, holding only the scopes it needs, and give a
workspace a credential-less member (a bot) for automation. Nothing a key can
do may outlive the key, take over the account, or widen its own access.

**Not in scope:** token exchange for OIDC, per-document scoping of a key
(a key acts with the full page access of its user, narrowed only by HTTP
scope), key rotation, `POST /auth/*` with a key, and the live-editing
websocket.

## Threat model

The comments in the code cite these by number (`S5`, `S8`, …), so this list is
the reference. Every threat has a server-side check and, where a second writer
could bypass the server (a migration, a direct database update, another code
path), a database backstop.

| # | Threat | Enforced by |
|---|---|---|
| **S1** | A key authenticates something outside `/api/*` — the `/collab/*` websocket, `/auth/password`, `/auth/logout`, `/auth/session`, `/p/*`. | `auth/api_key_loader.rs` only considers a Bearer header when the path starts with `/api/`. Everything else ignores it and falls through to the cookie. |
| **S2** | A key mints or manages a credential: another key, a member invitation or role change, a public share link. Each would let the key widen its own access or leave behind something that outlives it. | `auth/scope.rs::SESSION_ONLY`, in the middleware for every method, plus a second in-handler check in `routes/api/api_keys.rs`. |
| **S3** | A key with `read` (say) performs a write. | `auth/scope.rs`: the HTTP method picks the scope, `OVERRIDES` remaps the POSTs that are not creates, unknown methods need `update`. Table tests pin the override and session-only rows to real route templates. |
| **S4** | A bot ends up owning the workspace or a page. Owners can restrict pages, change page-owner grants and manage members, so a bot owner is a privilege escalation for whoever holds its key. | App guards in `bots.rs` (4xx before the store call), plus DB triggers `KB001` (`workspace_members` bot not owner), `KB002` (`document_grants` bot never `owner`, whatever UUID spelling the principal uses), `KB004` (`users.kind` never changes, so a human owner can't be flipped into a bot). |
| **S5** | A key outlives the password reset that was meant to recover from a stolen session. | The mint re-checks its own session inside the insert transaction (`PgApiKeyStore::create` locks the user rows `FOR NO KEY UPDATE`), and the trigger `users_revoke_api_keys_on_password_change` revokes `user_id = NEW.id OR created_by = NEW.id` — the user's keys **and the bot keys that user minted**, so a key minted just before the reset dies with it. A password change sets the hash and ends the sessions in the same transaction. |
| **S6** | A key of a user who cannot change a password (an OIDC-only account) stays valid forever. | `validate_ttl_days` caps it at 90 days and requires an expiry, and the trigger `api_keys_sso_ttl_guard` (`KB003`) refuses anything longer or unbounded, using the database clock. |
| **S7** | The plaintext key leaks: into the database, a log line, a trace, an audit row, an error message. | Only `HMAC-SHA256(KNOT_SESSION_KEY, secret)` is stored; `ApiKeySecret`'s `Debug` is redacted; the secret appears in the 201 body and nowhere else; the loader drops it before the lookup; sqlx errors don't include bound values; `TraceLayer` records no headers. |
| **S8** | A key keeps working after its user leaves the workspace or loses the owner role. | Membership is re-read on every request (`api_key_loader.rs`), so a key of a non-member is `401 api_key.invalid` even if a trigger were missed; the triggers `workspace_members_revoke_api_keys` revoke on removal and on owner demotion, and a mint racing either is refused under row locks. |
| **S9** | A broken Bearer header silently downgrades the request to whatever cookie the client happens to send — a different identity, or a session the key's owner never authorised. | Once a Bearer header is present on `/api/*`, every failure is the one indistinguishable `401 api_key.invalid`. There is no fallback to the cookie. A second `Authorization` header alongside a Bearer is refused the same way. |

## Data model

`users.kind text NOT NULL DEFAULT 'human' CHECK (kind IN ('human','bot'))`, and
`CHECK (kind <> 'bot' OR (password_hash IS NULL AND oidc_subject IS NULL))`:
a bot can never log in interactively.

`api_keys(id, workspace_id, user_id, name, key_hash UNIQUE, scopes text[],
created_by, created_at, expires_at, last_used_at, revoked_at)`:

- `key_hash` is the HMAC, so the unique index is the lookup path and no secret
  is stored.
- `scopes` is a non-empty subset of `read, create, update, delete` (CHECK).
- `user_id` is the principal the key acts as; `created_by` is the session that
  minted it. For your own key they are the same; for a bot's key `created_by`
  is the owner who minted it, which is what S5/S8 revoke on.
- `expires_at NULL` means never (bots and local-password humans); the SSO cap
  is a trigger, not a column constraint, because it depends on `users`.
- `revoked_at` is kept for 30 days in the listing so a user can see what
  disappeared and why; a revoked row never authenticates.

`workspace_members.role` is unchanged; a bot is `editor` or `viewer`.

## Authentication flow

1. `auth/api_key_loader.rs::authenticate` runs inside the session loader, only
   under `/api/`, only when an `Authorization` header uses the Bearer scheme,
   and only when the API-key store is configured.
2. The secret must look like `knot_` + 43 base64url chars; otherwise
   `401 api_key.invalid`.
3. `HMAC-SHA256(session key, secret)` → `find_live_by_hash` (not revoked, not
   expired). Any miss, including an expired or revoked key, is the same 401.
4. The user's membership in `api_keys.workspace_id` is read; none → 401.
5. `AuthContext { user_id, workspace_id, role }` and
   `ApiPrincipal { key_id, scopes }` go into the request extensions.
   `last_used_at` is touched fire-and-forget, debounced to once a minute per
   key, and never on a revoked key.
6. `routes/api/mod.rs` layers `require_session` → scope → CSRF. `csrf_mw`
   skips the double-submit check only when `ApiPrincipal` is present — a key
   is a header, not a cookie a browser attaches on its own.

## Scope model

Method default: `GET|HEAD|OPTIONS → read`, `POST → create`,
`PATCH|PUT → update`, `DELETE → delete`, anything else → `update`.

The `OVERRIDES` table in `auth/scope.rs` is the single place where a POST is
not a create: the doc mutations that move, restore, template, import
markdown, resolve comments, accept/reject/withdraw suggestions, mark
notifications read, and `purge` (delete). `POST /api/docs/{id}/export/{format}`
needs `read` because it renders a file and writes nothing.

`SESSION_ONLY` lists what no key may do, whatever its scopes: create, list and
revoke keys; invite, re-role and remove members; create and list public share
links (revoking one only narrows access, so `DELETE …/shares/{share_id}` stays
open to `delete`).

Both tables are checked by tests that assert their row count, so adding a row
without a dedicated test fails the build. `routes/api/api_keys.rs` also refuses
a key in-handler, so a future route rename can't silently expose key
management.

## Bots

An owner adds a bot with `POST /api/workspace/members`
`{"email", "display_name", "role": "editor"|"viewer", "kind": "bot"}`. The bot
has no password and no OIDC identity (CHECK constraint), cannot sign in
through the login form or through SSO (both refuse a bot before doing any
work), and cannot be re-created after removal (its email stays taken). Only an
owner may mint, list or revoke a bot's keys, and only for a bot that is a
member of their own workspace.

A bot is refused every path that would make it a page owner: `restrict`, a
`user:<id>` owner grant, creating a page under a restricted parent (also from
a template, a duplicate or a workspace import), importing restricted pages,
and duplicating a restricted page. Where a human creator becomes the owner of
a page they create, a bot gets an `editor` grant.

## Credential lifecycle

- Minimum lifetime 30 days (`400 api_key.ttl_too_short`); no maximum for bots
  and local-password humans; at most 90 days and never unbounded for pure-OIDC
  humans (`400 api_key.ttl_too_long`).
- At most 50 live keys per user or bot (`409 api_key.limit_reached`), counted
  by user, not by workspace.
- Revocation: by the user, by an owner for a bot's keys, on expiry, on any
  change of the user's password hash, on removal from the workspace, and on
  the minting owner losing the owner role or leaving.
- Rotating `KNOT_SESSION_KEY` invalidates every session and every API key;
  it is the break-glass revoke-everything.
