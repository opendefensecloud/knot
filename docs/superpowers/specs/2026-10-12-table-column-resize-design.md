# Resizable table columns — design

**Date:** 2026-10-12
**Status:** Approved (brainstorm)
**Issue:** [#36](https://github.com/opendefensecloud/knot/issues/36)

## Context

The issue asks that editors can resize table columns by dragging the column
lines, that long text wraps, that viewers see the result, that widths are
relations rather than fixed pixels (screens differ), and that the change is
saved with the page.

Column resizing is nominally on already — `KnotTable` is configured with
`resizable: true` (`web/src/features/editor/TableExtensions.ts`) — but in
practice it does not work, and where it does, it produces pixel widths:

1. **The drag handles are missing for almost every doc.** Tiptap's `Table`
   decides in `addProseMirrorPlugins()` whether to install prosemirror-tables'
   `columnResizing` plugin, using `this.editor.isEditable` *at editor creation*.
   `KnotEditor` creates the editor with `editable: canEdit`, where `canEdit`
   folds in edit mode, and toggles later with `setEditable()` without
   re-creating it. Existing docs open in view mode, so the plugin is never
   installed, and switching to Edit does not add it. Verified with a probe: an
   editor created non-editable and then `setEditable(true)` has no
   `tableColumnResizing` plugin; one created editable has it. Only freshly
   created docs (seeded into edit mode by `markDocEditMode`) can drag columns.
2. **Widths are pixels.** prosemirror-tables stores the dragged width of a
   column in the cells' `colwidth` attribute as pixels. Once every column is
   pinned, `TableView` writes an inline `width: <sum>px` on the table, which
   beats the stylesheet's `width: 100%`; the `.tableWrapper { overflow-x: auto }`
   rule (0.4.0) exists only to stop such a table blowing out the page.
3. **Unbreakable text spills.** `table-layout: fixed` wraps ordinary words, but
   a long URL or identifier in a narrow column overflows into its neighbour.

`colwidth` already rides the Yjs document, so viewers already see whatever is
stored and it is already persisted with the page.

## Decisions

- **Proportional widths.** A table always fills the available width (as an
  un-resized table already does); each column keeps its share. Dragging a
  border moves width between the two neighbouring columns only.
- **Markdown round-trip is out of scope.** Export → import and "create from
  template" go through markdown, which has no column-width concept, so tables
  arriving that way start equal-width. Candidate follow-up issue.
- **Own node view, not prosemirror-tables' resizing.** Its live preview writes
  pixel widths straight into the DOM, so correcting it after the fact would
  make the handle drift from the pointer. A pointer-near-cell-edge plugin (its
  approach) would also work but needs hit-testing and colspan edge cases that a
  node-owned handle layer avoids.

## Data model

- The existing `colwidth` cell attribute (array of numbers, one per spanned
  column) is kept. No schema change; `colwidth` stays in
  `EDITOR_ONLY_ATTRS` in `schema.test.ts` because markdown has no use for it.
- Its values are now read as **relative weights**, not pixels.
- Every committed drag rewrites the weights of **all** columns, normalised so
  they sum to **1000**, and writes them to every cell of every row (colspan
  mapped through `TableMap`). Normalising to a fixed total keeps two editors on
  different screen widths from mixing scales. A happy side effect: a stale
  client still reading the values as pixels renders a sensible 1000px table.
- **Resolving shares for rendering** (first row, as prosemirror-tables and
  Tiptap do):
  - a column with a positive weight keeps that weight;
  - columns with no weight (`null`, `0`, missing) share what the set ones
    leave of 1000. When nothing is left — a column added to a normalised
    table — each gets the mean of the known weights; with no weights at all,
    columns are equal;
  - each column then renders at `weight / sum of weights`.
- Legacy pixel values read as weights against the same 1000 total. A fully
  pinned table keeps its ratios exactly. prosemirror-tables only ever wrote
  the dragged column, so the common legacy shape is `[300, null, null, null]`;
  it renders 30/23/23/23%, keeping the dragged column the widest instead of
  dropping it to an equal share. Either way the table now fits the screen
  instead of scrolling.
- prosemirror-tables' own commands keep working on `colwidth` unchanged:
  `addColumn` inserts cells without a width (→ mean share), `deleteColumn`
  removes the entry (→ others re-share; a column added afterwards takes the
  freed share), merge/split carry the arrays, `fixTables` repairs per-column
  mismatches.

## Rendering

- `KnotTable` is configured `resizable: false`, so prosemirror-tables'
  `columnResizing` plugin and its `TableView` are never installed.
- `KnotTable` overrides `addNodeView()` to always return a `TableColumnView`,
  in view and edit mode alike, so a later `setEditable()` cannot lose anything.
  It keeps the current DOM contract: `div.tableWrapper > table.knot-table >
  colgroup + tbody`; the `renderHTML` override always emits the wrapper, so
  `renderWrapper` is no longer configured.
- `<col>` widths are percentages from the resolved shares; the table is
  `width: 100%` with no inline pixel width or min-width.
- `KnotTable` also overrides `renderHTML()` (used for `getHTML()` and the
  clipboard) to emit the same percentage colgroup, so pasting elsewhere keeps
  the proportions.
- `TableColumnView.update()` re-renders the colgroup and handles from the new
  node; `ignoreMutation()` ignores everything outside `contentDOM`.

## Drag interaction

- **Handles.** The node view keeps a handle layer inside the wrapper,
  absolutely positioned over the table: one strip per *inner* column border
  (none on the outer right edge — the table is full width), placed at the
  running total of shares (`left: <cumulative %>`), about 8px to grab, full
  table height, `cursor: col-resize`, showing the accent line on hover (where
  the device has hover) and while dragging. The table never overflows the
  wrapper (no min-width), so the percentages line up with the real borders
  without measuring. The wrapper is `display: flow-root`, so the table keeps
  its `0.8em` margin inside it and spacing around tables is unchanged; the
  layer is inset by that same margin.
- **Viewers.** Handles are hidden by CSS when the editor is not editable, and a
  `pointerdown` is ignored unless `view.editable`.
- **Dragging.** Pointer events (mouse, touch, pen), with pointer capture where
  available. Movement under 3px is not a drag, so a tap or click jitter writes
  nothing. Handles use `touch-action: pan-y pinch-zoom`: a vertical swipe that
  starts on a strip still scrolls (the browser cancels the pointer, which
  abandons the drag). Dragging border *i* moves width between columns *i* and *i+1*;
  every other column is untouched and the border follows the pointer. Neither
  of the two may shrink below 40px, computed against the table's current
  rendered width. `stopEvent()` claims events on the handle layer so
  ProseMirror never starts a selection from a handle.
- **Live preview.** During a drag only the `<col>` widths and the handle
  positions change in the DOM; no transactions. If a remote update to the same
  table arrives mid-drag, `update()` takes the new node but keeps rendering
  the in-progress shares, so the column does not snap back; the commit then
  rewrites every column, so the last drag wins. If the remote update changed
  the number of columns, the drag is cancelled.
- **Commit.** On `pointerup`, one transaction (one undo step) sets the
  normalised weights on every cell, located via `getPos()`. A press without
  movement writes nothing. `Escape` or `pointercancel` discards the drag and
  restores the stored widths.
- **Not included:** keyboard resizing, double-click to equalise, a "reset
  widths" control.

## Wrapping

`th, td` get `overflow-wrap: anywhere`. Column widths come from the colgroup
(`table-layout: fixed`), so this only affects how content breaks inside a
cell: long URLs and identifiers wrap instead of spilling into the next column.

## Code layout

- `web/src/features/editor/columnWidths.ts` (new) — pure functions:
  resolve stored weights to shares, move one border with the minimum-width
  clamp, normalise shares to integer weights summing exactly to 1000
  (largest-remainder rounding).
- `web/src/features/editor/TableColumnView.ts` (new) — the node view:
  wrapper, table, colgroup, handle layer, drag state, commit.
- `web/src/features/editor/TableExtensions.ts` — `resizable: false`,
  `addNodeView()` and `renderHTML()` overrides.
- `web/src/styles/prose.css` — handle styles, `overflow-wrap`, remove the
  now-unused `.column-resize-handle` / `.resize-cursor` rules. The
  `.tableWrapper` overflow rule stays as a safety net.
- `web/src/features/editor/schema.test.ts` — update the `colwidth` comment.
- `CHANGELOG.md` — Unreleased entry.

## Testing

Vitest/jsdom, written test-first:

- `columnWidths` unit tests: missing and zero weights, legacy pixel values,
  all-unknown, border move within bounds, clamping at the 40px floor on both
  sides, normalisation summing to exactly 1000.
- `TableColumnView` tests (jsdom has no layout, so the table width is
  stubbed): colgroup percentages from the node; `n-1` handles at cumulative
  positions; a simulated drag dispatches exactly one transaction whose cells
  all carry the normalised weights, including a colspan cell; no transaction
  for a press without movement, on `Escape`, or when not editable;
  `renderHTML` output carries percentages.
- Regression for the original bug: an editor created non-editable, switched
  with `setEditable(true)`, can drag a column.

e2e: a Playwright flow (a doc opened in view mode → Edit → drag a border →
reload → the share persists). Run locally against a throwaway Postgres; it
fails against the previous table extension.

Known limitation: on a narrow screen, a column dragged to the 40px floor on a
wide screen can render narrower than its cell padding.
