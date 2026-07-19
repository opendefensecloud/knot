---
name: knot-api
description: Use when scripting or automating against a knot instance (opendefensecloud/knot self-hosted knowledge base) with curl or HTTP — logging in (local password or OIDC/SSO browser session), fixing 401 auth.session_required or 403 CSRF errors, reading/updating docs as markdown, search, tasks, checklists, or workspace export/import.
---

# knot REST API (curl recipes)

## Overview

knot (github.com/opendefensecloud/knot) has **no API tokens and no OpenAPI spec**. Every `/api/*` call needs a session cookie (`sid`) minted by `POST /auth/login`. Unsafe methods (POST/PUT/PATCH/DELETE) additionally need the CSRF double-submit header: send the value of the `csrf` cookie as `X-CSRF-Token`. GET/HEAD need only the cookie. Do NOT guess Bearer-token auth — it does not exist.

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

`POST /auth/login` only works for local-password users. For OIDC/SSO accounts (Zitadel, Dex, …) there is no headless token flow — the session is minted by a browser redirect dance. Two working options:

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
| List docs | `GET /api/docs` |
| Create doc | `POST /api/docs` body `{"title":?, "parent_id":?, "after_id":?}` |
| Doc meta | `GET /api/docs/:id` · rename/icon: `PATCH` `{"title":?, "icon":?}` |
| Archive / restore | `DELETE /api/docs/:id` · `POST /api/docs/:id/restore` — **Owner role only** (403 `acl.owner_required` for Editors) |
| Move | `POST /api/docs/:id/move` `{"parent_id":?, "after_id":?, "before_id":?}` |
| **Read doc as markdown** | `GET /api/docs/:id/markdown` → `text/markdown` |
| **Write markdown into doc** | `POST /api/docs/:id/markdown`, raw UTF-8 body ≤ 1 MB → 204 |
| Search | `GET /api/search?q=…&limit=…` (min 2 chars, limit clamped 1–20) |
| My assigned tasks | `GET /api/workspace/tasks?include_completed=true` — only tasks **assigned to the current user**; unassigned checklist items never appear here |
| Check/uncheck task | `PATCH /api/docs/:doc_id/tasks/:item_index` `{"checked":true}` — `item_index` is 0-based in doc order |
| Doc history | `GET /api/docs/:id/history` · preview `…/:seq/markdown` · `POST …/:seq/restore` |
| Grants (ACL) | `GET /api/docs/:id/grants` · `PUT/DELETE /api/docs/:id/grants/:principal` |
| Share links | `POST /api/docs/:id/shares` · `DELETE …/shares/:share_id` |
| Workspace export | `GET /api/workspace/export` → zip (markdown + manifest) |
| Workspace import | `POST /api/workspace/import` (multipart zip upload) |
| Single-doc export | `GET /api/docs/:id/export` |
| Upload attachment | `POST /api/docs/:id/blobs` · fetch: `GET /api/blobs/:id` |
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

# Check off task #2 in a doc
kmut -X PATCH "$KNOT/api/docs/$DOC_ID/tasks/2" \
  -H 'Content-Type: application/json' -d '{"checked":true}'

# Full workspace backup
kget /api/workspace/export > knot-backup.zip
```

## Gotchas

- **Markdown import merges, it does not replace** (verified live): importing into a non-empty doc prepends the new content before the existing content. For clean writes: create a new doc and import there, or restore via history afterwards.
- `401 {"code":"auth.session_required"}` → cookie jar missing/expired: re-login.
- `403 {"code":"auth.csrf","message":"missing or mismatched CSRF token"}` on POST/PUT/PATCH/DELETE → `X-CSRF-Token` header missing or ≠ `csrf` cookie value.
- `403 {"code":"acl.editor_required"}` → the user is a workspace Viewer; writes need Editor+.
- Search returns `{"results":[]}` silently for queries < 2 chars; `limit` is clamped to 20 server-side.
- Real-time editing runs over a WebSocket CRDT protocol (yrs) — not curl territory; use the markdown endpoints instead.
- First-run bootstrap / break-glass admin (no HTTP): `knot-server admin create --email … --display-name …` (password via stdin) directly against the DB (`KNOT_DATABASE_URL`).
