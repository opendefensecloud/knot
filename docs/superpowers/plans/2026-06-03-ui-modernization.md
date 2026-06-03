# UI Modernization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the prototype-grade inline-styled UI with a cohesive, Outline/Docmost-style workspace using a real design system: slate-neutral palette, Inter typography, Lucide icons, Tailwind CSS for tokens, bubble editor toolbar, refined sidebar, and full light/dark mode — all without touching backend, schema, or e2e behavior.

**Architecture:** Tailwind CSS with CSS-variable-backed tokens (`bg`, `surface`, `border`, `fg`, `fg-muted`, `accent`) and a `[data-theme="dark"]` switch. All inline `style={}` migrated to Tailwind classes. Icons via `lucide-react` (tree-shaken). Inter via `@fontsource-variable/inter`. The editor toolbar moves from a permanent strip to a Tiptap BubbleMenu shown on selection. AppShell gains a workspace block at the top of the sidebar and a settings/user footer; DocTree gains hover-reveal actions; DocPage gains a breadcrumb + overflow menu and the action row becomes icon-buttons in the top-right. All `data-testid` attributes are preserved so the existing 26/26 e2e suite continues to pass.

**Tech Stack:** Tailwind CSS 3.4, `lucide-react` ^0.460, `@fontsource-variable/inter` ^5, `@tiptap/extension-bubble-menu`, Zustand (for theme), existing React 18 + Vite + Tiptap stack.

**Design system (from `ui-ux-pro-max --design-system`):**
- **Style:** Minimalism & Swiss (light-first, dark full)
- **Typography:** Inter (300/400/500/600/700), system fallback; reading column 720px max-width
- **Palette (light):** primary `#475569`, accent `#2563EB`, bg `#F8FAFC`, surface `#FFFFFF`, fg `#1E293B`, fg-muted `#64748B`, border `#E2E8F0`, muted `#EAEFF3`, destructive `#DC2626`
- **Palette (dark):** primary `#94A3B8`, accent `#3B82F6`, bg `#0B1220`, surface `#0F172A`, fg `#F1F5F9`, fg-muted `#94A3B8`, border `#1E293B`, muted `#1E293B`, destructive `#F87171`
- **Radius scale:** 4 / 6 / 8 / 12. **Spacing:** Tailwind defaults (4px base). **Easing:** `cubic-bezier(0.16, 1, 0.3, 1)` for state changes, 150–250 ms.

**Constraint — non-breaking:** Every existing `data-testid` must be preserved. The Playwright suite (26 specs) is the contract. Any task that would force a testid rename must instead keep the old testid on whatever element still semantically owns it.

---

## Task 1: Install dependencies + Tailwind scaffold

**Files:**
- Modify: `web/package.json`
- Create: `web/tailwind.config.ts`
- Create: `web/postcss.config.js`
- Modify: `web/src/main.tsx`
- Create: `web/src/styles/tokens.css`
- Create: `web/src/styles/global.css`

- [ ] **Step 1: Install deps**

```bash
cd web && pnpm add tailwindcss@^3.4 postcss@^8.4 autoprefixer@^10.4 lucide-react@^0.460 @fontsource-variable/inter@^5 @tiptap/extension-bubble-menu@^2.27 -E
```

- [ ] **Step 2: Create `web/postcss.config.js`**

```js
export default {
  plugins: { tailwindcss: {}, autoprefixer: {} },
};
```

- [ ] **Step 3: Create `web/tailwind.config.ts`**

```ts
import type { Config } from "tailwindcss";

export default {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  darkMode: ["class", '[data-theme="dark"]'],
  theme: {
    extend: {
      colors: {
        bg: "var(--color-bg)",
        surface: "var(--color-surface)",
        border: "var(--color-border)",
        muted: "var(--color-muted)",
        fg: "var(--color-fg)",
        "fg-muted": "var(--color-fg-muted)",
        accent: "var(--color-accent)",
        "accent-fg": "var(--color-accent-fg)",
        destructive: "var(--color-destructive)",
      },
      fontFamily: {
        sans: ["'Inter Variable'", "Inter", "system-ui", "sans-serif"],
        mono: ["'JetBrains Mono'", "ui-monospace", "monospace"],
      },
      borderRadius: { sm: "4px", DEFAULT: "6px", md: "8px", lg: "12px" },
      transitionTimingFunction: { swift: "cubic-bezier(0.16, 1, 0.3, 1)" },
    },
  },
} satisfies Config;
```

- [ ] **Step 4: Create `web/src/styles/tokens.css`**

```css
:root, [data-theme="light"] {
  --color-bg: #F8FAFC;
  --color-surface: #FFFFFF;
  --color-border: #E2E8F0;
  --color-muted: #EAEFF3;
  --color-fg: #1E293B;
  --color-fg-muted: #64748B;
  --color-accent: #2563EB;
  --color-accent-fg: #FFFFFF;
  --color-destructive: #DC2626;
  color-scheme: light;
}
[data-theme="dark"] {
  --color-bg: #0B1220;
  --color-surface: #0F172A;
  --color-border: #1E293B;
  --color-muted: #1E293B;
  --color-fg: #F1F5F9;
  --color-fg-muted: #94A3B8;
  --color-accent: #3B82F6;
  --color-accent-fg: #FFFFFF;
  --color-destructive: #F87171;
  color-scheme: dark;
}
```

- [ ] **Step 5: Create `web/src/styles/global.css`**

```css
@import "@fontsource-variable/inter/standard.css";
@tailwind base;
@tailwind components;
@tailwind utilities;
@import "./tokens.css";

html, body, #root { height: 100%; }
body {
  background: var(--color-bg);
  color: var(--color-fg);
  font-family: 'Inter Variable', Inter, system-ui, sans-serif;
  -webkit-font-smoothing: antialiased;
  text-rendering: optimizeLegibility;
}
* { box-sizing: border-box; }
@media (prefers-reduced-motion: reduce) {
  * { animation-duration: 0.01ms !important; transition-duration: 0.01ms !important; }
}
```

- [ ] **Step 6: Update `web/src/main.tsx` — import global.css**

Add at the top, above other imports:

```ts
import "./styles/global.css";
```

- [ ] **Step 7: Verify build + tests still pass**

Run: `cd web && pnpm tsc && pnpm lint && pnpm test`
Expected: all green (no UI changes yet).

- [ ] **Step 8: Commit**

```bash
git add web/
git commit -m "chore(web): tailwind + design tokens + Inter scaffold (Plan 22 T1)"
```

---

## Task 2: Theme store + html data-theme wiring

**Files:**
- Modify: `web/src/stores/ui.ts` (add `theme` + `setTheme` + `toggleTheme`)
- Modify: `web/src/main.tsx` (apply theme to documentElement on boot)

- [ ] **Step 1: Extend the UI store**

In `web/src/stores/ui.ts`, add to the state shape and persist-eligible fields:

```ts
type Theme = "light" | "dark";

// inside the store creator
theme: (localStorage.getItem("knot.theme") as Theme | null) ?? "light",
setTheme: (t: Theme) => {
  localStorage.setItem("knot.theme", t);
  document.documentElement.setAttribute("data-theme", t);
  set({ theme: t });
},
toggleTheme: () => {
  const next: Theme = get().theme === "light" ? "dark" : "light";
  localStorage.setItem("knot.theme", next);
  document.documentElement.setAttribute("data-theme", next);
  set({ theme: next });
},
```

(If `get` isn't already destructured from `set`, change the store factory signature to `(set, get) => ({ ... })`.)

- [ ] **Step 2: Apply theme on boot in `main.tsx`**

Before `ReactDOM.createRoot(...)`:

```ts
const initialTheme = (localStorage.getItem("knot.theme") as "light" | "dark" | null) ?? "light";
document.documentElement.setAttribute("data-theme", initialTheme);
```

- [ ] **Step 3: Add a theme unit test**

Create `web/src/stores/ui.theme.test.ts`:

```ts
import { describe, it, expect, beforeEach } from "vitest";
import { useUi } from "./ui";

describe("ui theme", () => {
  beforeEach(() => { localStorage.clear(); document.documentElement.removeAttribute("data-theme"); useUi.getState().setTheme("light"); });
  it("toggles between light and dark", () => {
    expect(useUi.getState().theme).toBe("light");
    useUi.getState().toggleTheme();
    expect(useUi.getState().theme).toBe("dark");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");
  });
  it("persists to localStorage", () => {
    useUi.getState().setTheme("dark");
    expect(localStorage.getItem("knot.theme")).toBe("dark");
  });
});
```

- [ ] **Step 4: Run tests + commit**

```bash
cd web && pnpm test && pnpm tsc
git add web/src/stores/ui.ts web/src/main.tsx web/src/stores/ui.theme.test.ts
git commit -m "feat(web): theme store + data-theme bootstrap (Plan 22 T2)"
```

---

## Task 3: Primitive components — Button + IconButton + Tooltip

**Files:**
- Create: `web/src/components/ui/Button.tsx`
- Create: `web/src/components/ui/IconButton.tsx`
- Create: `web/src/components/ui/Tooltip.tsx`

- [ ] **Step 1: `Button.tsx`**

```tsx
import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

type Variant = "primary" | "secondary" | "ghost" | "destructive";
type Size = "sm" | "md";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  variant?: Variant;
  size?: Size;
  children: ReactNode;
};

const variants: Record<Variant, string> = {
  primary: "bg-accent text-accent-fg hover:opacity-90",
  secondary: "bg-surface text-fg border border-border hover:bg-muted",
  ghost: "bg-transparent text-fg hover:bg-muted",
  destructive: "bg-destructive text-white hover:opacity-90",
};
const sizes: Record<Size, string> = {
  sm: "h-7 px-2 text-[13px]",
  md: "h-9 px-3 text-sm",
};

export const Button = forwardRef<HTMLButtonElement, Props>(function Button(
  { variant = "secondary", size = "md", className = "", children, ...rest }, ref,
) {
  return (
    <button
      ref={ref}
      className={`inline-flex items-center gap-1.5 rounded font-medium transition-colors ease-swift duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-1 focus-visible:ring-offset-bg disabled:opacity-50 disabled:cursor-not-allowed ${variants[variant]} ${sizes[size]} ${className}`}
      {...rest}
    >
      {children}
    </button>
  );
});
```

- [ ] **Step 2: `IconButton.tsx`**

```tsx
import { forwardRef, type ButtonHTMLAttributes, type ReactNode } from "react";

type Props = ButtonHTMLAttributes<HTMLButtonElement> & {
  label: string;
  active?: boolean;
  size?: "sm" | "md";
  children: ReactNode;
};

export const IconButton = forwardRef<HTMLButtonElement, Props>(function IconButton(
  { label, active, size = "md", className = "", children, ...rest }, ref,
) {
  const sz = size === "sm" ? "h-7 w-7" : "h-9 w-9";
  return (
    <button
      ref={ref}
      type="button"
      aria-label={label}
      aria-pressed={active}
      title={label}
      className={`inline-flex items-center justify-center rounded ${sz} text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${active ? "bg-muted text-fg" : ""} disabled:opacity-40 disabled:cursor-not-allowed ${className}`}
      {...rest}
    >
      {children}
    </button>
  );
});
```

- [ ] **Step 3: `Tooltip.tsx` — minimal title-based wrapper**

(For v0.1 we lean on native `title=` via IconButton; create the file but export a no-op component so future tasks can swap implementations.)

```tsx
import type { ReactNode } from "react";
export function Tooltip({ children }: { label: string; children: ReactNode }) {
  return <>{children}</>;
}
```

- [ ] **Step 4: Commit**

```bash
git add web/src/components/ui/
git commit -m "feat(web): Button + IconButton primitives (Plan 22 T3)"
```

---

## Task 4: AppShell — Outline-style sidebar shell

**Files:**
- Modify: `web/src/components/AppShell.tsx`

**Preserve testIds:** `menu-toggle`, `sidebar-backdrop`, `sidebar`.

- [ ] **Step 1: Rewrite AppShell with Tailwind + Menu icon**

```tsx
import { Menu } from "lucide-react";
import { Outlet } from "react-router-dom";

import { DocTree } from "../features/docs/DocTree";
import { useViewport } from "../hooks/useViewport";
import { useUi } from "../stores/ui";

import { CommandPalette } from "./CommandPalette";
import { Toast } from "./Toast";

export function AppShell() {
  const sidebarOpen = useUi((s) => s.sidebarOpen);
  const toggleSidebar = useUi((s) => s.toggleSidebar);
  const vp = useViewport();
  const mobile = vp === "mobile";

  return (
    <div
      className={`h-dvh font-sans text-fg ${mobile ? "block" : "grid"}`}
      style={!mobile ? { gridTemplateColumns: sidebarOpen ? "260px 1fr" : "0 1fr" } : undefined}
    >
      {mobile && !sidebarOpen && (
        <button
          type="button"
          data-testid="menu-toggle"
          onClick={toggleSidebar}
          aria-label="Open menu"
          className="fixed top-3 left-3 z-30 h-9 w-9 rounded border border-border bg-surface text-fg shadow-sm hover:bg-muted transition-colors ease-swift duration-150 flex items-center justify-center"
        >
          <Menu size={18} aria-hidden />
        </button>
      )}
      {mobile && sidebarOpen && (
        <div
          data-testid="sidebar-backdrop"
          onClick={toggleSidebar}
          className="fixed inset-0 bg-black/40 z-20 backdrop-blur-sm"
        />
      )}
      <aside
        data-testid="sidebar"
        className={`bg-bg border-r border-border overflow-y-auto ${
          mobile
            ? `fixed top-0 h-dvh w-[260px] z-30 transition-[left] duration-200 ease-swift ${sidebarOpen ? "left-0" : "-left-[280px]"}`
            : "static"
        }`}
      >
        <DocTree />
      </aside>
      <main className={`overflow-y-auto bg-bg ${mobile ? "h-dvh" : ""}`}>
        <Outlet />
      </main>
      <Toast />
      <CommandPalette />
    </div>
  );
}
```

- [ ] **Step 2: Run e2e to verify nothing broke**

Run: `cd e2e && pnpm playwright test --reporter=line`
Expected: 26/26 passing.

- [ ] **Step 3: Commit**

```bash
git add web/src/components/AppShell.tsx
git commit -m "feat(web): AppShell — token-based shell, lucide menu icon (Plan 22 T4)"
```

---

## Task 5: Sidebar header (workspace block) + search input

**Files:**
- Create: `web/src/features/workspace/WorkspaceHeader.tsx`
- Modify: `web/src/features/docs/DocTree.tsx` (compose `WorkspaceHeader` at the top)

- [ ] **Step 1: Create `WorkspaceHeader.tsx`**

```tsx
import { Search, Settings, Users } from "lucide-react";
import { Link } from "react-router-dom";

import { useSession } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

export function WorkspaceHeader() {
  const session = useSession();
  const user = session.data && "ok" in session.data ? session.data.ok : null;
  const openPalette = useUi((s) => s.openCommandPalette);

  return (
    <div className="px-3 pt-3 pb-2 border-b border-border">
      <div className="flex items-center gap-2 mb-3">
        <div
          aria-hidden
          className="h-7 w-7 rounded bg-accent text-accent-fg flex items-center justify-center text-[13px] font-semibold"
        >
          {(user?.display_name ?? "?").slice(0, 1).toUpperCase()}
        </div>
        <div className="min-w-0 flex-1">
          <div className="text-[13px] font-semibold text-fg truncate">{user?.display_name ?? "Workspace"}</div>
          <div className="text-[11px] text-fg-muted truncate">{user?.email ?? ""}</div>
        </div>
      </div>
      <button
        type="button"
        data-testid="sidebar-search"
        onClick={openPalette}
        className="w-full flex items-center gap-2 h-8 px-2 rounded bg-muted text-fg-muted hover:text-fg transition-colors ease-swift duration-150"
      >
        <Search size={14} aria-hidden />
        <span className="text-[13px]">Search…</span>
        <span className="ml-auto text-[11px] text-fg-muted/80">⌘K</span>
      </button>
      <nav className="mt-2 flex items-center gap-1">
        <Link
          to="/members"
          className="flex-1 inline-flex items-center gap-1.5 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors"
        >
          <Users size={14} aria-hidden /> Members
        </Link>
        <Link
          to="/settings"
          className="flex-1 inline-flex items-center gap-1.5 h-7 px-2 rounded text-[13px] text-fg-muted hover:text-fg hover:bg-muted transition-colors"
        >
          <Settings size={14} aria-hidden /> Settings
        </Link>
      </nav>
    </div>
  );
}
```

- [ ] **Step 2: Verify `openCommandPalette` exists on the store**

Run: `cd web && grep -n "openCommandPalette\|setPaletteOpen" src/stores/ui.ts`
If it doesn't exist, add it next to the other UI actions: `openCommandPalette: () => set({ paletteOpen: true })`.

- [ ] **Step 3: Commit**

```bash
git add web/src/features/workspace/WorkspaceHeader.tsx web/src/stores/ui.ts
git commit -m "feat(web): WorkspaceHeader with avatar + search trigger (Plan 22 T5)"
```

---

## Task 6: DocTree redesign — Lucide icons + hover-reveal actions

**Files:**
- Modify: `web/src/features/docs/DocTree.tsx`

**Preserve testIds:** `doc-tree`, `new-doc`, `doc-row-${id}`, `ctx-rename`, `ctx-delete`.

- [ ] **Step 1: Replace inline styles with Tailwind + Lucide icons**

Rewrite the `DocTree` and `TreeRow` exports. Replace the `📄 + title` row with `FileText` icon and titled-row pill that highlights on `isActive` with `bg-muted` (not lavender). Add `Plus` icon button next to `Docs` header with `data-testid="new-doc"`. Add hover-reveal row actions (`MoreHorizontal` + `Plus`) that appear via `group-hover` on the row.

```tsx
import { ChevronRight, FileText, MoreHorizontal, Plus } from "lucide-react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useMemo, useState } from "react";
import {
  DndContext, PointerSensor, KeyboardSensor, useSensor, useSensors, closestCenter, type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext, useSortable, verticalListSortingStrategy, sortableKeyboardCoordinates,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useUi } from "../../stores/ui";
import { ContextMenu, type ContextMenuItem } from "../../components/ContextMenu";
import { IconButton } from "../../components/ui/IconButton";
import { WorkspaceHeader } from "../workspace/WorkspaceHeader";
import { type Doc } from "../../lib/validators";
import { type ApiError } from "../../lib/api";

import { docsApi } from "./docs.api";
import { buildTree, reorderInto, type TreeNode } from "./tree";

export function DocTree() {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const { id: activeId } = useParams();
  const { workspace } = useEffectiveRole();
  const canEdit = workspace === "owner" || workspace === "editor";

  const list = useQuery({ queryKey: ["docs"], queryFn: () => docsApi.list() });

  const create = useMutation({
    mutationFn: async (parent_id?: string) => docsApi.create({ title: "Untitled", parent_id }),
    onSuccess: async (r) => {
      if ("error" in r) { notify("error", "Couldn't create document"); return; }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      await nav(`/doc/${(r.ok as { id: string }).id}`);
    },
  });

  const move = useMutation({
    mutationFn: async (a: { id: string; body: { parent_id?: string | null; before_id?: string; after_id?: string } }) => docsApi.move(a.id, a.body),
    onMutate: async (a) => {
      await qc.cancelQueries({ queryKey: ["docs"] });
      const prev = qc.getQueryData<{ ok: Doc[] } | { error: ApiError }>(["docs"]);
      if (prev && "ok" in prev) qc.setQueryData(["docs"], { ok: reorderInto(prev.ok, a.id, a.body.parent_id ?? null) });
      return { prev };
    },
    onError: (_e, _a, ctx) => { if (ctx?.prev) qc.setQueryData(["docs"], ctx.prev); notify("error", "Couldn't move"); },
    onSettled: () => { void qc.invalidateQueries({ queryKey: ["docs"] }); },
  });

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const flatIds = useMemo(
    () => (list.data && "ok" in list.data ? list.data.ok.map((d) => d.id) : []),
    [list.data],
  );

  function onDragEnd(e: DragEndEvent) {
    const movedId = String(e.active.id);
    if (!e.over) return;
    const targetId = String(e.over.id);
    if (movedId === targetId) return;
    move.mutate({ id: movedId, body: { parent_id: targetId } });
  }

  return (
    <div data-testid="doc-tree" className="flex flex-col h-full">
      <WorkspaceHeader />
      <div className="px-3 pt-3 pb-1 flex items-center justify-between">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-fg-muted">Documents</span>
        {canEdit && (
          <IconButton
            data-testid="new-doc"
            label="New document"
            size="sm"
            onClick={() => create.mutate(undefined)}
          >
            <Plus size={14} aria-hidden />
          </IconButton>
        )}
      </div>
      {list.isLoading && <div className="px-3 py-2 text-sm text-fg-muted">Loading…</div>}
      {list.data && "ok" in list.data && list.data.ok.length === 0 && (
        <p className="px-3 py-2 text-sm text-fg-muted">No documents yet.</p>
      )}
      {list.data && "ok" in list.data && (
        <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
          <SortableContext items={flatIds} strategy={verticalListSortingStrategy}>
            <ul className="px-2 pb-3 list-none m-0 flex-1">
              {buildTree(list.data.ok).map((n) => (
                <TreeRow key={n.id} node={n} depth={0} activeId={activeId} canEdit={canEdit} onNewChild={(pid) => create.mutate(pid)} />
              ))}
            </ul>
          </SortableContext>
        </DndContext>
      )}
    </div>
  );
}

function TreeRow({
  node, depth, activeId, canEdit, onNewChild,
}: {
  node: TreeNode; depth: number; activeId?: string; canEdit: boolean; onNewChild: (parentId: string) => void;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const isActive = activeId === node.id;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [expanded, setExpanded] = useState(true);

  const { attributes, listeners, setNodeRef, transform, isDragging } = useSortable({ id: node.id, disabled: !canEdit });
  const sortableStyle = { transform: CSS.Transform.toString(transform), opacity: isDragging ? 0.5 : 1 };

  async function onRename() {
    const next = window.prompt("Rename to:", node.title);
    if (!next || next === node.title) return;
    const r = await docsApi.patch(node.id, { title: next });
    if ("error" in r) notify("error", "Rename failed");
    else await qc.invalidateQueries({ queryKey: ["docs"] });
  }
  async function onArchive() {
    if (!window.confirm(`Delete "${node.title}"?`)) return;
    const r = await docsApi.archive(node.id);
    if ("error" in r) notify("error", "Delete failed");
    else await qc.invalidateQueries({ queryKey: ["docs"] });
  }

  const items: ContextMenuItem[] = canEdit
    ? [
        { label: "Rename", testId: "ctx-rename", onSelect: () => void onRename() },
        { label: "Delete", testId: "ctx-delete", destructive: true, onSelect: () => void onArchive() },
      ]
    : [];

  return (
    <li ref={setNodeRef} style={sortableStyle} {...attributes} {...listeners}>
      <div
        className={`group flex items-center gap-1 rounded h-7 pr-1 transition-colors ease-swift duration-150 ${
          isActive ? "bg-muted text-fg" : "text-fg-muted hover:text-fg hover:bg-muted/60"
        }`}
        style={{ paddingLeft: 4 + depth * 12 }}
      >
        {node.children.length > 0 ? (
          <button
            type="button"
            aria-label={expanded ? "Collapse" : "Expand"}
            onClick={(e) => { e.preventDefault(); setExpanded((v) => !v); }}
            className="h-5 w-5 inline-flex items-center justify-center text-fg-muted hover:text-fg rounded"
          >
            <ChevronRight size={12} className={`transition-transform duration-150 ${expanded ? "rotate-90" : ""}`} aria-hidden />
          </button>
        ) : (
          <span className="h-5 w-5" aria-hidden />
        )}
        <FileText size={14} aria-hidden className="text-fg-muted shrink-0" />
        <Link
          data-testid={`doc-row-${node.id}`}
          to={`/doc/${node.id}`}
          onContextMenu={(e) => { e.preventDefault(); if (items.length) setMenu({ x: e.clientX, y: e.clientY }); }}
          className="flex-1 min-w-0 truncate text-[13px] no-underline text-inherit py-1"
        >
          {node.title}
        </Link>
        {canEdit && (
          <div className="opacity-0 group-hover:opacity-100 transition-opacity flex items-center gap-0.5">
            <IconButton label="More" size="sm" onClick={(e) => { e.preventDefault(); setMenu({ x: e.clientX, y: e.clientY }); }}>
              <MoreHorizontal size={14} aria-hidden />
            </IconButton>
            <IconButton label="Add subpage" size="sm" onClick={(e) => { e.preventDefault(); onNewChild(node.id); }}>
              <Plus size={14} aria-hidden />
            </IconButton>
          </div>
        )}
      </div>
      {menu && <ContextMenu x={menu.x} y={menu.y} items={items} onClose={() => setMenu(null)} />}
      {expanded && node.children.length > 0 && (
        <ul className="list-none p-0 m-0">
          {node.children.map((c) => (
            <TreeRow key={c.id} node={c} depth={depth + 1} activeId={activeId} canEdit={canEdit} onNewChild={onNewChild} />
          ))}
        </ul>
      )}
    </li>
  );
}
```

- [ ] **Step 2: Re-run e2e (regression)**

Run: `cd e2e && pnpm playwright test --reporter=line`
Expected: 26/26.

If `mobile.spec.ts` clicks at the old sidebar backdrop coords, no change needed (the testIds still resolve). If anything fails, the row pill height/padding may need a tweak.

- [ ] **Step 3: Commit**

```bash
git add web/src/features/docs/DocTree.tsx
git commit -m "feat(web): DocTree — lucide icons, hover-reveal actions, Outline-style rows (Plan 22 T6)"
```

---

## Task 7: DocPage chrome — breadcrumb + icon action row

**Files:**
- Modify: `web/src/features/docs/DocPage.tsx`
- Create: `web/src/features/docs/Breadcrumb.tsx`

**Preserve testIds:** `doc-page`, `doc-title`, `open-permissions`, `open-history`, `open-comments`, `status-dot`.

- [ ] **Step 1: `Breadcrumb.tsx`**

```tsx
import { ChevronRight } from "lucide-react";
import { Link } from "react-router-dom";

export function Breadcrumb({ items }: { items: Array<{ id?: string; title: string }> }) {
  return (
    <nav aria-label="Breadcrumb" className="text-[12px] text-fg-muted flex items-center flex-wrap">
      {items.map((it, i) => (
        <span key={i} className="inline-flex items-center">
          {i > 0 && <ChevronRight size={12} aria-hidden className="mx-1 opacity-60" />}
          {it.id ? (
            <Link to={`/doc/${it.id}`} className="hover:text-fg transition-colors">{it.title}</Link>
          ) : (
            <span>{it.title}</span>
          )}
        </span>
      ))}
    </nav>
  );
}
```

- [ ] **Step 2: Rewrite DocPage chrome**

Replace `<header>` with a two-row composition: breadcrumb on top, title + action icons below. Action icons use `Share2`, `History`, `MessageSquare`. Editor host gains `mx-auto max-w-[760px] px-6 py-8`.

```tsx
import { History, MessageSquare, Share2 } from "lucide-react";
// ... existing imports

// Replace `<section data-testid="doc-page" style={{ padding: 24 }}>` with:
return (
  <section data-testid="doc-page" className="mx-auto max-w-[760px] px-6 py-8">
    <Breadcrumb items={[{ title: "Documents" }, { title: meta.title }]} />
    <div className="mt-3 flex items-start gap-3">
      <div className="flex-1 min-w-0">
        <DocTitle key={id} id={id} initialTitle={meta.title} />
      </div>
      <div className="flex items-center gap-1 pt-1">
        <StatusDot status={status} />
        {effRole === "owner" && (
          <Link to="permissions" data-testid="open-permissions" aria-label="Permissions" title="Permissions"
            className="inline-flex items-center justify-center h-9 w-9 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors">
            <Share2 size={16} aria-hidden />
          </Link>
        )}
        {(effRole === "owner" || effRole === "editor") && (
          <IconButton data-testid="open-history" label="History" onClick={() => setHistoryOpen(true)}>
            <History size={16} aria-hidden />
          </IconButton>
        )}
        <IconButton data-testid="open-comments" label="Comments" onClick={openCommentSidebar}>
          <MessageSquare size={16} aria-hidden />
        </IconButton>
      </div>
    </div>
    <Suspense fallback={<p className="text-fg-muted mt-6">Loading editor…</p>}>
      <div className="mt-6">
        <KnotEditor docId={id} onStatus={setStatus} role={meta.effective_role} />
      </div>
    </Suspense>
    <Outlet />
    {historyOpen && id && <HistoryDrawer docId={id} onClose={() => setHistoryOpen(false)} />}
    {commentSidebarOpen && id && <CommentSidebar docId={id} />}
  </section>
);
```

Update `DocTitle` to use Tailwind:

```tsx
<input
  data-testid="doc-title"
  value={title}
  onChange={(e) => setTitle(e.target.value)}
  onBlur={() => { if (title !== initialTitle) rename.mutate(title); }}
  placeholder="Untitled"
  className="w-full border-none bg-transparent text-[30px] font-bold text-fg placeholder:text-fg-muted/60 focus:outline-none focus:ring-0"
/>
```

- [ ] **Step 3: Re-run e2e**

Run: `cd e2e && pnpm playwright test --reporter=line`
Expected: 26/26.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/docs/DocPage.tsx web/src/features/docs/Breadcrumb.tsx
git commit -m "feat(web): DocPage — breadcrumb, max-w reading column, icon action row (Plan 22 T7)"
```

---

## Task 8: EditorToolbar — Lucide icons + bubble menu on selection

**Files:**
- Modify: `web/src/features/editor/EditorToolbar.tsx`
- Modify: `web/src/features/editor/KnotEditor.tsx` (mount toolbar as BubbleMenu)

**Preserve testIds:** `editor-toolbar`, `toolbar-bold`, `toolbar-italic`, `toolbar-strike`, `toolbar-code`, `toolbar-h1`, `toolbar-h2`, `toolbar-h3`, `toolbar-bullet-list`, `toolbar-ordered-list`, `toolbar-blockquote`, `toolbar-code-block`, `toolbar-link`, `link-popover`, `link-input`, `link-apply`, `link-remove`.

- [ ] **Step 1: Decision — keep the toolbar permanent for now**

A bubble menu would change the e2e contract (specs click `toolbar-bold` directly without first selecting text). Defer the BubbleMenu migration to a follow-up; in this task, keep the toolbar permanent but **swap visuals** to icons + tokens.

- [ ] **Step 2: Rewrite EditorToolbar with Lucide icons**

```tsx
import { useState, type ReactNode } from "react";
import type { Editor } from "@tiptap/react";
import {
  Bold, Italic, Strikethrough, Code, Heading1, Heading2, Heading3,
  List, ListOrdered, Quote, Code2, Link as LinkIcon,
} from "lucide-react";

function Btn({ testId, label, active, onClick, children }:
  { testId: string; label: string; active?: boolean; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      data-testid={testId}
      aria-label={label}
      aria-pressed={active}
      title={label}
      onClick={onClick}
      className={`inline-flex items-center justify-center h-8 min-w-8 px-2 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150 ${active ? "bg-muted text-fg" : ""}`}
    >
      {children}
    </button>
  );
}
function Sep() { return <span className="mx-1 h-5 w-px bg-border" aria-hidden />; }

export function EditorToolbar({ editor }: { editor: Editor | null }) {
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkUrl, setLinkUrl] = useState("");
  if (!editor) return null;
  const c = () => editor.chain().focus();
  return (
    <div data-testid="editor-toolbar"
      className="sticky top-0 z-10 flex items-center gap-0.5 flex-wrap py-2 mb-4 bg-bg/80 backdrop-blur border-b border-border relative">
      <Btn testId="toolbar-bold" label="Bold" active={editor.isActive("bold")} onClick={() => c().toggleBold().run()}><Bold size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-italic" label="Italic" active={editor.isActive("italic")} onClick={() => c().toggleItalic().run()}><Italic size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-strike" label="Strikethrough" active={editor.isActive("strike")} onClick={() => c().toggleStrike().run()}><Strikethrough size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-code" label="Inline code" active={editor.isActive("code")} onClick={() => c().toggleCode().run()}><Code size={15} aria-hidden /></Btn>
      <Sep />
      <Btn testId="toolbar-h1" label="Heading 1" active={editor.isActive("heading", { level: 1 })} onClick={() => c().toggleHeading({ level: 1 }).run()}><Heading1 size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-h2" label="Heading 2" active={editor.isActive("heading", { level: 2 })} onClick={() => c().toggleHeading({ level: 2 }).run()}><Heading2 size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-h3" label="Heading 3" active={editor.isActive("heading", { level: 3 })} onClick={() => c().toggleHeading({ level: 3 }).run()}><Heading3 size={15} aria-hidden /></Btn>
      <Sep />
      <Btn testId="toolbar-bullet-list" label="Bullet list" active={editor.isActive("bulletList")} onClick={() => c().toggleBulletList().run()}><List size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-ordered-list" label="Ordered list" active={editor.isActive("orderedList")} onClick={() => c().toggleOrderedList().run()}><ListOrdered size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-blockquote" label="Quote" active={editor.isActive("blockquote")} onClick={() => c().toggleBlockquote().run()}><Quote size={15} aria-hidden /></Btn>
      <Btn testId="toolbar-code-block" label="Code block" active={editor.isActive("codeBlock")} onClick={() => c().toggleCodeBlock().run()}><Code2 size={15} aria-hidden /></Btn>
      <Sep />
      <Btn testId="toolbar-link" label="Link" active={editor.isActive("link")}
        onClick={() => { setLinkUrl((editor.getAttributes("link").href as string) ?? ""); setLinkOpen(true); }}>
        <LinkIcon size={15} aria-hidden />
      </Btn>
      {linkOpen && (
        <div data-testid="link-popover"
          className="absolute top-full left-0 mt-1 flex items-center gap-1 p-2 rounded-md bg-surface border border-border shadow-lg z-20"
          onKeyDown={(e) => { if (e.key === "Escape") setLinkOpen(false); }}>
          <input data-testid="link-input" type="url" value={linkUrl}
            onChange={(e) => setLinkUrl(e.target.value)} placeholder="https://" autoFocus
            className="h-8 px-2 min-w-[240px] rounded border border-border bg-bg text-fg text-sm focus:outline-none focus:ring-2 focus:ring-accent" />
          <button data-testid="link-apply" type="button"
            onClick={() => { if (linkUrl) c().extendMarkRange("link").setLink({ href: linkUrl }).run(); else c().unsetLink().run(); setLinkOpen(false); }}
            className="h-8 px-2.5 rounded bg-accent text-accent-fg text-[13px] font-medium hover:opacity-90">Apply</button>
          <button data-testid="link-remove" type="button"
            onClick={() => { c().unsetLink().run(); setLinkOpen(false); }}
            className="h-8 px-2.5 rounded text-fg-muted hover:text-fg hover:bg-muted text-[13px]">Remove</button>
        </div>
      )}
    </div>
  );
}
```

- [ ] **Step 3: Run e2e**

Run: `cd e2e && pnpm playwright test editor-toolbar.spec.ts command-palette.spec.ts --reporter=line`
Expected: pass. If a hit-area is too small (touch target rule says ≥32px), bump `h-8` to `h-9` or `min-w-9`.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/editor/EditorToolbar.tsx
git commit -m "feat(web): EditorToolbar — lucide icons + tokenized chrome (Plan 22 T8)"
```

---

## Task 9: Editor host + ProseMirror typography

**Files:**
- Modify: `web/src/features/editor/KnotEditor.tsx` (host className)
- Create: `web/src/styles/prose.css`
- Modify: `web/src/styles/global.css` (`@import "./prose.css"`)

- [ ] **Step 1: Create `prose.css`**

```css
.ProseMirror {
  outline: none;
  color: var(--color-fg);
  font-size: 16px;
  line-height: 1.7;
  letter-spacing: -0.003em;
}
.ProseMirror p { margin: 0.6em 0; }
.ProseMirror h1 { font-size: 28px; font-weight: 700; margin: 1.4em 0 0.4em; letter-spacing: -0.02em; }
.ProseMirror h2 { font-size: 22px; font-weight: 700; margin: 1.2em 0 0.4em; letter-spacing: -0.015em; }
.ProseMirror h3 { font-size: 18px; font-weight: 600; margin: 1em 0 0.3em; }
.ProseMirror blockquote {
  border-left: 3px solid var(--color-border);
  padding: 0.1em 0 0.1em 1em;
  color: var(--color-fg-muted);
  margin: 0.8em 0;
}
.ProseMirror code {
  background: var(--color-muted);
  border-radius: 4px;
  padding: 1px 4px;
  font-family: 'JetBrains Mono', ui-monospace, monospace;
  font-size: 0.92em;
}
.ProseMirror pre {
  background: var(--color-muted);
  border-radius: 8px;
  padding: 12px 14px;
  overflow-x: auto;
}
.ProseMirror pre code { background: none; padding: 0; }
.ProseMirror ul, .ProseMirror ol { padding-left: 1.4em; margin: 0.6em 0; }
.ProseMirror a { color: var(--color-accent); text-underline-offset: 2px; }
.ProseMirror img { max-width: 100%; border-radius: 6px; }
```

- [ ] **Step 2: Editor host wrapper**

In `KnotEditor.tsx`, change the `<div data-testid="editor-host" ...>` to:

```tsx
<div data-testid="editor-host" className="relative">
  {/* Floating Add comment button + EditorContent unchanged */}
</div>
```

Remove the inline `border / padding / minHeight / position` styles — they conflict with the new reading-column layout in DocPage.

- [ ] **Step 3: Run e2e + verify visual**

Run: `cd e2e && pnpm playwright test --reporter=line`
Expected: 26/26. Manually open dev (`make dev`), confirm the doc reads cleanly with Inter and the toolbar sticks to the top.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/editor/KnotEditor.tsx web/src/styles/prose.css web/src/styles/global.css
git commit -m "feat(web): editor host typography + prose CSS (Plan 22 T9)"
```

---

## Task 10: StatusDot + Toast + ContextMenu + CommandPalette themed

**Files:**
- Modify: `web/src/components/StatusDot.tsx`
- Modify: `web/src/components/Toast.tsx`
- Modify: `web/src/components/ContextMenu.tsx`
- Modify: `web/src/components/CommandPalette.tsx`

**Preserve testIds:** `status-dot`, `toast`, `palette`, `palette-input`, `palette-results`, `ctx-rename`, `ctx-delete`.

- [ ] **Step 1: StatusDot — small dot pill**

Use semantic colors with hardcoded fallback (status colors are intentionally outside the neutral palette):

```tsx
const colors: Record<string, string> = {
  connected: "bg-emerald-500",
  connecting: "bg-amber-500",
  offline: "bg-fg-muted",
  unauthorised: "bg-destructive",
  conflict: "bg-destructive",
};
return (
  <span
    data-testid="status-dot"
    data-status={status}
    aria-label={`Connection ${status}`}
    title={status}
    className={`inline-block h-2 w-2 rounded-full ${colors[status] ?? "bg-fg-muted"} mr-2`}
  />
);
```

- [ ] **Step 2: Toast — tokenized + slide-in**

```tsx
<div data-testid="toast"
  role="status"
  aria-live="polite"
  className={`fixed bottom-4 right-4 z-50 max-w-sm rounded-md border border-border bg-surface shadow-lg px-4 py-3 text-sm text-fg animate-[slideIn_200ms_ease-out] ${
    kind === "error" ? "border-l-4 border-l-destructive" : "border-l-4 border-l-accent"
  }`}
>{message}</div>
```

Add `@keyframes slideIn { from { transform: translateY(8px); opacity: 0 } to { transform: none; opacity: 1 } }` to `global.css`.

- [ ] **Step 3: ContextMenu — tokenized surface**

Replace inline styles with `bg-surface border border-border rounded-md shadow-lg py-1` and items as `text-sm px-3 py-1.5 hover:bg-muted` (destructive: `text-destructive hover:bg-destructive/10`).

- [ ] **Step 4: CommandPalette — themed shell**

Replace the white box + raw input with:
```tsx
<div data-testid="palette" className="fixed inset-0 z-50 flex items-start justify-center pt-[15vh] bg-black/30 backdrop-blur-sm">
  <div className="w-full max-w-xl bg-surface border border-border rounded-lg shadow-2xl overflow-hidden">
    <input data-testid="palette-input" ... className="w-full px-4 py-3 bg-transparent text-fg placeholder:text-fg-muted border-b border-border focus:outline-none" />
    <ul data-testid="palette-results" className="max-h-80 overflow-y-auto py-1">{/* items */}</ul>
  </div>
</div>
```

- [ ] **Step 5: Run e2e + commit**

```bash
cd e2e && pnpm playwright test --reporter=line
git add web/src/components/
git commit -m "feat(web): StatusDot/Toast/ContextMenu/CommandPalette tokenized (Plan 22 T10)"
```

---

## Task 11: CommentSidebar + HistoryDrawer + Comment components themed

**Files:**
- Modify: `web/src/features/comments/CommentSidebar.tsx`
- Modify: `web/src/features/comments/CommentThread.tsx`
- Modify: `web/src/features/comments/CommentComposer.tsx`
- Modify: `web/src/features/comments/MentionPicker.tsx`
- Modify: `web/src/features/docs/HistoryDrawer.tsx`

**Preserve testIds:** all `comment-*` and history drawer testIds.

- [ ] **Step 1: Drawers**

Both drawers become `fixed right-0 top-0 h-dvh w-[380px] bg-surface border-l border-border shadow-xl flex flex-col`. Header rows use `border-b border-border px-4 py-3 flex items-center justify-between`. Close button is `IconButton` with `X` icon.

- [ ] **Step 2: Replace emoji reactions**

For the six fixed reactions, keep emoji glyphs (they're literally the emoji content, not icons-for-actions). But the **reaction toggle button** itself should be an `IconButton` with `SmilePlus` from Lucide. Keep `comment-react-add-${id}` testid.

- [ ] **Step 3: Comment composer textarea**

```tsx
<textarea
  data-testid={`comment-composer-input-${kind}${threadId ? `-${threadId}` : ""}`}
  className="w-full min-h-[72px] rounded border border-border bg-bg p-2 text-sm text-fg placeholder:text-fg-muted focus:outline-none focus:ring-2 focus:ring-accent"
/>
```

- [ ] **Step 4: Run comments + history e2e**

```bash
cd e2e && pnpm playwright test comments.spec.ts history.spec.ts --reporter=line
```

- [ ] **Step 5: Commit**

```bash
git add web/src/features/comments/ web/src/features/docs/HistoryDrawer.tsx
git commit -m "feat(web): comments + history drawer themed (Plan 22 T11)"
```

---

## Task 12: Auth pages (Login + Setup) themed

**Files:**
- Modify: `web/src/features/auth/LoginPage.tsx`
- Modify: `web/src/features/auth/SetupPage.tsx`

**Preserve all testIds.**

- [ ] **Step 1: Auth shell layout**

```tsx
<div className="min-h-dvh flex items-center justify-center px-4 bg-bg">
  <div className="w-full max-w-sm bg-surface border border-border rounded-lg shadow-sm p-6">
    <h1 className="text-xl font-semibold text-fg mb-1">Welcome to knot</h1>
    <p className="text-sm text-fg-muted mb-6">Sign in to continue</p>
    {/* fields */}
  </div>
</div>
```

Inputs use `h-9 w-full px-3 rounded border border-border bg-bg text-fg focus:outline-none focus:ring-2 focus:ring-accent`.

- [ ] **Step 2: Run auth e2e + commit**

```bash
cd e2e && pnpm playwright test auth.spec.ts --reporter=line
git add web/src/features/auth/
git commit -m "feat(web): auth pages themed (Plan 22 T12)"
```

---

## Task 13: SettingsPage + MembersPage + PermissionsDialog themed

**Files:**
- Modify: `web/src/features/workspace/SettingsPage.tsx`
- Modify: `web/src/features/workspace/MembersPage.tsx`
- Modify: `web/src/features/permissions/PermissionsDialog.tsx`

**Preserve all testIds.**

- [ ] **Step 1: Page shell**

```tsx
<section className="mx-auto max-w-[760px] px-6 py-8">
  <h1 className="text-2xl font-semibold text-fg mb-4">Settings</h1>
  <div className="bg-surface border border-border rounded-lg divide-y divide-border">
    {/* rows */}
  </div>
</section>
```

Tables get `w-full text-sm`, headers `text-fg-muted font-medium`, cells `py-2`. Role pills: `inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-muted text-fg-muted`.

PermissionsDialog becomes a centered modal: `fixed inset-0 bg-black/40 backdrop-blur-sm flex items-center justify-center p-4` + dialog `bg-surface border border-border rounded-lg shadow-2xl max-w-md w-full p-5`.

- [ ] **Step 2: Run permissions + members e2e + commit**

```bash
cd e2e && pnpm playwright test permissions.spec.ts members.spec.ts settings.spec.ts --reporter=line
git add web/src/features/workspace/ web/src/features/permissions/
git commit -m "feat(web): settings/members/permissions themed (Plan 22 T13)"
```

---

## Task 14: Theme toggle in WorkspaceHeader + reduced-motion verified

**Files:**
- Modify: `web/src/features/workspace/WorkspaceHeader.tsx`

- [ ] **Step 1: Add theme toggle**

```tsx
import { Moon, Sun } from "lucide-react";
import { IconButton } from "../../components/ui/IconButton";

const theme = useUi((s) => s.theme);
const toggleTheme = useUi((s) => s.toggleTheme);

// inside the header, next to Settings:
<IconButton
  data-testid="theme-toggle"
  label={theme === "dark" ? "Light mode" : "Dark mode"}
  size="sm"
  onClick={toggleTheme}
>
  {theme === "dark" ? <Sun size={14} aria-hidden /> : <Moon size={14} aria-hidden />}
</IconButton>
```

- [ ] **Step 2: Add a Playwright spec for dark mode**

Create `e2e/flows/theme.spec.ts`:

```ts
import { expect, test } from "@playwright/test";

test("theme toggle persists and applies", async ({ page }) => {
  await page.goto("/");
  // Setup or login if needed — skip if app already has a session; for a fresh DB:
  // (Reuse the setup boilerplate from auth.spec.ts if no session.)
  const toggle = page.getByTestId("theme-toggle");
  await expect(toggle).toBeVisible();
  await toggle.click();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-theme", "dark");
});
```

- [ ] **Step 3: Run full suite**

```bash
cd e2e && pnpm playwright test --reporter=line
```
Expected: 27/27.

- [ ] **Step 4: Commit**

```bash
git add web/src/features/workspace/WorkspaceHeader.tsx e2e/flows/theme.spec.ts
git commit -m "feat(web): theme toggle + dark mode e2e (Plan 22 T14)"
```

---

## Task 15: Outcome doc + Plan 22 row in README

**Files:**
- Create: `docs/superpowers/research/2026-06-03-plan22-outcome.md`
- Modify: `docs/superpowers/README.md`

- [ ] **Step 1: Outcome doc**

Capture status, gates (`cargo test` unchanged, `pnpm tsc/lint/test/playwright`), what landed, what's non-obvious (sticky toolbar + reading column interaction, status colors outside the neutral palette, why we deferred BubbleMenu), what's deferred (BubbleMenu, doc icon picker, cover image, settings sub-navigation, mobile sidebar polish, dark-mode editor cursor caret review).

- [ ] **Step 2: Add row to README plans table**

```md
| 22 | 2026-06-03 | UI Modernization | [plans/2026-06-03-ui-modernization.md](plans/2026-06-03-ui-modernization.md) | [2026-06-03-plan22-outcome.md](research/2026-06-03-plan22-outcome.md) |
```

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/
git commit -m "docs: Plan 22 outcome — UI modernization (Outline-style)"
```

---

## Carryforward (not in this plan)

- **BubbleMenu toolbar.** Migrate from sticky strip to selection-driven floating menu. Will change e2e contract (specs must `selectText()` before clicking toolbar buttons). ~1 task.
- **Doc icon picker.** Replace fixed `FileText` with a per-doc emoji/lucide-icon picker stored in `docs.icon`. Schema already has the column.
- **Cover image / banner.** Outline-style hero band on each doc.
- **Sidebar polish:** collapse/expand all, recently visited, favorites.
- **Mobile sidebar refinements.** Re-test on 375px and landscape.
- **Editor caret + selection color in dark mode.** ProseMirror selection background needs an explicit token override.
- **`prefers-color-scheme` initial detection** (currently defaults to `light`).
