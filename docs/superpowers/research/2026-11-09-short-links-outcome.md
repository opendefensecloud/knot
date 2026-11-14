# Outcome: short links to pages

**Date:** 2026-11-09
**Spec:** [specs/2026-11-09-short-links-design.md](../specs/2026-11-09-short-links-design.md)
**Plan:** [plans/2026-11-09-short-links.md](../plans/2026-11-09-short-links.md)
**Goal:** Every page has a stable short link `https://<host>/<code>` that any reader copies from the page header and that opens the page after sign-in, under the page's normal ACL.
**Depends on:** PR #84 (SSO sign-in returns to the linked page); this branch is stacked on it.

## What landed

- **Storage** (`knot-storage/src/short_links.rs`, migration `20261109120000_doc_short_links`).
  - Table `doc_short_links(code PK, doc_id UNIQUE → documents ON DELETE CASCADE, created_by, created_at)`, with a CHECK on the code shape.
  - The migration takes its `documents` and `users` locks through `knot_lock_tables` with a 5 s lock timeout.
  - `ShortLinkStore` has three methods: `get_or_create`, `find_for_doc` and `resolve`.
  - Codes are 7 symbols from a 56-symbol alphabet without look-alikes, always with a digit, drawn from the OS CSPRNG by rejection sampling.
- **Server** (`knot-server/src/routes/api/short_links.rs`).
  - `POST /api/docs/{id}/short-link` gets or creates a page's code. `GET /api/docs/{id}/short-link` only reads it. Both sit behind `require_doc_role_mw`, so any reader may use them, and a POST on an archived page is refused with `409 doc.archived`.
  - `GET /api/short-links/{code}` resolves a code. It checks `acl.access(..).can_read()` itself and gives one `404 short_link.not_found` for unknown, malformed, restricted, no-grant and viewer-on-archived. The bodies are byte-identical, and a test pins that.
- **Web.**
  - A `CopyLinkButton` in the doc header for every reader. It falls back to the long link on an archived page without a code, and to a selectable field when the clipboard refuses.
  - A `/:code` route (`ShortLinkResolver`) inside `RequireAuth`, after the fixed routes. Paths that are not codes fall through to `/`.
  - Signed-out users go through PR #84's login return, so `/<code>` survives SSO.

## Gates

- **Tests written red first:**
  - e2e: `short-links.spec.ts`, 3 tests with Dex SSO.
  - Server integration: `short_links_integration.rs`, 7 tests.
  - Storage: `tests/short_links.rs` with 5 tests, plus 3 unit tests.
  - vitest: 8 tests across `CopyLinkButton.test.tsx` and `ShortLinkResolver.test.tsx`.
- **Suites:** see the PR description for the numbers on the final commit.

## Non-obvious

- `CREATE TABLE … REFERENCES documents … REFERENCES users` takes SHARE ROW EXCLUSIVE on both tables (the final review caught that the first version locked only `documents`). That is why the migration uses the bounded lock loop from the page-history work rather than a bare `CREATE TABLE`.
- The digit rule is what keeps codes from colliding with app routes. No fixed route name contains a digit, and a unit test checks a hand-kept list of today's route names (not read from `routes.tsx`). A future top-level route with a digit in its name would need a look here.
- Resolving in the SPA after sign-in, not with a server 302, means a signed-out request learns nothing: no page UUID, and not whether the code exists.
- The test fixture's second page needed its own sort key. Two root pages with the same key make `move_to` return `Conflict`.

## Deferred

- Boards, readable or slug links, revoking or rotating a code, click metrics.

## Carryforward

- If an instance ever hosts knot under a path prefix (`KNOT_BASE_URL=https://host/knot`), the SPA has no router `basename` yet, so `/<prefix>/<code>` won't route. This is the same limitation the existing long links have.
