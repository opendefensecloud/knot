# Resizable Table Columns Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Editors can drag table column borders in any doc, widths are stored as proportions and fill the available width on every screen, and long text wraps inside its cell.

**Architecture:** A pure width model (`columnWidths.ts`) converts the `colwidth` cell attribute between stored integer weights and rendered shares. A table node view (`TableColumnView.ts`), always installed regardless of editability, renders a percentage `<colgroup>` plus a handle layer and commits a drag as one transaction. `KnotTable` swaps prosemirror-tables' pixel resizing for this view.

**Tech Stack:** Tiptap 3 / ProseMirror (`@tiptap/pm/tables` `TableMap`), Yjs via `@tiptap/extension-collaboration`, Vitest + jsdom, Playwright (e2e, not runnable locally).

**Spec:** `docs/superpowers/specs/2026-10-12-table-column-resize-design.md`

## Global Constraints

- `colwidth` stays the only storage; no schema change; stays in `EDITOR_ONLY_ATTRS`.
- Committed weights sum to exactly `WEIGHT_TOTAL = 1000`, every entry ≥ 1.
- Minimum dragged column width: `MIN_COLUMN_PX = 40` rendered pixels.
- One drag = one transaction; no transaction for a press without movement, `Escape`, `pointercancel`, or a non-editable view.
- Web tests run with `NODE_OPTIONS=--no-experimental-webstorage` (Node 26 + jsdom localStorage clash); pnpm via `npx pnpm`.
- Only the user commits and pushes: "Commit" steps below mean *stage*.

## Review Focus

1. A table whose first row has a colspan cell — the column count and per-column weights must come from `TableMap`, not the cell count.
2. A column already narrower than 40px (phone, many columns) — a drag must never make it jump wider or shrink further.
3. Legacy pixel `colwidth` values and partially set arrays (`[200, 0, null]`) — ratios kept, unset columns take the mean.
4. Doc opened read-only then switched to edit — handles must work (the original bug).
5. A remote change arriving mid-drag that adds or removes a column — the drag must be cancelled, not commit a mis-sized weight array.

---

### Task 1: Width model (`columnWidths.ts`)

**Files:**
- Create: `web/src/features/editor/columnWidths.ts`
- Test: `web/src/features/editor/columnWidths.test.ts`

**Interfaces — produces:**
```ts
export const WEIGHT_TOTAL = 1000;
export const MIN_COLUMN_PX = 40;
export function resolveShares(weights: readonly (number | null)[]): number[];
export function moveBorder(shares: readonly number[], border: number, delta: number, minShare: number): number[];
export function toWeights(shares: readonly number[]): number[];
export function sharesEqual(a: readonly number[], b: readonly number[]): boolean;
export function readColumnWeights(table: PMNode): (number | null)[];
export function setColumnWeights(tr: Transaction, tableStart: number, table: PMNode, weights: readonly number[]): Transaction;
export function sharePercent(share: number): string; // "33.333%"
```

- [ ] Write failing tests: `resolveShares` (all null → equal; missing/zero/negative → mean of known; legacy px keep ratio; empty → []); `moveBorder` (moves only the pair, pair sum constant, clamps at `minShare` both directions, a column already below the floor neither jumps wider nor shrinks, out-of-range border is a no-op); `toWeights` (sums to exactly 1000, largest-remainder, tiny share ≥ 1); `readColumnWeights` / `setColumnWeights` through a mounted editor, including a colspan cell in the first row.
- [ ] Run, see them fail (module missing).
- [ ] Implement.
- [ ] Run, see them pass.
- [ ] Stage.

### Task 2: Table node view + extension wiring

**Files:**
- Create: `web/src/features/editor/TableColumnView.ts`
- Modify: `web/src/features/editor/TableExtensions.ts` (`resizable: false`, `addNodeView`, `renderHTML`)
- Modify: `web/src/test/boundEditor.ts` (`editable` option)
- Test: `web/src/features/editor/TableColumnView.test.ts`

**Interfaces — consumes** Task 1. **Produces:**
```ts
export class TableColumnView implements NodeView {
  constructor(node: PMNode, view: EditorView, getPos: () => number | undefined, htmlAttributes: Record<string, unknown>);
}
```
DOM: `div.tableWrapper > table.knot-table > colgroup + tbody`, plus `div.knot-col-handles > div.knot-col-handle[data-border=i]` (n-1).

- [ ] Write failing tests (jsdom; stub the table's `getBoundingClientRect().width = 800`): percentage colgroup from stored weights; `n-1` handles at cumulative `left`; drag border 0 by +80px → exactly one doc-changing transaction, every cell in the column carries `[350]`, `[150]`, `[250]`, `[250]`; colspan header cell gets a two-entry array; drag past the floor clamps to 40px → weight 50; press without move → no transaction; `Escape` mid-drag → no transaction and colgroup restored; non-editable view → no transaction; editor created non-editable then `setEditable(true)` → drag commits (regression); remote column added mid-drag → drag cancelled; `getHTML()` emits percentage `<col>`s.
- [ ] Run, see them fail.
- [ ] Implement view and wiring.
- [ ] Run, see them pass; run the whole editor suite (schema parity must still pass).
- [ ] Stage.

### Task 3: Styles, cleanup, changelog, e2e

**Files:**
- Modify: `web/src/styles/prose.css` (wrap, handle layer, wrapper margin; drop `.column-resize-handle`, `.resize-cursor`)
- Modify: `web/src/features/editor/schema.test.ts` (comment on `colwidth`)
- Modify: `CHANGELOG.md` (Unreleased → Added / Fixed)
- Create: `e2e/flows/table-column-resize.spec.ts`

- [ ] CSS: `th, td { overflow-wrap: anywhere }`; `.tableWrapper { position: relative; margin-block: 0.8em }`, table margin 0 inside it; handle layer `inset: 0; pointer-events: none`; handle 9px, `cursor: col-resize`, `touch-action: none`, accent line on hover / `.is-dragging`; hidden under `.ProseMirror[contenteditable="false"]`.
- [ ] e2e: new doc → table → switch to view → reload (editor now created read-only) → no visible handle → Edit → drag first border +120px → first column wider → reload → still wider.
- [ ] Full verification: `npx pnpm test`, `npx pnpm tsc`, `npx pnpm lint`, `npx pnpm build`.
- [ ] Stage.
