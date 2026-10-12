/**
 * TableColumnView — the node view for every table, editable or not.
 *
 * Columns render as percentages of a full-width table (see columnWidths.ts),
 * and a handle layer over the table carries one drag strip per inner column
 * border. Because the columns are percentages, a border sits at the running
 * total of the shares before it, so the strips line up with the real borders
 * without measuring anything.
 *
 * prosemirror-tables' own resizing is not used. Tiptap installs it only when
 * the editor is editable at creation, and docs open in view mode, so its
 * handles never appeared after switching to Edit. This view is installed
 * regardless and checks `view.editable` when a drag starts.
 *
 * A drag only touches the DOM while it runs and commits one transaction on
 * release, so it is one undo step and one Yjs update.
 */

import type { Node as PMNode } from "@tiptap/pm/model";
import type { EditorView, NodeView, ViewMutationRecord } from "@tiptap/pm/view";

import {
  MIN_COLUMN_PX,
  moveBorder,
  readColumnWeights,
  resolveShares,
  setColumnWeights,
  sharePercent,
  sharesEqual,
  toWeights,
} from "./columnWidths";

/**
 * Movement below this is not a drag: a tap, a click's jitter, or the first
 * pixels of a scroll swipe that the browser is about to cancel.
 */
const DRAG_THRESHOLD_PX = 3;

type Drag = {
  border: number;
  /** Set once the pointer has moved DRAG_THRESHOLD_PX from where it started. */
  active: boolean;
  pointerId: number;
  handle: HTMLElement;
  startX: number;
  tableWidth: number;
  startShares: number[];
  shares: number[];
};

function syncChildCount(parent: HTMLElement, count: number, create: () => HTMLElement) {
  while (parent.children.length < count) parent.appendChild(create());
  while (parent.children.length > count) parent.lastElementChild?.remove();
}

function setStyle(element: Element | undefined, property: "width" | "left", value: string) {
  if (!(element instanceof HTMLElement) || element.style[property] === value) return;
  element.style[property] = value;
}

export class TableColumnView implements NodeView {
  dom: HTMLDivElement;
  contentDOM: HTMLTableSectionElement;

  private node: PMNode;
  private readonly view: EditorView;
  private readonly getPos: () => number | undefined;
  private readonly table: HTMLTableElement;
  private readonly colgroup: HTMLTableColElement;
  private readonly handleLayer: HTMLDivElement;
  private drag: Drag | null = null;

  constructor(
    node: PMNode,
    view: EditorView,
    getPos: () => number | undefined,
    htmlAttributes: Record<string, string | number | boolean | null | undefined> = {},
  ) {
    this.node = node;
    this.view = view;
    this.getPos = getPos;

    const doc = view.dom.ownerDocument;
    this.dom = doc.createElement("div");
    this.dom.className = "tableWrapper";
    this.table = this.dom.appendChild(doc.createElement("table"));
    for (const [name, value] of Object.entries(htmlAttributes)) {
      if (value !== undefined && value !== null) this.table.setAttribute(name, String(value));
    }
    this.colgroup = this.table.appendChild(doc.createElement("colgroup"));
    this.contentDOM = this.table.appendChild(doc.createElement("tbody"));

    this.handleLayer = this.dom.appendChild(doc.createElement("div"));
    this.handleLayer.className = "knot-col-handles";
    this.handleLayer.contentEditable = "false";
    this.handleLayer.addEventListener("pointerdown", this.onPointerDown);

    this.render(this.storedShares());
  }

  update(node: PMNode): boolean {
    if (node.type !== this.node.type) return false;
    this.node = node;
    // A remote change mid-drag: keep showing the drag (the commit rewrites
    // every column, so the last drag wins) unless the column count changed,
    // which would make the in-progress shares meaningless.
    if (this.drag && readColumnWeights(node).length !== this.drag.shares.length) this.endDrag();
    this.render(this.drag ? this.drag.shares : this.storedShares());
    return true;
  }

  /** Keeps ProseMirror from starting a selection on a handle. */
  stopEvent(event: Event): boolean {
    return event.target instanceof Node && this.handleLayer.contains(event.target);
  }

  /** Only the cell content is ProseMirror's; colgroup and handles are ours. */
  ignoreMutation(mutation: ViewMutationRecord): boolean {
    if (mutation.type === "selection") return false;
    return !this.contentDOM.contains(mutation.target);
  }

  destroy(): void {
    this.endDrag();
    this.handleLayer.removeEventListener("pointerdown", this.onPointerDown);
  }

  private storedShares(): number[] {
    return resolveShares(readColumnWeights(this.node));
  }

  private render(shares: readonly number[]): void {
    const doc = this.dom.ownerDocument;
    syncChildCount(this.colgroup, shares.length, () => doc.createElement("col"));
    shares.forEach((share, column) => setStyle(this.colgroup.children[column], "width", sharePercent(share)));

    syncChildCount(this.handleLayer, Math.max(shares.length - 1, 0), () => {
      const handle = doc.createElement("div");
      handle.className = "knot-col-handle";
      return handle;
    });
    let left = 0;
    for (let border = 0; border < shares.length - 1; border++) {
      left += shares[border] ?? 0;
      const handle = this.handleLayer.children[border];
      if (handle instanceof HTMLElement) handle.dataset.border = String(border);
      setStyle(handle, "left", sharePercent(left));
    }
  }

  private onPointerDown = (event: PointerEvent): void => {
    const handle = event.target instanceof Element ? event.target.closest<HTMLElement>(".knot-col-handle") : null;
    if (!handle || this.drag || !this.view.editable || event.button !== 0) return;
    const tableWidth = this.table.getBoundingClientRect().width;
    if (tableWidth <= 0) return;

    event.preventDefault();
    const startShares = this.storedShares();
    this.drag = {
      border: Number(handle.dataset.border),
      active: false,
      pointerId: event.pointerId,
      handle,
      startX: event.clientX,
      tableWidth,
      startShares,
      shares: startShares,
    };
    handle.classList.add("is-dragging");
    // Absent in jsdom; in browsers it keeps the drag alive outside the window.
    if (typeof handle.setPointerCapture === "function") handle.setPointerCapture(event.pointerId);

    const doc = this.dom.ownerDocument;
    doc.addEventListener("pointermove", this.onPointerMove);
    doc.addEventListener("pointerup", this.onPointerUp);
    doc.addEventListener("pointercancel", this.onPointerCancel);
    doc.addEventListener("keydown", this.onKeyDown);
  };

  private onPointerMove = (event: PointerEvent): void => {
    const drag = this.drag;
    if (!drag || event.pointerId !== drag.pointerId) return;
    const offset = event.clientX - drag.startX;
    if (!drag.active && Math.abs(offset) < DRAG_THRESHOLD_PX) return;
    drag.active = true;
    const delta = offset / drag.tableWidth;
    drag.shares = moveBorder(drag.startShares, drag.border, delta, MIN_COLUMN_PX / drag.tableWidth);
    this.render(drag.shares);
  };

  private onPointerUp = (event: PointerEvent): void => {
    const drag = this.drag;
    if (!drag || event.pointerId !== drag.pointerId) return;
    this.onPointerMove(event);
    this.endDrag();
    if (!drag.active || sharesEqual(drag.shares, drag.startShares)) {
      this.render(this.storedShares());
      return;
    }
    this.commit(drag.shares);
  };

  private onPointerCancel = (event: PointerEvent): void => {
    if (this.drag && event.pointerId === this.drag.pointerId) this.cancelDrag();
  };

  private onKeyDown = (event: KeyboardEvent): void => {
    if (event.key !== "Escape" || !this.drag) return;
    event.preventDefault();
    this.cancelDrag();
  };

  /** Writes the drag; anything that stops it falls back to the stored widths. */
  private commit(shares: readonly number[]): void {
    const pos = this.getPos();
    const table = pos === undefined ? null : this.view.state.doc.nodeAt(pos);
    if (pos !== undefined && table?.type === this.node.type && readColumnWeights(table).length === shares.length) {
      const tr = setColumnWeights(this.view.state.tr, pos + 1, table, toWeights(shares));
      if (tr.docChanged) {
        this.view.dispatch(tr);
        return;
      }
    }
    this.render(this.storedShares());
  }

  private cancelDrag(): void {
    this.endDrag();
    this.render(this.storedShares());
  }

  private endDrag(): void {
    const drag = this.drag;
    if (!drag) return;
    this.drag = null;
    drag.handle.classList.remove("is-dragging");
    if (typeof drag.handle.hasPointerCapture === "function" && drag.handle.hasPointerCapture(drag.pointerId)) {
      drag.handle.releasePointerCapture(drag.pointerId);
    }
    const doc = this.dom.ownerDocument;
    doc.removeEventListener("pointermove", this.onPointerMove);
    doc.removeEventListener("pointerup", this.onPointerUp);
    doc.removeEventListener("pointercancel", this.onPointerCancel);
    doc.removeEventListener("keydown", this.onKeyDown);
  }
}
