---
name: knot-api
description: Use when scripting or automating against a knot instance (opendefensecloud/knot self-hosted knowledge base) with curl or HTTP — logging in (local password or OIDC/SSO browser session) or using scoped API keys (Authorization: Bearer knot_…, bot accounts), fixing 401 auth.session_required / api_key.invalid or 403 CSRF / api_key.scope_denied errors, reading/updating docs as markdown, moving or duplicating docs, restricting docs and granting access, search, tasks, checklists, or workspace export/import.
---

# knot REST API (curl recipes)

## Overview

knot (github.com/opendefensecloud/knot) has **no OpenAPI spec**. Every `/api/*` call authenticates one of two ways:

- **API key** (best for scripts, CI and AI agents): `Authorization: Bearer knot_…`. No cookie, no CSRF header. Works on `/api/*` only. See [API keys](#api-keys).
- **Session cookie** (`sid`) minted by `POST /auth/login`. Unsafe methods (POST/PUT/PATCH/DELETE) additionally need the CSRF double-submit header: send the value of the `csrf` cookie as `X-CSRF-Token`. GET/HEAD need only the cookie. Everything outside `/api/` (`/auth/*`, the `/collab/*` websocket, `/p/*`) is cookie-only.

## API keys

A key acts as the user (or bot) it belongs to, in that user's workspace, with that user's page access, narrowed by the key's scopes.

- **Create** in the app under Settings → API keys (a workspace owner manages a bot's keys under Members), or with `POST /api/auth/api-keys` `{"name":"ci","scopes":["read","update"],"expires_in_days":30}` from a **session** (cookie + CSRF) → 201 `{"id","key","name","scopes","expires_at"}`. `key` is shown in this response **only**; it can't be fetched again, so store it now. An owner adds `"user_id":"<bot id>"` to mint a key for a bot.
- **List / revoke**: `GET /api/auth/api-keys` → your keys in this workspace, newest first: the live ones and those revoked or expired in the last 30 days (`id, name, scopes, created_by, created_at, expires_at, last_used_at, revoked_at`; never the key itself) · `DELETE /api/auth/api-keys/{id}` → 204. An owner adds `?user_id=<bot id>` to either for a bot's keys.
- **At most 50 live keys** per user or bot: the 51st → `409 api_key.limit_reached`; revoke one first (revoked and expired keys don't count).
- **Session-only routes.** Calling `/api/auth/api-keys`, inviting, re-roling or removing members (`POST /api/workspace/members`, `PATCH`/`DELETE /api/workspace/members/{id}`), or creating or listing public share links (`POST`/`GET /api/docs/{id}/shares`) with a key → `403 api_key.session_required`, whatever its scopes, so a key can never mint another key or a public link, widen its own scopes or make itself an owner. Listing members (`GET`) works with `read`; revoking a share link (`DELETE …/shares/{share_id}`) with `delete`. Another user's keys → `403 api_key.foreign_forbidden` (only an owner, and only for the bots of their workspace); an unknown id or someone else's key → `404 api_key.not_found`. A key request whose session ended meanwhile (password change, logout, removal) → `401 auth.session_required`.
- **Scopes** (any non-empty subset; independent, `update` does not imply `read`; empty or unknown names → `422 api_key.scopes_invalid`):

  | Scope | Covers |
  |---|---|
  | `read` | GET/HEAD, plus `POST /api/docs/{id}/export/{format}` (renders a file, writes nothing) |
  | `create` | every other POST: new pages, duplicates, uploads, comments, replies, suggestions (not the session-only routes above) |
  | `update` | PATCH/PUT, plus the POSTs `…/move`, `…/markdown` (import), `…/restore`, `…/history/{seq}/restore`, `…/template`, `…/comments/{thread_id}/resolve` and `/unresolve`, `…/suggestions/{sid}/accept`, `/reject`, `/withdraw`, `/api/notifications/read`, `/api/whats-new/seen` |
  | `delete` | DELETE (archive, grants, share links, …; not the session-only routes above), plus `POST /api/docs/{id}/purge` |

  Missing scope → `403 {"error":{"code":"api_key.scope_denied","details":{"scope":"update"}}}` naming the scope it needed. A script that reads a page and writes it back as markdown needs `read` + `update`. So does an agent that reads its tasks, ticks them and writes progress into existing pages; opening a new page (for a handoff, say) or leaving a comment is a create and needs `create` too.
- **Lifetime**: `expires_in_days` ≥ 30 (`400 api_key.ttl_too_short`), or `null`/omitted for no expiry. SSO-only users (no local password) must pick 30–90 days (`400 api_key.ttl_too_long`, also enforced by the database). Bots and local-password users may have keys that never expire.
- **Bots** are credentialless members for automation: an owner creates one with `POST /api/workspace/members` `{"email":"ci-bot@example.com","display_name":"CI bot","role":"editor","kind":"bot"}` (role `editor` or `viewer` only, `422 bot.owner_forbidden` otherwise; no password, no login), then mints its keys with `"user_id"`. `GET /api/workspace/members` shows `"kind":"human"|"bot"`. A bot can never own the workspace or a page: owner grants → `422 bot.owner_forbidden`, restricting a page → `403 acl.owner_required` (a bot never holds the page-owner role that needs), creating a page (also from a template or by workspace import) under a restricted parent, importing restricted pages, or duplicating a restricted page → `403 bot.restricted_forbidden`. A bot can't sign in through SSO either, even when an IdP account has its email. A removed bot can't be re-invited (`409 workspace.email_taken`); add a new one.
- **Not with a key**: anything outside `/api/` — the `/collab/*` live-editing websocket, `/auth/session`, `/auth/password`, `/auth/logout`, public links. Write page content through `POST /api/docs/{id}/markdown` instead of the websocket.
- **Keys die** when revoked, when they expire, when their user's password hash changes (all of that user's keys, whether the app or a direct database update changed it), when the user is removed from the workspace (revoked: re-inviting the user doesn't bring them back), when the owner who minted a bot's key leaves the workspace or stops being an owner, and when the server's `KNOT_SESSION_KEY` is rotated. Every one of these answers `401 api_key.invalid`, the same as a typo. A Bearer header is never retried as a cookie session.

```bash
KNOT=https://knot.example.com
export KNOT_API_KEY=knot_…              # from Settings → API keys; keep it out of argv/history
kapi() { curl -sf -H "Authorization: Bearer $KNOT_API_KEY" "$@"; }

kapi "$KNOT/api/docs" | jq '.[] | {id, title}'                       # read
kapi -X POST "$KNOT/api/docs/$DOC_ID/markdown" \
  -H 'Content-Type: text/markdown' --data-binary @page.md            # update
```

The cookie recipes below work the same with a key: replace `kget`/`kmut` by `kapi` and drop `-b "$JAR"` / `X-CSRF-Token`.

## Login + helpers (run once; session lives 30 days)

```bash
KNOT=https://knot.example.com          # base URL, no trailing slash
JAR=$(mktemp)

# → 204 No Content, sets cookies: sid (HttpOnly) + csrf
curl -sf -c "$JAR" -X POST "$KNOT/auth/login" \
  -H 'Content-Type: application/json' \
  -d "{\"email\":\"$KNOT_EMAIL\",\"password\":\"$KNOT_PASSWORD\"}"

CSRF=$(awk '$6=="csrf" {print $7}' "$JAR")

kget() { curl -sf -b "$JAR" "$KNOT$1"; }                          # safe methods
kmut() { curl -sf -b "$JAR" -H "X-CSRF-Token: $CSRF" "$@"; }      # unsafe methods
```

Keep the password in an env var, never in argv/history. Failed logins are throttled per IP + email (1 s delay, lockout after repeated failures) — don't retry in a loop. `kget /auth/session` returns the current user/workspace as JSON. Cookies carry `Secure` by default; plain-http dev instances need `KNOT_COOKIE_SECURE=false` server-side.

`GET /auth/config` (no auth needed) tells you what login methods the instance offers: `{"setup_available":…,"oidc_enabled":…,"password_login_enabled":…}`.

## OIDC-only accounts (no local password)

`POST /auth/login` only works for local-password users. For OIDC/SSO accounts (Zitadel, Dex, …) there is no headless login flow — the session is minted by a browser redirect dance. Simplest: log in once in the browser, create an API key under Settings (max 90 days for SSO users) and script with that. Otherwise, two working options:

- **Drive the API from the browser session** (verified live): after the user logs in via SSO, run `fetch` in the page context — the `sid` cookie rides along automatically, and the `csrf` cookie is deliberately NOT HttpOnly, so `document.cookie` can read it:
  ```js
  const csrf = document.cookie.split('; ').find(c => c.startsWith('csrf='))?.split('=')[1];
  await fetch('/api/docs', {method: 'POST',
    headers: {'X-CSRF-Token': csrf, 'Content-Type': 'application/json'},
    body: JSON.stringify({title: 'New doc'})});
  ```
  With Claude in Chrome, use `javascript_tool` on a logged-in knot tab.
- **Break-glass local admin** (needs deploy access, not HTTP): `knot-server admin create` inside the pod/container mints a local-password owner even when SSO is the primary login.

## Endpoint quick reference

| Action | Call |
|---|---|
| List docs | `GET /api/docs` → only pages you can read, each with `effective_role` (`owner`/`editor`/`viewer`), `restricted`, `within_restricted` and `parent_hidden`. A page whose parent you can't read has `parent_id: null, parent_hidden: true` (the app lists it under "Shared with you") |
| Create doc | `POST /api/docs` body `{"title":?, "parent_id":?, "after_id":?}` → 201. No `after_id`: first among `parent_id`'s children (top level when omitted); with it: right after that sibling |
| Doc meta | `GET /api/docs/{id}` (adds `restricted`, `within_restricted`, `restriction_root_id`) · rename/icon: `PATCH` `{"title":?, "icon":?}` |
| Archive / restore | `DELETE /api/docs/{id}` (with its subpages) · `POST /api/docs/{id}/restore` (an archive root only) → 204 — Editor+ · delete for good: `POST /api/docs/{id}/purge` — Owner only |
| Move | `POST /api/docs/{id}/move` `{"parent_id":?, "after_id":?, "before_id":?}` → 200. Right after `after_id` (it wins when both are set), else right before `before_id`, else last among `parent_id`'s children. `parent_id` omitted or `null` = top level |
| **Duplicate doc** | `POST /api/docs/{id}/duplicate` `{"include_subpages":false}` → 201 with the copy, placed right after the original as `"<title> (copy)"`. `include_subpages` (default `false`) copies its live subpages too. Drawings and files the pages show (those you can read) get copies of their own; links between copied pages point at the copies. Editor+ |
| **Read doc as markdown** | `GET /api/docs/{id}/markdown` → `text/markdown` |
| **Write markdown into doc** | `POST /api/docs/{id}/markdown`, raw UTF-8 body ≤ 1 MB → 204 |
| Search | `GET /api/search?q=…&limit=…` (min 2 chars, limit clamped 1–20) |
| My assigned tasks | `GET /api/workspace/tasks?include_completed=true` — only tasks **assigned to the current user**; unassigned checklist items never appear here |
| Check/uncheck task | `PATCH /api/docs/{doc_id}/tasks/{item_index}` `{"checked":true}` — `item_index` is 0-based in doc order |
| Doc history | `GET /api/docs/{id}/history` · preview `…/{seq}/markdown` · `POST …/{seq}/restore` |
| Grants (ACL) | `GET /api/docs/{id}/grants` · `PUT /api/docs/{id}/grants/{principal}` `{"role":"owner\|editor\|viewer","inherit":true}` → 204 · `DELETE …/grants/{principal}` → 204. Listing needs read access, changes need Owner on the page. `principal` is `user:<user_id>` of a current workspace member, stored as the lower-case hyphenated id whatever spelling you send. `inherit:false` covers that page only, not its subpages |
| **Restrict a doc** | `PUT /api/docs/{id}/restricted` `{"restricted":true}` → 200 with the doc — Owner (effective role on the page). Only people with a grant on the page or its subpages keep access; workspace roles, the workspace owner's included, stop applying inside. Restricting makes you an `owner` (inherit) of the page and revokes the public links of the page and its subpages. `{"restricted":false}` lifts it. Both are idempotent |
| **Who has access** | `GET /api/docs/{id}/access` → `{"restricted", "restriction_root_id", "restriction_root_title", "people":[{"principal","role","inherit","source_doc_id","source_title"}]}` — any reader. `people` are the grants that apply: the page's own plus inherited ones from parents, stopping at the restricted page. Ids and titles of pages you can't read are `null` |
| **Comments** | `GET /api/docs/{id}/comments?include_resolved=true` · new thread `POST /api/docs/{id}/comments` `{"body":"…","mentions":["<user_id>"]}` → 201 · reply `POST /api/docs/{id}/comments/{thread_id}/replies` (same body) · `POST …/{thread_id}/resolve` — `mentions` (optional) are the members to notify; without it only `@Name` matching a display name with no spaces notifies anyone. Members who can't read the page are never notified |
| **Change suggestions** | `GET /api/docs/{id}/suggestions?include_resolved=true` → `[{"id","kind":"replace\|insert\|delete","status":"pending\|applying\|accepted\|rejected\|withdrawn","original_text","proposed_text","reason","author_id","decided_by",…,"replies":[…]}]` · accept `POST …/suggestions/{sid}/accept` `{"seen_text":"<text the range holds now>"}` → 200 (Owner; 409 `suggestion.changed` if the page no longer holds `seen_text` there, 409 `suggestion.anchor_lost` if the text is gone; `""` for an insertion) · `POST …/{sid}/reject` (Owner) · `POST …/{sid}/withdraw` (its author) · replies `POST …/{sid}/replies` `{"body","mentions":[…]}`, `PATCH`/`DELETE …/replies/{reply_id}`. Creating one (`POST /api/docs/{id}/suggestions`, Editor+) needs Yjs RelativePosition anchors (`position_y`, `position_y_end`, base64) that only the editor can compute, so script reads and decisions, not new suggestions. The page changes only when an owner accepts |
| **Inbox** | `GET /api/notifications?filter=unread\|all&limit=50&cursor=<id>` → `{"items":[{"id","kind","doc_id","doc_title","target_kind","target_id","actor_display_name","data":{"excerpt",…},"created_at","read"}],"next_cursor"}`, newest first; pass `next_cursor` as `cursor` for the next page · `GET /api/notifications/unread_count` → `{"count","capped"}` (counts to 100) · `POST /api/notifications/read` `{"ids":[…]}` or `{"all":true}` → 204. `kind` is `mention`, `reply`, `task_assigned`, `task_due`, `doc_shared`, `suggestion` (a change suggested on a page you own) or `suggestion_decided` (yours was accepted or rejected). Only your own notifications, about pages you can read now |
| Share links | `POST /api/docs/{id}/shares` · `GET …/shares` · `DELETE …/shares/{share_id}` — create and list from a session only, not with an API key |
| Workspace export | `GET /api/workspace/export` → zip (markdown + manifest) |
| Workspace import | `POST /api/workspace/import` (multipart zip upload) — Owner; with `?parent_id=<page>` the import goes under that page, Editor+ on it |
| Single-doc export | `GET /api/docs/{id}/export` |
| **Export doc as Word / OpenDocument** | `POST /api/docs/{id}/export/docx` or `…/export/odt` → the file; body empty, or multipart with one PNG per diagram key (≤ 10 MiB total, ≤ 2 MiB and ≤ 100 parts). Header `X-Knot-Export-Fallbacks: diagrams=…, images=…, unknown=…` counts what was left out. Viewer+ |
| Diagrams to render for that export | `GET /api/docs/{id}/export/diagrams` → `{"limits":{…}, "diagrams":[{"key","kind":"mermaid","source"} \| {"key","kind":"board","board_id","label"}]}` |
| Upload attachment | `POST /api/docs/{id}/blobs` · fetch: `GET /api/blobs/{id}` |
| Workspace info | `GET /api/workspace` · templates: `GET /api/workspace/templates` |

## Core recipes

```bash
# Read a doc as markdown
kget "/api/docs/$DOC_ID/markdown" > doc.md

# Push markdown into a doc (raw body — NOT a JSON envelope)
kmut -X POST "$KNOT/api/docs/$DOC_ID/markdown" \
  -H 'Content-Type: text/markdown' --data-binary @notes.md

# Create a fresh doc, capture its id, then import markdown into it
DOC_ID=$(kmut -X POST "$KNOT/api/docs" -H 'Content-Type: application/json' \
  -d '{"title":"Imported notes"}' | jq -r '.id')

# Search
kget "/api/search?q=deployment&limit=20" | jq '.results'

# Move a doc under $PARENT_ID, right after its child $SIBLING_ID
kmut -X POST "$KNOT/api/docs/$DOC_ID/move" -H 'Content-Type: application/json' \
  -d "{\"parent_id\":\"$PARENT_ID\",\"after_id\":\"$SIBLING_ID\"}"

# Duplicate a doc with its subpages, capture the copy's id
COPY_ID=$(kmut -X POST "$KNOT/api/docs/$DOC_ID/duplicate" \
  -H 'Content-Type: application/json' -d '{"include_subpages":true}' | jq -r '.id')

# Restrict a doc and its subpages, then let one member read just one subpage
kmut -X PUT "$KNOT/api/docs/$DOC_ID/restricted" \
  -H 'Content-Type: application/json' -d '{"restricted":true}'
MEMBER_ID=$(kget /api/workspace/members | jq -r '.[] | select(.email=="ana@example.com") | .user_id')
kmut -X PUT "$KNOT/api/docs/$SUBPAGE_ID/grants/user:$MEMBER_ID" \
  -H 'Content-Type: application/json' -d '{"role":"viewer","inherit":false}'
kget "/api/docs/$SUBPAGE_ID/access" | jq '.people'

# Check off task #2 in a doc
kmut -X PATCH "$KNOT/api/docs/$DOC_ID/tasks/2" \
  -H 'Content-Type: application/json' -d '{"checked":true}'

# Full workspace backup
kget /api/workspace/export > knot-backup.zip

# Export a doc as Word (or …/export/odt). An empty body means no diagram
# pictures: Mermaid blocks and boards become "(not included)" placeholders.
kmut -X POST "$KNOT/api/docs/$DOC_ID/export/docx" -D headers.txt -o page.docx
grep -i '^x-knot-export-fallbacks' headers.txt
```

## Gotchas

- **Markdown import merges, it does not replace** (verified live): importing into a non-empty doc prepends the new content before the existing content. For clean writes: create a new doc and import there, or restore via history afterwards.
- `401 {"code":"auth.session_required"}` → cookie jar missing/expired: re-login.
- `401 api_key.invalid` → the Bearer key is malformed, unknown, expired, revoked, its user's password changed, or the user left the workspace; one answer for all. Make a new key from a session.
- `403 api_key.scope_denied` → the key lacks `details.scope`; make a key with it (scopes can't be added to an existing key).
- `403 {"code":"auth.csrf","message":"missing or mismatched CSRF token"}` on POST/PUT/PATCH/DELETE → `X-CSRF-Token` header missing or ≠ `csrf` cookie value.
- `403 {"code":"acl.editor_required"}` → the user is a Viewer of the page (or of its parent, for create, move and duplicate), or a workspace Viewer for top-level pages; writes need Editor+.
- **No-access is uniform, so there is no existence oracle.** A page you can't read answers exactly like an unknown id: `403 acl.no_grant` on every `/api/docs/{id}…` route, `404 doc.not_found` as a `parent_id` in a request body (create, move, from-template, import), `422 doc.bad_anchor` as an `after_id` / `before_id`. Listings (docs, search, templates, archive, tasks, export) just leave it out, and a parent you can't read comes back as `parent_id: null`. A public link to a page in a restricted subtree answers exactly like a revoked one: `410` on `/p/{token}`, `404` on its blob and board routes.
- Restricted pages: `403 acl.owner_required` (restrict / unrestrict needs Owner on the page), `409 grant.last_owner` (on a restricted page you can't remove or downgrade its last `owner` grant), `422 grant.not_member` (a `user:` principal must be a current workspace member), `403 acl.restricted_move` (moving a page into, out of, or between restricted subtrees needs Owner on the moved page), `409 doc.hidden_descendants` (archive or purge of a page with subpages you can't see; ask their owners to move them first), `409 doc.restore_restricted_parent_archived` (restoring would take the page out of its restricted parent, which is archived; restore that parent first), `409 share.restricted` (no public links inside a restricted subtree).
- Removing a member deletes their page grants. Re-inviting them (or their next SSO login) does not bring access back.
- `403 acl.no_grant` on a `/api/docs/{id}…` route (move, duplicate, restore, from-template, …) → `{id}` is no page you can see: an unknown id and a page of another workspace both answer this, never 404.
- Create / move: `422 doc.bad_anchor` → `after_id` / `before_id` is not a live, non-template sibling under the target parent, or is the moved doc itself (it used to fall back to last). `404 doc.not_found` → the `parent_id` is not a page of your workspace; a page of another workspace counts as unknown, also for `POST /api/docs/from-template/{id}`. `409 doc.archived` → the parent (or the moved doc) is in the archive. Move only: `409 doc.move_cycle` → the parent is the doc itself or one of its subpages. `409 doc.sort_key_conflict` → another writer took the key (e.g. an older pod during a rolling upgrade); retry.
- Restore: `409 doc.not_archive_root` (restore its archive root instead), `409 doc.purging` (a purge has started), `409 doc.sort_key_conflict` (its top-level place was taken; retry) and `409 doc.restore_conflict` (it was restored, moved and archived again meanwhile; retry).
- Duplicate: `403 acl.no_grant` (an unknown page, or one in another workspace, even for a member of both; not 404), `403 acl.editor_required` (Viewer on the page or the workspace), `409 doc.archived` (the page is in the archive), `409 doc.duplicate_conflict` (while it was copied, the page was moved or archived, or a file it shows was purged or is missing; retry), `409 doc.sort_key_conflict` (retry), `413 doc.duplicate_too_large` (over 200 pages, the page included, or 200 MiB of files), `422 doc.is_template` (a template can't be duplicated; create a page from it with `POST /api/docs/from-template/{id}`), `503 doc.duplicate_busy` (with `Retry-After: 5`; that pod is already making two copies and none ended within 5 s — wait and retry). The copy is all or nothing.
- Workspace import with `?parent_id=` of an unknown page or one in another workspace: `404 doc.not_found` (was `403 acl.editor_required`).
- Search returns `{"results":[]}` silently for queries < 2 chars; `limit` is clamped to 20 server-side.
- Word/OpenDocument export: `503 export.busy` (with `Retry-After: 5`) when two exports already run on that pod — wait and retry; `413 export.too_large` when the multipart body passes 10 MiB — resend with smaller or no pictures. A picture part over 2 MiB, past the 100th part, or named after no diagram on the page is ignored, not an error.
- Real-time editing runs over a WebSocket CRDT protocol (yrs) — not curl territory; use the markdown endpoints instead.
- First-run bootstrap / break-glass admin (no HTTP): `knot-server admin create --email … --display-name …` (password via stdin) directly against the DB (`KNOT_DATABASE_URL`).
- Locked out of a restricted page (its last owner left the workspace)? There is no in-app override. With DB access: `knot-server admin restricted-list --workspace <slug>` prints each restricted page as `<id>\t<title>\towners=<emails>`, and `knot-server admin grant --doc <id> --email <member> --role owner [--no-inherit]` grants a current member access. The grant is audited as `admin.grant` with `break_glass: true`.
