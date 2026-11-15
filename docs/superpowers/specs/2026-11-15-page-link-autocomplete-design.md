# `[[` page-link autocomplete — design

Date: 2026-11-15 · Status: approved by Marco

## Intent

Linking to another page should not need the toolbar. Typing `[[` opens a page
picker; picking a page inserts the existing internal link. When the page does
not exist yet, the picker creates it as a subpage, so a wiki can be written
"from the inside out" (Wikipedia/Obsidian style).

## Goals

- `[[` in edit mode opens a popup that filters pages by title as you type
  (↑/↓, Enter, Esc, like `@` mentions).
- Candidates: pages the user can read, from the doc list the client already
  holds. Archived pages and templates are excluded. Each row shows the title
  and, smaller, its path ("Team › Urlaub") to tell same-named pages apart.
- Picking a page replaces `[[query` with a link whose text is the page's
  current title and whose href is `knot://doc/<id>` — the same storage as
  today's internal links (pill style, lock decoration unchanged).
- Last row: "Neue Unterseite „<query>“ anlegen" (UI copy follows the app's
  language). It creates a page with that title as a child of the current
  page, inserts the link to it and keeps the caret on the current page.
  Shown only when the query has at least one non-space character and the
  user may create a subpage of the current page.

## Non-goals

- `[[Page|alias]]` shorthand.
- Resolving `[[…]]` during Markdown import.
- Updating link text when a target page is renamed.

## Constraints

- No schema change; `tools/schema.json` and the Rust crates stay untouched.
- Reuse `@tiptap/suggestion` and `placeFixedPopup` as `MentionExtension` and
  `DateTimeExtension` do.
- Must not show pages the user cannot read.

## Approaches

1. **Suggestion extension over the client doc list (chosen).** Same pattern
   as `@` and `/`; instant, no new API, permission-safe because the list is
   already filtered by the server.
2. Suggestion extension backed by `/api/search`. Adds a round trip per
   keystroke and full-text noise; titles are what users type after `[[`.
3. A new `page_link` node. Needs a schema, Markdown and export change for no
   user-visible gain over the existing link mark.

## Calls made (Claude)

- Client-side title filtering over the cached doc list (approach 1).
- After creating a subpage, stay on the current page.
- The create row needs a typed query; empty `[[` shows only existing pages.

## Left to the builder

- Ranking (prefix matches before substring matches is a sensible default).
- How the create call reuses the existing "new page" mutation and cache
  invalidation.
- Popup limits (e.g. at most ~8 rows) and the empty state text.

## Tests

Test-first, one test per behaviour, shown to Marco before implementing:
filtering, inserting the link, creating and linking a subpage, no create row
without permission. Plus one Playwright flow.
