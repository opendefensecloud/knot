# Context-aware Doc Creation Placement Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans. Steps use checkbox (`- [ ]`).

**Goal:** When a doc is open, the New-document modal lets you create the new doc nested under it (default) or at the same level; the command palette nests under the current doc.

**Architecture:** Pure frontend. A `placementParent(choice, current)` helper maps a placement choice + the current doc to a `parent_id`. `NewDocPicker` gains a Location radio (shown only when a doc is open) and threads the resolved parent into the existing `create`/`createFromTemplate` calls. The command palette creates with `parent_id = active doc id`. No backend change (`parent_id` is an existing create field).

**Tech Stack:** React + TS, TanStack Query, react-router. Tests: `cd web && pnpm test` (vitest + @testing-library/react), `pnpm tsc --noEmit`; e2e `cd e2e && pnpm playwright test`.

**Spec:** `docs/superpowers/specs/2026-06-24-create-doc-placement-design.md`

---

## File Structure
- Create: `web/src/features/docs/placement.ts` — `Placement` type + `placementParent`.
- Create: `web/src/features/docs/placement.test.ts` — unit tests.
- Modify: `web/src/features/docs/DocTree.tsx` — pass current doc to `NewDocPicker`; selector; threaded parent.
- Modify: `web/src/components/CommandPalette.tsx` — create nested under the active doc.
- Test: `web/src/features/docs/NewDocPicker.test.tsx` (new); `e2e/flows/create-placement.spec.ts` (new).

---

## Task 1: `placementParent` helper

**Files:** Create `web/src/features/docs/placement.ts` + `placement.test.ts`.

- [ ] **Step 1: Write the failing test** — `web/src/features/docs/placement.test.ts`:

```ts
import { describe, expect, it } from "vitest";
import { placementParent } from "./placement";

const child = { id: "c", parent_id: "p" };
const root = { id: "r", parent_id: null };

describe("placementParent", () => {
  it("nested → the current doc's id", () => {
    expect(placementParent("nested", child)).toBe("c");
  });
  it("sibling → the current doc's parent_id", () => {
    expect(placementParent("sibling", child)).toBe("p");
  });
  it("sibling of a top-level doc → null (stays top level)", () => {
    expect(placementParent("sibling", root)).toBeNull();
  });
  it("no current doc → null for both", () => {
    expect(placementParent("nested", null)).toBeNull();
    expect(placementParent("sibling", null)).toBeNull();
  });
});
```

- [ ] **Step 2: Run → fail** — `cd web && pnpm test placement` → FAIL (module missing).

- [ ] **Step 3: Implement** — `web/src/features/docs/placement.ts`:

```ts
export type Placement = "nested" | "sibling";

/** Map a placement choice + the currently-open doc to the new doc's parent_id.
 *  `current` is null when no doc is open. "nested" files under the current doc;
 *  "sibling" files alongside it (same parent; null/top-level if current is top-level). */
export function placementParent(
  p: Placement,
  current: { id: string; parent_id: string | null } | null,
): string | null {
  if (!current) return null;
  return p === "nested" ? current.id : current.parent_id;
}
```

- [ ] **Step 4: Run → pass** — `cd web && pnpm test placement` → PASS (4).

- [ ] **Step 5: Commit**
```bash
git add web/src/features/docs/placement.ts web/src/features/docs/placement.test.ts
git commit -m "feat(docs): placementParent helper for create placement"
```

---

## Task 2: NewDocPicker location selector + threaded parent

**Files:** Modify `web/src/features/docs/DocTree.tsx`. Test: `web/src/features/docs/NewDocPicker.test.tsx`.

Context: `NewDocPicker` (`DocTree.tsx:211`) currently takes `{ onClose, onPickBlank: () => void, onPickTemplate: (id, title) => void }`. Its call site (`DocTree.tsx:185-204`) creates blank via `create.mutate(undefined)` and template via `docsApi.createFromTemplate(templateId, { title })`. `DocTree` has `activeId` (`:49`) and the doc list (`list.data.ok`). `create.mutate` accepts `parent_id?: string`.

- [ ] **Step 1: Export NewDocPicker + write the failing test**

Add `export` to `function NewDocPicker(...)`. Create `web/src/features/docs/NewDocPicker.test.tsx`:

```tsx
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { NewDocPicker } from "./DocTree";

afterEach(cleanup);

function renderPicker(props: Partial<React.ComponentProps<typeof NewDocPicker>> = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onPickBlank = vi.fn();
  render(
    <QueryClientProvider client={qc}>
      <NewDocPicker
        onClose={() => {}}
        onPickBlank={onPickBlank}
        onPickTemplate={() => {}}
        current={null}
        {...props}
      />
    </QueryClientProvider>,
  );
  return { onPickBlank };
}

describe("NewDocPicker location selector", () => {
  it("hides the selector when no doc is open", () => {
    renderPicker({ current: null });
    expect(screen.queryByTestId("new-doc-loc-nested")).toBeNull();
  });

  it("defaults to nested and passes the current doc id when a doc is open", () => {
    const { onPickBlank } = renderPicker({ current: { id: "cur", parent_id: "par", title: "Specs" } });
    expect(screen.getByTestId("new-doc-loc-nested")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("cur");
  });

  it("same-level passes the current doc's parent_id", () => {
    const { onPickBlank } = renderPicker({ current: { id: "cur", parent_id: "par", title: "Specs" } });
    fireEvent.click(screen.getByTestId("new-doc-loc-sibling"));
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("par");
  });
});
```

Run `cd web && pnpm test NewDocPicker` → FAIL (no `current` prop / no selector).

- [ ] **Step 2: Update the `NewDocPicker` signature + add the selector**

Change the component to accept `current` and a parent in the callbacks. Add the import `import { placementParent, type Placement } from "./placement";` and `useState` (already imported). Replace the signature + add state + selector:

```tsx
export function NewDocPicker({
  onClose,
  onPickBlank,
  onPickTemplate,
  current,
}: {
  onClose: () => void;
  onPickBlank: (parentId: string | null) => void;
  onPickTemplate: (templateId: string, title: string, parentId: string | null) => void;
  current: { id: string; parent_id: string | null; title: string } | null;
}) {
  const [placement, setPlacement] = useState<Placement>("nested");
  const parentId = placementParent(placement, current);
  const templates = useQuery({
    queryKey: ["templates"],
    queryFn: () => docsApi.listTemplates(),
    refetchOnMount: "always",
    staleTime: 0,
  });
  const items = templates.data && "ok" in templates.data ? templates.data.ok : [];
```

Render the selector right inside the body `<div className="p-3 ...">`, **above** the Blank button (only when `current`):

```tsx
          {current && (
            <fieldset className="mb-3 rounded border border-border p-2">
              <legend className="px-1 text-[11px] font-semibold uppercase tracking-wider text-fg-muted">
                Location
              </legend>
              <label className="flex items-center gap-2 py-1 text-sm text-fg cursor-pointer">
                <input
                  type="radio"
                  name="new-doc-location"
                  data-testid="new-doc-loc-nested"
                  checked={placement === "nested"}
                  onChange={() => setPlacement("nested")}
                />
                <span className="truncate">Nested under “{current.title}”</span>
              </label>
              <label className="flex items-center gap-2 py-1 text-sm text-fg cursor-pointer">
                <input
                  type="radio"
                  name="new-doc-location"
                  data-testid="new-doc-loc-sibling"
                  checked={placement === "sibling"}
                  onChange={() => setPlacement("sibling")}
                />
                <span className="truncate">Same level as “{current.title}”</span>
              </label>
            </fieldset>
          )}
```

Update the Blank button: `onClick={() => onPickBlank(parentId)}`. Update each template card: `onClick={() => onPickTemplate(t.id, t.title, parentId)}`.

- [ ] **Step 3: Wire the call site in `DocTree`**

Compute the current doc and pass it; thread the parent through the callbacks (`DocTree.tsx:185-204`):

```tsx
      {pickerOpen && (
        <NewDocPicker
          onClose={() => setPickerOpen(false)}
          current={(() => {
            const docs = list.data && "ok" in list.data ? list.data.ok : [];
            const c = activeId ? docs.find((d) => d.id === activeId) : undefined;
            return c ? { id: c.id, parent_id: c.parent_id, title: c.title } : null;
          })()}
          onPickBlank={(parentId) => {
            setPickerOpen(false);
            create.mutate(parentId ?? undefined);
          }}
          onPickTemplate={async (templateId, title, parentId) => {
            setPickerOpen(false);
            const r = await docsApi.createFromTemplate(templateId, {
              title,
              ...(parentId ? { parent_id: parentId } : {}),
            });
            if ("error" in r) {
              notify("error", "Couldn't create from template");
              return;
            }
            await qc.invalidateQueries({ queryKey: ["docs"] });
            const created = r.ok as { id: string };
            markDocEditMode(created.id);
            await nav(`/doc/${created.id}`);
          }}
        />
      )}
```

(Confirm `createFromTemplate`'s body type accepts `parent_id` — it does per `docs.api.ts`. The blank `create.mutate` already accepts a `parent_id` string.)

- [ ] **Step 4: Run tests + typecheck** — `cd web && pnpm test NewDocPicker placement && pnpm tsc --noEmit` → PASS.

- [ ] **Step 5: Commit**
```bash
git add web/src/features/docs/DocTree.tsx web/src/features/docs/NewDocPicker.test.tsx
git commit -m "feat(docs): New-document modal offers nested vs same-level placement"
```

---

## Task 3: Command palette nests under the current doc

**Files:** Modify `web/src/components/CommandPalette.tsx` (create action ~`:118-128`).

Context: the create action does `docsApi.create({ title: "Untitled" })` (root). The palette mounts globally, so derive the active doc id from the route path rather than `useParams`.

- [ ] **Step 1: Derive the active doc id from the location**

Ensure `useLocation` is imported from `react-router-dom` (alongside the existing router imports). Near the top of the component, add:

```ts
  const loc = useLocation();
  const activeDocId = loc.pathname.match(/^\/doc\/([^/]+)/)?.[1];
```

- [ ] **Step 2: Create under the active doc**

Change the create action body:

```ts
          const r = await docsApi.create(
            activeDocId ? { title: "Untitled", parent_id: activeDocId } : { title: "Untitled" },
          );
          close();
          if ("error" in r) return;
          const created = r.ok as { id: string };
          await qc.invalidateQueries({ queryKey: ["docs"] });
          markDocEditMode(created.id);
          void nav(`/doc/${created.id}`);
```

(If `activeDocId` is a dep of the `useMemo`/`useCallback` that builds the actions list, add it to the dependency array so the action closes over the current value.)

- [ ] **Step 3: Typecheck + tests** — `cd web && pnpm tsc --noEmit && pnpm test` → PASS (no behavior change to existing tests).

- [ ] **Step 4: Commit**
```bash
git add web/src/components/CommandPalette.tsx
git commit -m "feat(docs): command palette creates under the current doc"
```

---

## Task 4: E2E — placement from an open doc

**Files:** Create `e2e/flows/create-placement.spec.ts`.

- [ ] **Step 1: Read the patterns** — `sed -n '1,60p' e2e/flows/new-doc-edit-mode.spec.ts` and `e2e/flows/tree-reorder.spec.ts` for the owner-setup, the `new-doc` → `new-doc-modal` → `new-doc-blank` flow, and how rows (`doc-row-<id>`) / nesting are asserted. Reuse the DB-reset + `/setup` owner helpers verbatim.

- [ ] **Step 2: Write the spec**

```ts
import { expect, test } from "@playwright/test";
// Reuse owner setup + the new-doc flow helpers from new-doc-edit-mode.spec.ts.

test("new doc nests under the current doc by default", async ({ page }) => {
  // ... sign in as owner; create a first top-level doc "Parent" and open it ...
  // Open the picker, assert the Location selector shows and "nested" is selected,
  // pick Blank:
  await page.getByTestId("new-doc").click();
  await expect(page.getByTestId("new-doc-loc-nested")).toBeChecked();
  await page.getByTestId("new-doc-blank").click();
  // The new doc opens; the sidebar shows it as a CHILD of "Parent"
  // (assert via indentation/order of [data-testid^="doc-row-"], or that the
  // new row is nested under the Parent row).
});

test("same-level creates a sibling of the current doc", async ({ page }) => {
  // ... open "Parent" (a top-level doc) ...
  await page.getByTestId("new-doc").click();
  await page.getByTestId("new-doc-loc-sibling").click();
  await page.getByTestId("new-doc-blank").click();
  // The new doc is top-level (sibling of Parent), not nested under it.
});
```

Fill in the seed/open/create steps to match the sibling specs; assert tree structure via the `doc-row-*` testids (depth/order). Keep assertions robust (don't over-fit pixel indentation — check parent/child relationship via DOM nesting or the row order + the absence/presence under the parent).

- [ ] **Step 3: Run** — `cd e2e && pnpm playwright test create-placement` → PASS. (If the full suite is flaky, run this spec alone; `make db.cleanup` first if needed.)

- [ ] **Step 4: Commit**
```bash
git add e2e/flows/create-placement.spec.ts
git commit -m "test(e2e): create placement nested vs same-level"
```

---

## Task 5: Verification
- [ ] `cd web && pnpm test && pnpm tsc --noEmit` → all pass (incl. `placement`, `NewDocPicker`).
- [ ] `cd web && pnpm lint` → no NEW errors (compare to the 4 pre-existing).
- [ ] `cd e2e && pnpm playwright test create-placement new-doc-edit-mode` → PASS.
- [ ] Manual: `make dev`; open a doc, click New document → confirm "Nested under …" default; create → appears as a child; repeat with "Same level" → sibling; with no doc open, the selector is absent and create is top-level; ⌘K → "Create new document" while in a doc → nests under it.

---

## Self-Review notes
- **Spec coverage:** `placementParent` (Task 1) ✓; picker Location selector + threaded parent for Blank & Template (Task 2) ✓; palette nests under current (Task 3) ✓; unit + component + e2e tests (Tasks 1,2,4) ✓; no-current → top-level + no selector (Task 2 test) ✓.
- **Naming consistency:** `Placement` (`"nested" | "sibling"`), `placementParent`, `current: { id, parent_id, title }`, testids `new-doc-loc-nested`/`new-doc-loc-sibling` used consistently across tasks.
- **No backend change:** uses the existing `parent_id` on `create`/`createFromTemplate`; cycle/move guards already shipped.
