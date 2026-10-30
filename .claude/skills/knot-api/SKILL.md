---
name: knot-api
description: Use when scripting or automating against a knot instance (opendefensecloud/knot self-hosted knowledge base) with curl or HTTP — logging in (local password or OIDC/SSO browser session), fixing 401 auth.session_required or 403 CSRF errors, reading/updating docs as markdown, moving or duplicating docs, restricting docs and granting access, search, tasks, checklists, or workspace export/import.
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
| Grants (ACL) | `GET /api/docs/{id}/grants` · `PUT /api/docs/{id}/grants/{principal}` `{"role":"owner\|editor\|viewer","inherit":true}` → 204 · `DELETE …/grants/{principal}` → 204. Listing needs read access, changes need Owner on the page. `principal` is `user:<user_id>` of a current workspace member. `inherit:false` covers that page only, not its subpages |
| **Restrict a doc** | `PUT /api/docs/{id}/restricted` `{"restricted":true}` → 200 with the doc — Owner (effective role on the page). Only people with a grant on the page or its subpages keep access; workspace roles, the workspace owner's included, stop applying inside. Restricting makes you an `owner` (inherit) of the page and revokes the public links of the page and its subpages. `{"restricted":false}` lifts it. Both are idempotent |
| **Who has access** | `GET /api/docs/{id}/access` → `{"restricted", "restriction_root_id", "restriction_root_title", "people":[{"principal","role","inherit","source_doc_id","source_title"}]}` — any reader. `people` are the grants that apply: the page's own plus inherited ones from parents, stopping at the restricted page. Ids and titles of pages you can't read are `null` |
| Share links | `POST /api/docs/{id}/shares` · `DELETE …/shares/{share_id}` |
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
