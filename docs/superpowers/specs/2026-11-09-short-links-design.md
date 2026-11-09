# Short links to pages — design

**Date:** 2026-11-09
**Status:** Draft (brainstorm), for review

## Context

People send each other knot pages in chat (Teams, Matrix). A page link is
`https://<host>/doc/<uuid>`, 36 characters of UUID, and pasted into a chat it
reads as noise. The person asking for this wants
`knot.dogfood.opendefense.cloud/<short code>` instead — "a short code is
enough", it doesn't need to say what the page is.

Everyone who receives such a link has a knot account on the same instance.
That is what makes this small: a short code is **not** a public share link.
It grants nothing; it only names a page. Whoever opens it still needs a
session and read access to the page, exactly as with the long link. So a
code can be short and guessable without being a security problem, unlike
the public `/p/<token>` link, whose token *is* the credential and is long on
purpose.

Depends on: PR #84 (SSO sign-in returns to the linked page). Without it a
signed-out colleague who follows a short link and signs in with SSO lands on
the start page. The branch builds on #84's branch and merges after it.

## Goals

- Every page can get a short link `https://<host>/<code>`.
- Opening it lands on the page — after sign-in if needed, SSO included.
- A **"Copy link"** button in the page header copies the short link to the
  clipboard. It is shown to everyone who can read the page, not only to
  owners (the existing "Share" button is owner-only, `DocPage.tsx:524`).
- A link stays valid for good: renaming, moving or archiving the page doesn't
  change it, so a link sent a month ago still works.

## Non-goals

- Boards (Excalidraw) — pages only.
- Readable/slug links (`/w/team/my-page`). An opaque code is enough.
- Replacing the long URL. The address bar keeps `/doc/<uuid>`; it stays the
  normal internal URL and every existing link keeps working.
- Public or anonymous access through a short link.
- Custom or user-chosen codes, several codes per page, expiry, revocation,
  click statistics.

## Anti-goals (would make this a failure even if it "works")

- A short link that lets someone read a page they couldn't read through the
  long link, or tells them a page exists (a restricted page, someone else's
  page) when they have no access to it.
- A code that collides with an app path (`/login`, `/tasks`, `/about`,
  `/api`, `/p`, `/setup`, …), today or when a new top-level route is added
  later.
- A link that silently breaks later (code changed, reused for another page).

## Decided

- **URL shape:** `https://<host>/<code>`, directly under the root, built from
  the server's `base_url` like public share links (`shares.rs:80`
  `url_for`).
- **Code:** opaque, short, fixed length, and it **always contains at least
  one digit**, so it can never equal an app route word. *(The digit rule and
  the following details are the designer's call, not the requester's.)*
  Proposed: 7 characters from an alphabet without look-alikes
  (`23456789abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ`, 57 symbols),
  random, re-drawn when it has no digit or collides.
- **One code per page, never changes, never reused.** Created the first time
  someone clicks "Copy link" for that page, not for all pages up front.
  Later clicks return the same code.
- **Who may create/copy:** anyone with read access to the page (viewer and
  up). They can already copy the long URL; the short one reveals nothing more.
- **Opening a link:** requires a session and read access, checked on every
  open with the same ACL as `/doc/<uuid>` (restricted subtrees included).
  Unknown code and "exists but you can't read it" answer **the same way**, so
  a code reveals nothing about pages the caller can't see.
- **Archived page:** the link keeps resolving for those who may see the
  archived page; the page itself then shows its usual archived state. A
  viewer, who gets 404 for an archived page today (`require_doc_role.rs`),
  gets the same "not found" as for an unknown code.
- **"Copy link" on an archived page** copies the page's **existing** short
  link (requester's decision). No new code is created for an archived page.
  If it never had one, the button copies the long `/doc/<uuid>` link instead
  *(designer's call; the requester only decided the case with an existing
  link)*.
- **Purged page:** its code goes with it (FK `ON DELETE CASCADE`); the code
  is not handed out again (codes are random and unique, no reuse logic).
- **Button:** "Copy link" in the page header action row
  (`web/src/features/docs/DocPage.tsx`, next to "Share"), visible to every
  reader, copies `https://<host>/<code>` and shows the existing "Copied!"
  toast (`PermissionsDialog.tsx:340` uses
  `navigator.clipboard.writeText(...).then(() => notify("info", "Copied!"))`).

## How it works (proposal)

### Storage

A new table, not a column on `documents`:

```
doc_short_links(
  code        text PRIMARY KEY,           -- the 7-char code
  doc_id      uuid NOT NULL UNIQUE REFERENCES documents(id) ON DELETE CASCADE,
  created_by  uuid NULL REFERENCES users(id) ON DELETE SET NULL,
  created_at  timestamptz NOT NULL DEFAULT now()
)
```

A separate table keeps the migration off `documents`, which the page-history
work showed is expensive to lock (see the 0.9.10 upgrade notes). `CREATE
TABLE` with a FK still takes `SHARE ROW EXCLUSIVE` on `documents` for a
moment and waits behind long transactions; the migration should use the
existing `knot_lock_tables` helper (M1.2) or otherwise bound its lock wait,
and the CHANGELOG gets a one-line upgrade note.

### API

- `POST /api/docs/{id}/short-link` — get-or-create, behind the existing
  `require_doc_role_mw` with viewer as the minimum role. Returns
  `{ code, url }`. Idempotent: a second call returns the same code
  (`INSERT … ON CONFLICT (doc_id) DO NOTHING` then read; retry on a `code`
  collision). For an archived page the middleware answers 409
  `doc.archived` to every non-GET request (`require_doc_role.rs:72-80`), so
  this never creates a code for an archived page — wanted.
- `GET /api/docs/{id}/short-link` — read-only, never creates. Returns
  `{ code, url }`, or 404 when the page has no code yet. Passes the
  middleware on archived pages (GET). The client uses it when the page is
  archived and falls back to the long URL on 404; for live pages it calls the
  POST.
- `GET /api/short-links/{code}` — authenticated; resolves the code, then
  checks read access with `acl.access(workspace, doc_id, user)` (the same
  call `docs.rs:416` uses). Returns `{ doc_id }`, or **404 `short_link.not_found`**
  for an unknown code, a malformed code, no read access, or an archived page
  the caller may not see.

### Opening `https://<host>/<code>` (proposal)

Resolve in the SPA, not with a server redirect:

1. A top-level SPA route `/:code` (after all fixed routes, inside
   `RequireAuth`). A path that doesn't match the code pattern falls through
   to today's `*` → `/` behaviour.
2. Signed out: `RequireAuth` sends the user to `/login` with the whole
   location as `from` (PR #84), SSO carries it through `return_to`, and the
   user comes back to `/<code>`.
3. Signed in: the route calls `GET /api/short-links/{code}` and replaces the
   location with `/doc/<uuid>`. On 404 it shows a small "This link doesn't
   exist or you don't have access" page.

Why not a server-side 302 to `/doc/<uuid>`: the server would have to answer
an unauthenticated request, so either it redirects anyone (anyone can map
codes to page UUIDs) or it duplicates the login redirect. Resolving after
sign-in keeps the "nothing leaks" anti-goal trivially true and reuses the
existing login flow. The server's SPA fallback (`static_files.rs`) already
serves `index.html` for unknown paths, so no server routing changes are
needed for `/<code>`.

### Client

- `web/src/features/docs/` — a `CopyLinkButton` in the header action row for
  every reader; on click `POST …/short-link`, copy `url`, toast.
  Clipboard can fail (insecure context, denied permission): fall back to
  showing the URL in a small popover to copy by hand.
- `web/src/routes.tsx` — the `/:code` route and its small resolve component.
- API client + zod validator next to the existing ones (`web/src/lib/`).

## Tests (behaviours, each written red first)

1. e2e: a reader (not owner) clicks "Copy link", the clipboard holds
   `<base>/<code>` with a 7-char code containing a digit; a second click
   gives the same link.
2. e2e: a signed-out colleague opens `<base>/<code>`, signs in with SSO
   (Dex) and lands on the page.
3. e2e / integration: a user without read access (restricted page) opening
   the code sees the same "doesn't exist" page as for an unknown code; the
   API answers 404 for both.
4. Integration: renaming and moving the page keep the code; archiving keeps
   it resolving for an editor; purging removes it.
5. e2e: on an archived page "Copy link" copies the existing short link; on
   an archived page that never had one it copies `/doc/<uuid>`, and no code
   is created.
6. Unit: the code generator only yields codes with at least one digit from
   the alphabet, and a code never equals a fixed SPA route (`routes.tsx`).

## Left to the builder

- Exact component names, file layout, where the code generator lives
  (`knot-storage` store vs. server).
- Button label/icon (suggest "Copy link" with `Link` icon from lucide) and
  the mobile layout of the action row.
- Metrics (e.g. a counter for resolves) if cheap.
- Docs: a row in `docs/superpowers/README.md`, plan + outcome doc per the
  repo's workflow, CHANGELOG entry.
