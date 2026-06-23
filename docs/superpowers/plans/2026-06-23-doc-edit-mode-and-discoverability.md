# Doc edit-mode, title gating, and action discoverability — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** New documents open in edit mode, the title is read-only outside edit mode, and the existing Permissions / un-template actions become discoverable.

**Architecture:** Pure frontend (`web/`) change. New docs seed the existing `sessionStorage["knot.editMode.{id}"]` flag before navigating, via one shared helper used at all three create sites. `DocTitle` gains an `editable` prop matching the editor-body predicate. The doc toolbar gets a text "Share" button; the sidebar tree gains an owner-only "Permissions…" entry and its template toggle is tightened to owner-only.

**Tech Stack:** React + TypeScript, react-router-dom, TanStack Query, Vitest (unit), Playwright (e2e). Test runner: `cd web && pnpm test` (vitest), `cd e2e && pnpm playwright test`.

**Spec:** `docs/superpowers/specs/2026-06-23-doc-edit-mode-and-discoverability-design.md`

---

## File Structure

- Create: `web/src/features/docs/editMode.ts` — `markDocEditMode(id)` helper + the `editModeKey(id)` builder (single source of truth for the sessionStorage key).
- Create: `web/src/features/docs/editMode.test.ts` — unit test for the helper.
- Modify: `web/src/features/docs/DocPage.tsx` — use `editModeKey` from the helper; add `editable` prop to `DocTitle`; replace the bare Share2 icon with a text "Share" button.
- Create: `web/src/features/docs/DocTitle.test.tsx` — unit test for title gating.
- Modify: `web/src/features/docs/DocTree.tsx` — call `markDocEditMode` before navigating at both create sites; thread `isOwner` into `TreeRow`; add "Permissions…" menu entry; gate template toggle on owner.
- Modify: `web/src/components/CommandPalette.tsx` — call `markDocEditMode` before navigating at the create action.
- Create/Modify e2e: `e2e/flows/new-doc-edit-mode.spec.ts` — new-doc-opens-in-edit-mode + title-gating coverage.

---

## Task 1: Shared edit-mode helper

**Files:**
- Create: `web/src/features/docs/editMode.ts`
- Test: `web/src/features/docs/editMode.test.ts`

- [ ] **Step 1: Write the failing test**

```ts
// web/src/features/docs/editMode.test.ts
import { afterEach, describe, expect, it } from "vitest";
import { editModeKey, markDocEditMode } from "./editMode";

afterEach(() => window.sessionStorage.clear());

describe("editMode helper", () => {
  it("builds the per-doc key", () => {
    expect(editModeKey("abc")).toBe("knot.editMode.abc");
  });

  it("marks a doc as edit-mode in sessionStorage", () => {
    markDocEditMode("abc");
    expect(window.sessionStorage.getItem("knot.editMode.abc")).toBe("1");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd web && pnpm test editMode`
Expected: FAIL — cannot find module `./editMode`.

- [ ] **Step 3: Write minimal implementation**

```ts
// web/src/features/docs/editMode.ts

/** sessionStorage key holding the per-tab edit-mode flag for a doc. */
export function editModeKey(id: string): string {
  return `knot.editMode.${id}`;
}

/**
 * Seed edit mode for a freshly-created doc so DocPage opens it editable.
 * Call this with the new doc id immediately before navigating to it.
 */
export function markDocEditMode(id: string): void {
  try {
    window.sessionStorage.setItem(editModeKey(id), "1");
  } catch {
    /* sessionStorage unavailable — DocPage falls back to view mode */
  }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cd web && pnpm test editMode`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add web/src/features/docs/editMode.ts web/src/features/docs/editMode.test.ts
git commit -m "feat(web): add markDocEditMode helper for new-doc edit mode"
```

---

## Task 2: DocPage uses the shared key

**Files:**
- Modify: `web/src/features/docs/DocPage.tsx:63` (the inline `editModeKey` const)

This removes the duplicated key string so DocPage and the helper can never drift.

- [ ] **Step 1: Add the import**

At the top of `DocPage.tsx`, alongside the existing `./docs.api` import, add:

```ts
import { editModeKey } from "./editMode";
```

- [ ] **Step 2: Replace the inline key builder**

Find (`DocPage.tsx:63`):

```ts
  const editModeKey = id ? `knot.editMode.${id}` : null;
```

Replace with:

```ts
  const editModeKeyOrNull = id ? editModeKey(id) : null;
```

Then update the two references below it (the `useState` initializer and the persist `useEffect`) from `editModeKey` to `editModeKeyOrNull`:

- `DocPage.tsx:72`: `if (!editModeKeyOrNull) return false;`
- `DocPage.tsx:73`: `return window.sessionStorage.getItem(editModeKeyOrNull) === "1";`
- `DocPage.tsx:76`: `if (!editModeKeyOrNull) return;`
- `DocPage.tsx:77`: `window.sessionStorage.setItem(editModeKeyOrNull, editMode ? "1" : "0");`
- `DocPage.tsx:78`: dependency array `}, [editMode, editModeKeyOrNull]);`

- [ ] **Step 3: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS (no errors).

- [ ] **Step 4: Commit**

```bash
git add web/src/features/docs/DocPage.tsx
git commit -m "refactor(web): DocPage reuses editModeKey helper"
```

---

## Task 3: New docs open in edit mode (three create sites)

**Files:**
- Modify: `web/src/features/docs/DocTree.tsx:58-70` (blank create) and `:165-175` (from-template)
- Modify: `web/src/components/CommandPalette.tsx:119-127` (create action)

- [ ] **Step 1: Import the helper in DocTree**

In `web/src/features/docs/DocTree.tsx`, alongside `import { docsApi } from "./docs.api";`, add:

```ts
import { markDocEditMode } from "./editMode";
```

- [ ] **Step 2: Seed edit mode in the blank-create mutation**

Find (`DocTree.tsx:66-68`):

```ts
      await qc.invalidateQueries({ queryKey: ["docs"] });
      const created = r.ok as { id: string };
      await nav(`/doc/${created.id}`);
```

Replace with:

```ts
      await qc.invalidateQueries({ queryKey: ["docs"] });
      const created = r.ok as { id: string };
      markDocEditMode(created.id);
      await nav(`/doc/${created.id}`);
```

- [ ] **Step 3: Seed edit mode in the from-template path**

Find (`DocTree.tsx:172-174`):

```ts
            await qc.invalidateQueries({ queryKey: ["docs"] });
            const created = r.ok as { id: string };
            await nav(`/doc/${created.id}`);
```

Replace with:

```ts
            await qc.invalidateQueries({ queryKey: ["docs"] });
            const created = r.ok as { id: string };
            markDocEditMode(created.id);
            await nav(`/doc/${created.id}`);
```

- [ ] **Step 4: Import the helper in CommandPalette**

In `web/src/components/CommandPalette.tsx`, add near the other feature imports:

```ts
import { markDocEditMode } from "../features/docs/editMode";
```

(Adjust the relative path if CommandPalette already imports from `../features/docs/...`; match the existing style.)

- [ ] **Step 5: Seed edit mode in the command-palette create action**

Find (`CommandPalette.tsx:121-126`):

```ts
          const r = await docsApi.create({ title: "Untitled" });
          close();
          if ("error" in r) return;
          const created = r.ok as { id: string };
          await qc.invalidateQueries({ queryKey: ["docs"] });
          void nav(`/doc/${created.id}`);
```

Replace with:

```ts
          const r = await docsApi.create({ title: "Untitled" });
          close();
          if ("error" in r) return;
          const created = r.ok as { id: string };
          await qc.invalidateQueries({ queryKey: ["docs"] });
          markDocEditMode(created.id);
          void nav(`/doc/${created.id}`);
```

- [ ] **Step 6: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add web/src/features/docs/DocTree.tsx web/src/components/CommandPalette.tsx
git commit -m "feat(web): open newly-created docs in edit mode"
```

---

## Task 4: Title is read-only outside edit mode

**Files:**
- Modify: `web/src/features/docs/DocPage.tsx:21-48` (`DocTitle`) and `:122` (call site)
- Test: `web/src/features/docs/DocTitle.test.tsx`

`DocTitle` is currently a non-exported function. Export it so it can be unit-tested.

- [ ] **Step 1: Write the failing test**

```tsx
// web/src/features/docs/DocTitle.test.tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { DocTitle } from "./DocPage";

function renderTitle(editable: boolean) {
  const qc = new QueryClient();
  return render(
    <QueryClientProvider client={qc}>
      <DocTitle id="d1" initialTitle="Hello" editable={editable} />
    </QueryClientProvider>,
  );
}

describe("DocTitle gating", () => {
  it("is read-only when not editable", () => {
    renderTitle(false);
    expect(screen.getByTestId("doc-title")).toHaveAttribute("readonly");
  });

  it("is editable when editable", () => {
    renderTitle(true);
    expect(screen.getByTestId("doc-title")).not.toHaveAttribute("readonly");
  });
});
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd web && pnpm test DocTitle`
Expected: FAIL — `DocTitle` is not exported / does not accept `editable`.

- [ ] **Step 3: Update `DocTitle`**

In `DocPage.tsx`, change the signature and `<input>` (`:21-47`):

```tsx
export function DocTitle({
  id,
  initialTitle,
  editable,
}: {
  id: string;
  initialTitle: string;
  editable: boolean;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [title, setTitle] = useState(initialTitle);

  const rename = useMutation({
    mutationFn: async (next: string) => docsApi.patch(id, { title: next }),
    onSuccess: async (r) => {
      if ("error" in r) {
        notify("error", "Couldn't rename");
        return;
      }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      await qc.invalidateQueries({ queryKey: ["doc", id] });
    },
  });

  return (
    <input
      data-testid="doc-title"
      value={title}
      readOnly={!editable}
      onChange={(e) => setTitle(e.target.value)}
      onBlur={() => { if (editable && title !== initialTitle) rename.mutate(title); }}
      placeholder="Untitled"
      className={`w-full border-none bg-transparent text-[30px] font-bold text-fg placeholder:text-fg-muted/60 focus:outline-none focus:ring-0 px-0 ${
        editable ? "" : "cursor-default"
      }`}
    />
  );
}
```

- [ ] **Step 4: Update the call site**

Find (`DocPage.tsx:122`):

```tsx
          <DocTitle key={id} id={id} initialTitle={meta.title} />
```

Replace with:

```tsx
          <DocTitle key={id} id={id} initialTitle={meta.title}
                    editable={effRole !== "viewer" && editMode} />
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cd web && pnpm test DocTitle`
Expected: PASS (2 tests).

- [ ] **Step 6: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add web/src/features/docs/DocPage.tsx web/src/features/docs/DocTitle.test.tsx
git commit -m "feat(web): make doc title read-only outside edit mode"
```

---

## Task 5: Labeled "Share" button in the doc toolbar

**Files:**
- Modify: `web/src/features/docs/DocPage.tsx:130-140`

- [ ] **Step 1: Replace the bare Share2 icon link with a labeled button**

Find (`DocPage.tsx:130-140`):

```tsx
          {effRole === "owner" && (
            <Link
              to="permissions"
              data-testid="open-permissions"
              aria-label="Permissions"
              title="Permissions"
              className="inline-flex items-center justify-center h-9 w-9 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              <Share2 size={16} aria-hidden />
            </Link>
          )}
```

Replace with:

```tsx
          {effRole === "owner" && (
            <Link
              to="permissions"
              data-testid="open-permissions"
              aria-label="Share"
              title="Share & permissions"
              className="inline-flex items-center gap-1.5 h-9 px-3 rounded text-[13px] font-medium text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150"
            >
              <Share2 size={16} aria-hidden />
              <span>Share</span>
            </Link>
          )}
```

- [ ] **Step 2: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 3: Verify visually (optional but recommended)**

Run `make dev`, open a doc you own, confirm the toolbar shows a "🔗 Share" text button that opens the permissions dialog.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/docs/DocPage.tsx
git commit -m "feat(web): label the doc Share/permissions button"
```

---

## Task 6: Sidebar — owner-only Permissions entry + tighter template gating

**Files:**
- Modify: `web/src/features/docs/DocTree.tsx` — `DocTree` (pass `isOwner`), `TreeRow` props (`:277-338`), menu render gate (`:373`).

The tree only knows the workspace role (`DocTree.tsx:49`). Gate the new owner-only actions on `workspace === "owner"`; doc-grant-only owners still reach these via the toolbar (documented limitation in the spec).

- [ ] **Step 1: Add `useNavigate` to TreeRow imports**

`react-router-dom`'s `useNavigate` is already imported at the top of `DocTree.tsx` (used by `DocTree`). No new import needed; `TreeRow` will call `useNavigate()` itself.

- [ ] **Step 2: Compute and thread `isOwner` from `DocTree`**

In `DocTree` after `const canEdit = ...` (`:50`), add:

```ts
  const isOwner = workspace === "owner";
```

Pass it to each top-level `TreeRow` (`:145-152`):

```tsx
                <TreeRow
                  key={n.id}
                  node={n}
                  depth={0}
                  activeId={activeId}
                  canEdit={canEdit}
                  isOwner={isOwner}
                  onNewChild={(pid) => create.mutate(pid)}
                />
```

- [ ] **Step 3: Accept `isOwner` in `TreeRow` props**

In the `TreeRow` prop type (`:277-285`), add `isOwner`:

```tsx
  node,
  depth,
  activeId,
  canEdit,
  isOwner,
  onNewChild,
}: {
  node: TreeNode;
  depth: number;
  activeId?: string;
  canEdit: boolean;
  isOwner: boolean;
  onNewChild: (parentId: string) => void;
}) {
```

- [ ] **Step 4: Add a navigate handle inside `TreeRow`**

At the top of the `TreeRow` body (next to `const qc = useQueryClient();`, `:286`), add:

```tsx
  const nav = useNavigate();
```

- [ ] **Step 5: Rebuild the context-menu items with owner gating**

Replace the items block (`:328-338`):

```tsx
  const items: ContextMenuItem[] = canEdit
    ? [
        { label: "Rename", testId: "ctx-rename", onSelect: () => void onRename() },
        {
          label: node.is_template ? "Remove from templates" : "Save as template",
          testId: "ctx-template",
          onSelect: () => void onToggleTemplate(),
        },
        { label: "Delete", testId: "ctx-delete", destructive: true, onSelect: () => void onArchive() },
      ]
    : [];
```

with:

```tsx
  const items: ContextMenuItem[] = [];
  if (canEdit) {
    items.push({ label: "Rename", testId: "ctx-rename", onSelect: () => void onRename() });
  }
  if (isOwner) {
    items.push({
      label: "Permissions…",
      testId: "ctx-permissions",
      onSelect: () => void nav(`/doc/${node.id}/permissions`),
    });
    items.push({
      label: node.is_template ? "Remove from templates" : "Save as template",
      testId: "ctx-template",
      onSelect: () => void onToggleTemplate(),
    });
  }
  if (canEdit) {
    items.push({ label: "Delete", testId: "ctx-delete", destructive: true, onSelect: () => void onArchive() });
  }
```

- [ ] **Step 6: Pass `isOwner` to recursive child rows**

In the recursive `TreeRow` render (`:415-422`), add `isOwner={isOwner}`:

```tsx
            <TreeRow
              key={c.id}
              node={c}
              depth={depth + 1}
              activeId={activeId}
              canEdit={canEdit}
              isOwner={isOwner}
              onNewChild={onNewChild}
            />
```

- [ ] **Step 7: Typecheck**

Run: `cd web && pnpm tsc --noEmit`
Expected: PASS.

- [ ] **Step 8: Commit**

```bash
git add web/src/features/docs/DocTree.tsx
git commit -m "feat(web): sidebar Permissions entry; owner-only template toggle"
```

---

## Task 7: E2E — new docs open editable; title gating

**Files:**
- Create: `e2e/flows/new-doc-edit-mode.spec.ts`

The suite forces edit mode globally via `localStorage["knot.editMode.defaultOn"]` (`e2e/playwright.config.ts:26-29`). This test must prove the **default** behavior, so it clears that override before creating the doc.

- [ ] **Step 1: Read an existing flow for the auth/setup pattern**

Run: `sed -n '1,40p' e2e/flows/role-gating.spec.ts`
Adopt the same import/auth/login/workspace-setup helpers it uses (the exact helper names are project-specific — reuse them verbatim rather than inventing new ones).

- [ ] **Step 2: Write the spec**

```ts
// e2e/flows/new-doc-edit-mode.spec.ts
import { expect, test } from "@playwright/test";
// Reuse the SAME setup helpers as e2e/flows/role-gating.spec.ts
// (e.g. signing in as an owner and landing in a workspace).

test("a newly-created doc opens in edit mode with an editable title", async ({ page }) => {
  // Clear the global override so we exercise production default behavior.
  await page.addInitScript(() => {
    try { window.localStorage.removeItem("knot.editMode.defaultOn"); } catch { /* ignore */ }
  });

  // ... sign in as owner and open the workspace (reuse role-gating helpers) ...

  // Create a blank doc from the sidebar.
  await page.getByTestId("new-doc").click();
  // NewDocPicker → pick "Blank". Match the picker's blank-option testid/text
  // used in the app; inspect NewDocPicker if unsure.
  await page.getByRole("button", { name: /blank/i }).click();

  // The title input must be editable immediately (not readOnly).
  const title = page.getByTestId("doc-title");
  await expect(title).toBeVisible();
  await expect(title).not.toHaveAttribute("readonly", /.*/);

  // The edit toggle should show the "Stop editing" affordance (we are in edit mode).
  await expect(page.getByTestId("toggle-edit-mode")).toHaveAttribute(
    "aria-pressed",
    "true",
  );
});

test("title is read-only after switching to view mode", async ({ page }) => {
  // ... reuse owner setup + create a doc (as above) ...
  // Toggle out of edit mode:
  await page.getByTestId("toggle-edit-mode").click();
  await expect(page.getByTestId("doc-title")).toHaveAttribute("readonly", /.*/);
});
```

> Note: the second test depends on doc-creation steps identical to the first. Repeat those steps inline (do not factor into a shared `test.beforeEach` unless the file already uses one) so each test is self-contained.

- [ ] **Step 3: Run the new spec**

Run: `cd e2e && pnpm playwright test new-doc-edit-mode`
Expected: PASS (2 tests). If selectors for the blank-doc picker differ, open `web/src/features/docs/DocTree.tsx` `NewDocPicker` (`:182+`) and match the real testid/label.

- [ ] **Step 4: Commit**

```bash
git add e2e/flows/new-doc-edit-mode.spec.ts
git commit -m "test(e2e): new docs open in edit mode; title gating"
```

---

## Task 8: Full verification

- [ ] **Step 1: Web unit tests**

Run: `cd web && pnpm test`
Expected: PASS (including `editMode` and `DocTitle` suites).

- [ ] **Step 2: Typecheck + lint**

Run: `cd web && pnpm tsc --noEmit && pnpm lint`
Expected: PASS, no warnings.

- [ ] **Step 3: Affected e2e flows**

Run: `cd e2e && pnpm playwright test new-doc-edit-mode role-gating`
Expected: PASS. `role-gating` confirms viewers/editors still see the correct control subset after the toolbar/sidebar changes.

- [ ] **Step 4: Manual smoke (recommended)**

`make dev`, then as an owner: create a doc (sidebar + ⌘K palette + from-template) → each opens editable. Toggle to view mode → title becomes read-only. Confirm the "Share" button and the sidebar right-click "Permissions…" both open the permissions dialog. As a non-owner editor, confirm the sidebar no longer offers the template toggle.

---

## Self-Review notes

- **Spec coverage:** new-doc edit mode (Tasks 1–3) ✓; title gating (Task 4) ✓; labeled Share (Task 5) ✓; sidebar Permissions + owner-only template (Task 6) ✓; tests incl. the override-clearing e2e (Tasks 1,4,7) ✓; documented workspace-role limitation honored by gating on `workspace === "owner"` ✓.
- **Naming consistency:** `editModeKey` / `markDocEditMode` used identically across Tasks 1–3; `isOwner` prop consistent across Task 6 steps; `DocTitle` `editable` prop consistent between Task 4 definition and call site.
- **No backend changes:** all touched endpoints already exist; confirmed in the spec.
