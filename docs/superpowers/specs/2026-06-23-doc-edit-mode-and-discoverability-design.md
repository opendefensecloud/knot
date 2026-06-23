# Doc edit-mode, title gating, and action discoverability — design

**Date:** 2026-06-23
**Status:** Approved (brainstorm)

## Problem

Four issues raised against the document UI:

1. **New documents open in view mode.** Creating a doc lands on a read-only page;
   the user must hunt for the edit toggle before typing.
2. **The title is editable in view mode.** The title `<input>` has no gating —
   it accepts edits regardless of role or edit mode.
3. **"Remove a template / make it a normal document again" felt missing.** It
   exists, but is buried behind an unlabeled toolbar icon.
4. **"How do I manage permissions?" felt missing.** It also exists, but is reached
   only through an unlabeled "share" icon and is absent from the sidebar menu.

Investigation found #3 and #4 are **already implemented** — the real defect is
**discoverability**, not missing features. The doc toolbar is a row of seven
unlabeled icons (`🔗 ✏️ 🕐 </> ⬇️ ▦ 💬`); Permissions is a generic share icon and
the template toggle is a generic layout icon. The sidebar right-click menu offers
Rename / template-toggle / Delete but no Permissions entry.

## Scope

In scope:
- Open **all** newly-created docs (blank, command-palette, and from-template) in
  edit mode.
- Make the document **title** read-only unless the body is editable.
- Discoverability (minimal): a text **"Share"** button replacing the bare
  permissions icon, and a **"Permissions…"** entry in the sidebar right-click menu.
- Correct the sidebar template-toggle gating to **owner-only** to match the backend.

Out of scope:
- Toolbar overflow-menu redesign (considered, not chosen).
- Any backend/API changes — all backend endpoints already exist and are correct.

## Current behavior (references)

- Edit mode lives in `DocPage` state, seeded from
  `sessionStorage["knot.editMode.{id}"]` (`web/src/features/docs/DocPage.tsx:63-78`).
  A fresh doc has no key → defaults to view mode. Toggle + ⌘E are gated to
  owner/editor (`DocPage.tsx:80-90, 141-150`).
- `KnotEditor` is editable only when `role !== "viewer" && editMode`
  (`web/src/features/editor/KnotEditor.tsx:93`).
- `DocTitle` renders a plain always-editable `<input>` taking only `id` +
  `initialTitle` (`DocPage.tsx:21-48, 122`).
- Creation paths: sidebar (`DocTree.tsx:58-69`), command palette
  (`CommandPalette.tsx:121`), from-template
  (`POST /api/docs/from-template/:id`, surfaced in `DocTree.tsx` NewDocPicker).
- Permissions UI: `PermissionsDialog` at route `/doc/:id/permissions`, opened by an
  owner-only Share2 icon (`DocPage.tsx:130-140`).
- Template toggle: owner-only toolbar icon (`DocPage.tsx:184-204`) and a
  `canEdit`-gated (editor+) sidebar entry (`DocTree.tsx:316-338`). Backend
  `set_template` is **owner-only** (`routes/api/docs.rs:417`) — the sidebar gate is
  too loose.
- Sidebar role: `useEffectiveRole()` (no doc id) yields the **workspace** role;
  `canEdit = workspace === "owner" || "editor"` (`DocTree.tsx:49-50`). The tree has
  no per-doc role.

## Design

### 1. New docs open in edit mode

Seed the edit-mode flag for the new doc id **before navigating**, reusing the
existing sessionStorage mechanism that `DocPage` already reads on mount:

```ts
// after a successful create / create-from-template, before navigate:
window.sessionStorage.setItem(`knot.editMode.${created.id}`, "1");
nav(`/doc/${created.id}`);
```

Apply at every creation site:
- `DocTree.tsx` blank-create handler (`:58-69`)
- `CommandPalette.tsx` create handler (`:121-126`)
- the from-template create path (wherever it navigates to the new doc id)

Rationale: no new prop threading or router state; `DocPage` initializes from the
key it already reads (`DocPage.tsx:72-73`). Only the just-created doc is affected —
existing docs keep the safe view-mode default. The creator is always the doc owner,
so they have edit rights and the editor will be editable.

A small shared helper (e.g. `markDocEditMode(id)`) keeps the three call sites
consistent and is the single thing to unit-test.

### 2. Title follows edit mode

`DocTitle` gains an `editable: boolean` prop. The parent passes
`editable = effRole !== "viewer" && editMode` — the same predicate the editor body
uses. When not editable the `<input>` is `readOnly`, drops the rename `onBlur`
side-effect, and loses the text-cursor affordance (styling only). Layout is
unchanged so view and edit modes don't shift.

```tsx
<DocTitle key={id} id={id} initialTitle={meta.title}
          editable={effRole !== "viewer" && editMode} />
```

### 3. Discoverability (minimal)

**Toolbar — labeled Share button.** Replace the bare Share2 `Link` icon
(`DocPage.tsx:130-140`) with a text **"Share"** button (still owner-only, still
linking to the `permissions` route, keeping `data-testid="open-permissions"`).
The Share2 icon may remain inside the button as an adornment. All other toolbar
icons are unchanged.

```
[sync]  [ 🔗 Share ]  ✏️  🕐  </>  ⬇️  ▦  💬
```

**Sidebar right-click — add "Permissions…".** Add an entry that navigates to
`/doc/{node.id}/permissions`, gated to **owner** (`workspace === "owner"`). Insert
between Rename and the template toggle:

```
Rename
Permissions…          ← new, owner-only
Save as template / Remove from templates   ← gating tightened to owner-only
Delete
```

**Sidebar — tighten template-toggle gating.** Pass an `isOwner` signal
(`workspace === "owner"`) into `TreeNode` and gate both the template toggle and the
new Permissions entry on it, matching the owner-only backend. Rename/Delete keep
their existing `canEdit` gate.

**Accepted limitation:** the sidebar only knows the workspace role, so a user who
is workspace-*editor* but doc-*owner* via an explicit grant won't see Permissions /
template actions in the sidebar for that doc. They still get both from the doc
**toolbar**, which uses the per-doc `effective_role`. Documenting rather than
fixing keeps this change minimal; per-doc role in the tree is a separate effort.

## Testing

- **Unit (web):** `markDocEditMode` writes the expected sessionStorage key.
- **Unit (web):** `DocTitle` is `readOnly` when `editable={false}` and editable
  when `true`; rename fires only when editable.
- **E2E (playwright):** creating a new doc lands in edit mode (title + body
  editable immediately) without clicking the edit toggle. Note: the suite forces
  edit mode globally via `localStorage["knot.editMode.defaultOn"]`
  (`e2e/playwright.config.ts:26-29`); this test must assert the **default** path,
  so it runs without that override (or in a context where it is cleared) to prove
  new docs open editable on their own.
- **E2E (playwright):** in view mode the title input is read-only; toggling to edit
  mode makes it editable.
- **E2E (playwright):** the labeled "Share" button opens the permissions dialog;
  the sidebar "Permissions…" entry (as owner) navigates to the same route.
- Re-run `role-gating.spec.ts` to confirm viewers/editors still see the correct
  subset of controls.

## Risks

- The e2e global edit-mode override could mask the new-doc default. Mitigated by
  the dedicated test above that runs without the override.
- Tightening the sidebar template gate to owner-only is a behavior change for
  workspace-editors; this is a **fix** (the API already rejected them) and is the
  documented intent.
