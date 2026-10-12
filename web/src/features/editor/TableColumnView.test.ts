/**
 * Column resizing through the table node view, against a real Yjs-bound
 * editor. jsdom has no layout, so the table's rendered width is stubbed;
 * everything else — the node view, the transactions, the stored `colwidth`
 * — is the shipped code path.
 */

import type { Node as PMNode } from "@tiptap/pm/model";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { setColumnWeights } from "./columnWidths";

const TABLE_WIDTH = 800;
const START_X = 100;

let bound: BoundEditor | null = null;

afterEach(() => {
  bound?.destroy();
  bound = null;
});

const row = (tag: "th" | "td", count: number) =>
  `<tr>${Array.from({ length: count }, (_, i) => `<${tag}><p>${tag}${i}</p></${tag}>`).join("")}</tr>`;

/** Four equal columns: a header row and one body row. */
const FOUR_COLUMNS = `<table>${row("th", 4)}${row("td", 4)}</table>`;

function mount(html: string, opts: { editable?: boolean } = {}): BoundEditor {
  bound = mountBoundEditor(opts);
  bound.editor.commands.setContent(html);
  stubTableWidth(bound);
  return bound;
}

function tableElement(b: BoundEditor): HTMLTableElement {
  return b.editor.view.dom.querySelector("table")!;
}

function stubTableWidth(b: BoundEditor) {
  const table = tableElement(b);
  if (!table) return;
  table.getBoundingClientRect = () =>
    ({ width: TABLE_WIDTH, height: 100, top: 0, left: 0, right: TABLE_WIDTH, bottom: 100, x: 0, y: 0 }) as DOMRect;
}

function tableNode(b: BoundEditor): { node: PMNode; pos: number } {
  let found: { node: PMNode; pos: number } | null = null;
  b.editor.state.doc.descendants((node, pos) => {
    if (!found && node.type.name === "table") found = { node, pos };
    return !found;
  });
  return found!;
}

/** colwidth of every cell, row by row. */
function storedColwidths(b: BoundEditor): (number[] | null)[][] {
  const rows: (number[] | null)[][] = [];
  tableNode(b).node.forEach((tableRow) => {
    const cells: (number[] | null)[] = [];
    tableRow.forEach((cell) => cells.push(cell.attrs.colwidth as number[] | null));
    rows.push(cells);
  });
  return rows;
}

const colWidths = (b: BoundEditor) =>
  Array.from(tableElement(b).querySelectorAll("col")).map((col) => col.style.width);

const handle = (b: BoundEditor, border: number) =>
  b.editor.view.dom.querySelector<HTMLElement>(`.knot-col-handle[data-border="${border}"]`);

function countDocChanges(b: BoundEditor): () => number {
  let count = 0;
  b.editor.on("transaction", ({ transaction }) => {
    if (transaction.docChanged) count++;
  });
  return () => count;
}

function pointerDown(b: BoundEditor, border: number) {
  handle(b, border)!.dispatchEvent(
    new PointerEvent("pointerdown", { bubbles: true, clientX: START_X, button: 0, pointerId: 1 }),
  );
}

function pointerMove(dx: number) {
  document.dispatchEvent(new PointerEvent("pointermove", { bubbles: true, clientX: START_X + dx, pointerId: 1 }));
}

function pointerUp(dx: number) {
  document.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, clientX: START_X + dx, pointerId: 1 }));
}

function drag(b: BoundEditor, border: number, dx: number) {
  pointerDown(b, border);
  pointerMove(dx);
  pointerUp(dx);
}

describe("table column view: rendering", () => {
  it("renders stored weights as a percentage colgroup", () => {
    const b = mount(
      `<table><tr><td colwidth="100"><p>a</p></td><td colwidth="300"><p>b</p></td></tr></table>`,
    );
    expect(colWidths(b)).toEqual(["25%", "75%"]);
  });

  it("gives an unresized table equal columns", () => {
    const b = mount(FOUR_COLUMNS);
    expect(colWidths(b)).toEqual(["25%", "25%", "25%", "25%"]);
  });

  it("puts one handle on each inner border, at the running total", () => {
    const b = mount(FOUR_COLUMNS);
    const handles = Array.from(b.editor.view.dom.querySelectorAll<HTMLElement>(".knot-col-handle"));
    expect(handles.map((h) => h.style.left)).toEqual(["25%", "50%", "75%"]);
  });

  it("keeps the table inside its wrapper", () => {
    const b = mount(FOUR_COLUMNS);
    const table = tableElement(b);
    expect(table.parentElement?.classList.contains("tableWrapper")).toBe(true);
    expect(table.classList.contains("knot-table")).toBe(true);
    expect(table.style.width).toBe("");
    expect(table.style.minWidth).toBe("");
  });

  it("serialises percentage columns to HTML, so pasting elsewhere keeps the proportions", () => {
    const b = mount(
      `<table><tr><td colwidth="100"><p>a</p></td><td colwidth="300"><p>b</p></td></tr></table>`,
    );
    const html = b.editor.getHTML();
    expect(html).toContain('<col style="width: 25%;">');
    expect(html).toContain('<col style="width: 75%;">');
  });

  it("never installs prosemirror-tables' pixel resizing", () => {
    const b = mount(FOUR_COLUMNS);
    const keys = b.editor.state.plugins.map((plugin) => (plugin as unknown as { key: string }).key);
    expect(keys.some((key) => key.startsWith("tableColumnResizing"))).toBe(false);
  });
});

describe("table column view: dragging", () => {
  it("commits a drag once, writing the new proportions to every cell", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    drag(b, 0, 80);

    expect(docChanges()).toBe(1);
    expect(storedColwidths(b)).toEqual([
      [[350], [150], [250], [250]],
      [[350], [150], [250], [250]],
    ]);
    expect(colWidths(b)).toEqual(["35%", "15%", "25%", "25%"]);
  });

  it("follows the pointer live and writes nothing until release", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    pointerDown(b, 0);
    pointerMove(80);

    expect(colWidths(b)).toEqual(["35%", "15%", "25%", "25%"]);
    expect(handle(b, 0)!.style.left).toBe("35%");
    expect(docChanges()).toBe(0);

    pointerUp(80);
    expect(docChanges()).toBe(1);
  });

  it("gives a colspan cell one width per column it spans", () => {
    const b = mount(
      `<table><tr><th colspan="2"><p>wide</p></th><th><p>c</p></th><th><p>d</p></th></tr>${row("td", 4)}</table>`,
    );

    drag(b, 1, 80);

    expect(storedColwidths(b)).toEqual([
      [[250, 350], [150], [250]],
      [[250], [350], [150], [250]],
    ]);
  });

  it("stops a column at 40px", () => {
    const b = mount(FOUR_COLUMNS);

    drag(b, 0, -400);

    // 40px of an 800px table is 5%.
    expect(storedColwidths(b)[0]).toEqual([[50], [450], [250], [250]]);
  });

  it("ignores jitter of a few pixels, so a tap or a scroll swipe changes nothing", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    pointerDown(b, 0);
    pointerMove(2);
    expect(colWidths(b)).toEqual(["25%", "25%", "25%", "25%"]);
    pointerUp(-2);

    expect(docChanges()).toBe(0);
  });

  it("writes nothing for a press without movement", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    drag(b, 0, 0);

    expect(docChanges()).toBe(0);
    expect(storedColwidths(b)[0]).toEqual([null, null, null, null]);
  });

  it("abandons the drag on Escape and restores the stored widths", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    pointerDown(b, 0);
    pointerMove(80);
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    pointerUp(80);

    expect(docChanges()).toBe(0);
    expect(colWidths(b)).toEqual(["25%", "25%", "25%", "25%"]);
  });

  it("abandons the drag when the pointer is cancelled", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    pointerDown(b, 0);
    pointerMove(80);
    document.dispatchEvent(new PointerEvent("pointercancel", { bubbles: true, pointerId: 1 }));
    pointerUp(80);

    expect(docChanges()).toBe(0);
    expect(colWidths(b)).toEqual(["25%", "25%", "25%", "25%"]);
  });

  it("does not let a viewer drag", () => {
    const b = mount(FOUR_COLUMNS);
    b.editor.setEditable(false);
    const docChanges = countDocChanges(b);

    drag(b, 0, 80);

    expect(docChanges()).toBe(0);
    expect(colWidths(b)).toEqual(["25%", "25%", "25%", "25%"]);
  });

  it("lets an editor created read-only drag once it is switched to editing", () => {
    // Docs open in view mode; the editor is created read-only and switched
    // with setEditable(). prosemirror-tables' resizing was decided at
    // creation, so the handles never appeared for existing docs.
    bound = mountBoundEditor({ editable: false });
    bound.editor.commands.setContent(FOUR_COLUMNS);
    bound.editor.setEditable(true);
    stubTableWidth(bound);
    const docChanges = countDocChanges(bound);

    drag(bound, 0, 80);

    expect(docChanges()).toBe(1);
    expect(storedColwidths(bound)[0]).toEqual([[350], [150], [250], [250]]);
  });

  it("keeps the dragged column under the pointer when someone else resizes mid-drag", () => {
    const b = mount(FOUR_COLUMNS);

    pointerDown(b, 0);
    pointerMove(80);
    const { node, pos } = tableNode(b);
    b.editor.view.dispatch(setColumnWeights(b.editor.state.tr, pos + 1, node, [100, 100, 100, 700]));

    expect(colWidths(b)).toEqual(["35%", "15%", "25%", "25%"]);

    pointerUp(80);
    expect(storedColwidths(b)[0]).toEqual([[350], [150], [250], [250]]);
  });

  it("cancels the drag when a column is added mid-drag", () => {
    const b = mount(FOUR_COLUMNS);
    const docChanges = countDocChanges(b);

    pointerDown(b, 0);
    pointerMove(80);
    const { pos } = tableNode(b);
    // Inside the first header cell's paragraph: table > row > cell > paragraph.
    b.editor.commands.setTextSelection(pos + 4);
    b.editor.commands.addColumnAfter();
    pointerUp(80);

    expect(docChanges()).toBe(1); // the added column; the drag wrote nothing
    expect(colWidths(b)).toEqual(["20%", "20%", "20%", "20%", "20%"]);
  });
});
