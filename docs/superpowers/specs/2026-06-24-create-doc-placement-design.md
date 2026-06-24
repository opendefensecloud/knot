# Context-aware doc creation placement — design

**Date:** 2026-06-24
**Status:** Approved (brainstorm)

## Problem

Creating a document while viewing one ignores the current doc. The sidebar
**"New document"** button (`DocTree.tsx` → `NewDocPicker` → `create.mutate(undefined)`)
and the command-palette **"Create new document"** both create at the **root level**.
The only way to nest is the per-row **"+" Add-subpage**, which nests under that
specific row. There's no way to create relative to the doc you're currently in.

## Goal

When a doc is open, let the user create the new doc **nested under it** (default) or
at the **same level** (as a sibling), chosen in the existing New-document modal.
When no doc is open, keep today's top-level behavior.

## Design

### Placement semantics

Given the currently-open doc `C` (route `/doc/:id`, looked up in the loaded
`["docs"]` list for its `title` + `parent_id`):

- **Nested under C** → `parent_id = C.id`
- **Same level as C** → `parent_id = C.parent_id` (sibling of C; top-level when C is
  itself top-level, i.e. `parent_id = null`)
- **No current doc** (e.g. home view) → `parent_id = null` (top level), no selector
  shown.

A pure helper centralizes this so it's testable and reused by both entry points:

```ts
// web/src/features/docs/placement.ts
export type Placement = "nested" | "sibling";
/** Map a placement choice + the current doc to the new doc's parent_id.
 *  `current` is null when no doc is open. */
export function placementParent(
  p: Placement,
  current: { id: string; parent_id: string | null } | null,
): string | null {
  if (!current) return null;
  return p === "nested" ? current.id : current.parent_id;
}
```

### NewDocPicker — Location selector

`NewDocPicker` gains a small **Location** radio group, rendered **only when a current
doc is provided**:

- `( ) Nested under "<C.title>"`  — **default selected**
- `( ) Same level as "<C.title>"`

The picker holds the selected `Placement` in local state. Both actions carry the
resolved parent:

- `onPickBlank(parentId: string | null)`
- `onPickTemplate(templateId, title, parentId: string | null)`

`DocTree` passes the current doc (`{ id, parent_id, title }` from
`list.data.ok.find(d => d.id === activeId)`, or `null`) into `NewDocPicker`, which
computes `placementParent(selected, current)` and hands the parent to the callbacks.
`DocTree.create` already accepts `parent_id`; `createFromTemplate` already accepts
`{ title, parent_id }`. No backend change — `parent_id` is an existing field, and
the move/cycle guards already protect the tree.

Labels truncate long titles (reuse the tree row's truncation styling). Testids:
`new-doc-loc-nested`, `new-doc-loc-sibling`.

### Command palette

`CommandPalette`'s "Create new document" nests under the current doc when one is open
(matching the picker default): compute the active doc id from the route and create
with `parent_id = activeId` (the palette default is **nested**, not the
sibling/root choice — it's the quick path). Top level when no doc is open. It already
navigates + `markDocEditMode`s the new doc.

### After create

Unchanged: navigate to `/doc/<new>`, open in edit mode (`markDocEditMode`), invalidate
`["docs"]`. The parent row expands to reveal the new child on the next tree render
(the tree builds from the refreshed list; the new node sits under its parent). If the
parent isn't auto-expanded, expand it — but the tree currently renders children
expanded by default, so no extra work expected; verify in the e2e.

## Components / files

- Create: `web/src/features/docs/placement.ts` — `Placement` + `placementParent`.
- Create: `web/src/features/docs/placement.test.ts` — unit tests.
- Modify: `web/src/features/docs/DocTree.tsx` — pass current doc to `NewDocPicker`;
  `NewDocPicker` location selector + threaded parent; callbacks carry `parentId`.
- Modify: `web/src/components/CommandPalette.tsx` — create nested under active doc.
- Test: `web/src/features/docs/DocTree.test.tsx` (or a focused NewDocPicker test);
  `e2e/flows/create-placement.spec.ts`.

## Testing

- **Unit (`placement.test.ts`):** `nested` → `C.id`; `sibling` → `C.parent_id`;
  `sibling` when `C.parent_id === null` → `null`; `current === null` → `null` for both.
- **Component:** `NewDocPicker` shows the Location selector only when a current doc is
  passed; default is "nested"; choosing "same level" + Blank calls `onPickBlank` with
  the sibling parent. (Mock the templates query; render with/without a current doc.)
- **E2E (`create-placement.spec.ts`):** open a top-level doc D; create Blank "nested"
  → new doc appears as a child of D (indented under D). Create another "same level"
  → appears as a sibling of D (top level). Assert tree structure via row
  testids/indentation. Reuse the `tree-reorder`/`new-doc-edit-mode` spec patterns.

## Out of scope / notes

- No persistence of the last-chosen placement (defaults to "nested" each open) — YAGNI.
- No backend changes; relies on the existing `parent_id` create path and the
  cycle/move guards shipped earlier.
- The per-row "+" Add-subpage is unchanged.
