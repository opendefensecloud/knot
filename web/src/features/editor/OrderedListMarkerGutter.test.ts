/**
 * Numbered lists from item 10 on (#95, '"10." shows as ".0."').
 *
 * An ordered list's markers hang outside its content box, in a gutter sized
 * for one digit. Each <ol> carries the digit count of its widest marker so
 * the stylesheet can widen that gutter; these tests pin the count, including
 * the `start` attribute and edits that cross a power of ten. Whether the
 * marker then actually fits is a layout question, answered in a real browser
 * by e2e/flows/editor-lists.spec.ts.
 */

import type { JSONContent } from "@tiptap/core";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { MARKER_DIGITS_PROPERTY, markerDigits } from "./OrderedListMarkerGutter";

function listItem(text: string, ...nested: JSONContent[]): JSONContent {
  return {
    type: "list_item",
    content: [{ type: "paragraph", content: [{ type: "text", text }] }, ...nested],
  };
}

function items(count: number): JSONContent[] {
  return Array.from({ length: count }, (_, index) => listItem(`item ${index + 1}`));
}

function orderedList(count: number, start?: number): JSONContent {
  return { type: "ordered_list", ...(start ? { attrs: { start } } : {}), content: items(count) };
}

describe("markerDigits", () => {
  it("counts the digits of the last item's number", () => {
    expect(markerDigits(1, 1)).toBe(1);
    expect(markerDigits(1, 9)).toBe(1);
    expect(markerDigits(1, 10)).toBe(2);
    expect(markerDigits(1, 99)).toBe(2);
    expect(markerDigits(1, 100)).toBe(3);
  });

  it("starts counting at the list's start number", () => {
    // 95 … 104: ten items, but the last marker is "104."
    expect(markerDigits(95, 10)).toBe(3);
    expect(markerDigits(10, 1)).toBe(2);
  });

  it("treats a missing or non-numeric start as 1", () => {
    expect(markerDigits(undefined, 10)).toBe(2);
    expect(markerDigits(null, 9)).toBe(1);
    expect(markerDigits("", 9)).toBe(1);
    expect(markerDigits(Number.NaN, 9)).toBe(1);
    expect(markerDigits("12", 1)).toBe(2);
  });

  it("numbers from zero and negative starts, as a browser does", () => {
    // 0 … 8: the widest marker is still one digit.
    expect(markerDigits(0, 9)).toBe(1);
    // -15, -14, -13: the first marker is the widest, minus sign included.
    expect(markerDigits(-15, 3)).toBe(3);
    // -1, 0, 1, …, 9: "-1." is two characters wide.
    expect(markerDigits(-1, 11)).toBe(2);
  });
});

describe("ordered list marker gutter", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function mountWith(content: JSONContent[]): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.setContent({ type: "doc", content });
    return bound;
  }

  /** The digit count each rendered <ol> carries, in document order. */
  function renderedDigits(b: BoundEditor): string[] {
    return Array.from(b.editor.view.dom.querySelectorAll("ol")).map((ol) =>
      ol.style.getPropertyValue(MARKER_DIGITS_PROPERTY),
    );
  }

  it("leaves a single-digit list on the stylesheet default", () => {
    const b = mountWith([orderedList(9)]);
    expect(renderedDigits(b)).toEqual([""]);
  });

  it("widens a list that reaches item 10", () => {
    const b = mountWith([orderedList(10)]);
    expect(renderedDigits(b)).toEqual(["2"]);
  });

  it("widens a list that reaches item 100", () => {
    const b = mountWith([orderedList(100)]);
    expect(renderedDigits(b)).toEqual(["3"]);
  });

  it("widens a short list whose start number is already wide", () => {
    const b = mountWith([orderedList(10, 95)]);
    expect(renderedDigits(b)).toEqual(["3"]);
  });

  it("widens a list pasted with a negative start for its minus sign", () => {
    const b = mountWith([orderedList(3, -15)]);
    expect(renderedDigits(b)).toEqual(["3"]);
  });

  it("sizes each list on its own, nested lists included", () => {
    const firstItem = listItem("item 1", orderedList(3));
    const outer: JSONContent = { type: "ordered_list", content: [firstItem, ...items(11)] };
    const b = mountWith([outer, orderedList(4)]);
    // Document order: the outer list, the list nested in its first item,
    // then the separate short list.
    expect(renderedDigits(b)).toEqual(["2", "", ""]);
  });

  it("follows edits that cross a power of ten, both ways", () => {
    const b = mountWith([orderedList(9)]);
    expect(renderedDigits(b)).toEqual([""]);

    // Enter at the end of item 9 adds item 10.
    b.editor.commands.setTextSelection(b.editor.state.doc.content.size - 3);
    b.editor.commands.splitListItem("list_item");
    expect(b.editor.state.doc.firstChild!.childCount).toBe(10);
    expect(renderedDigits(b)).toEqual(["2"]);

    // Deleting item 10 again narrows it back.
    const list = b.editor.state.doc.firstChild!;
    const lastItem = list.lastChild!;
    const lastItemStart = 1 + list.content.size - lastItem.nodeSize;
    b.editor.view.dispatch(
      b.editor.state.tr.delete(lastItemStart, lastItemStart + lastItem.nodeSize),
    );
    expect(b.editor.state.doc.firstChild!.childCount).toBe(9);
    expect(renderedDigits(b)).toEqual([""]);
  });

  it("follows a change to the start number alone", () => {
    const b = mountWith([orderedList(3)]);
    b.editor.commands.updateAttributes("ordered_list", { start: 98 });
    expect(renderedDigits(b)).toEqual(["3"]);
  });

  it("does not touch bullet lists", () => {
    const b = mountWith([{ type: "bullet_list", content: items(12) }]);
    const ul = b.editor.view.dom.querySelector("ul")!;
    expect(ul.style.getPropertyValue(MARKER_DIGITS_PROPERTY)).toBe("");
  });
});
