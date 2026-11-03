/**
 * Keys and clicks in and around tables, on the shipped extension set.
 *
 * - Tab / Shift-Tab: Tiptap runs the table's shortcuts before the lists' and
 *   code blocks', so in a cell they moved to the next/previous cell (and
 *   selected it whole) instead of indenting a list item or a code line.
 * - ArrowDown / a click below a table that ends the page: with no trailing
 *   paragraph there was no way below it.
 *
 * jsdom has no layout: `endOfTextblock` is stubbed the way a browser answers
 * for a one-line block, and every box measures 0.
 */

import { afterEach, describe, expect, it } from "vitest";
import type { JSONContent } from "@tiptap/core";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";

let bound: BoundEditor | null = null;
afterEach(() => {
  bound?.destroy();
  bound = null;
});

const text = (t: string): JSONContent => ({ type: "text", text: t });
const p = (t = ""): JSONContent => ({ type: "paragraph", content: t ? [text(t)] : [] });
const li = (...content: JSONContent[]): JSONContent => ({ type: "list_item", content });
const ul = (...items: JSONContent[]): JSONContent => ({ type: "bullet_list", content: items });
const code = (t: string): JSONContent => ({ type: "code_block", content: [text(t)] });
const cell = (...content: JSONContent[]): JSONContent => ({ type: "table_cell", content });
const row = (...cells: JSONContent[]): JSONContent => ({ type: "table_row", content: cells });
const table = (...rows: JSONContent[]): JSONContent => ({ type: "table", content: rows });

function mount(...content: JSONContent[]) {
  bound = mountBoundEditor();
  bound.editor.commands.setContent({ type: "doc", content });
  bound.editor.view.endOfTextblock = () => true;
  return bound;
}

/** Puts the caret right after the first occurrence of `word`. */
function caretAfter(b: BoundEditor, word: string) {
  let at = -1;
  b.editor.state.doc.descendants((node, pos) => {
    if (at >= 0) return false;
    if (node.isText && node.text?.includes(word)) at = pos + node.text.indexOf(word) + word.length;
    return true;
  });
  if (at < 0) throw new Error(`no "${word}"`);
  placeSelection(b, at);
}

function press(b: BoundEditor, key: string, shiftKey = false) {
  const event = new KeyboardEvent("keydown", { key, shiftKey, bubbles: true, cancelable: true });
  b.editor.view.dom.dispatchEvent(event);
}

/** The block types from the caret up to the doc, innermost first. */
function caretPath(b: BoundEditor): string[] {
  const { $from } = b.editor.state.selection;
  const path: string[] = [];
  for (let depth = $from.depth; depth > 0; depth -= 1) path.push($from.node(depth).type.name);
  return path;
}

/** The document as `type(children)` with text quoted, attributes left out. */
function outline(node: JSONContent): string {
  if (node.type === "text") return JSON.stringify(node.text);
  return `${node.type}(${(node.content ?? []).map(outline).join(",")})`;
}
const shape = (...content: JSONContent[]) => content.map(outline).join(" ");
const doc = (b: BoundEditor) => shape(...(b.editor.getJSON().content ?? []));

describe("Tab in a table cell", () => {
  it("indents a list item that can be indented", () => {
    const b = mount(table(row(cell(ul(li(p("one")), li(p("two")))), cell(p("x")))));
    caretAfter(b, "two");
    press(b, "Tab");
    expect(doc(b)).toBe(shape(table(row(cell(ul(li(p("one"), ul(li(p("two")))))), cell(p("x"))))));
  });

  it("outdents a nested list item on Shift-Tab", () => {
    // The second cell: from the first, Shift-Tab has no cell to go back to.
    const b = mount(table(row(cell(p("x")), cell(ul(li(p("one"), ul(li(p("two")))))))));
    caretAfter(b, "two");
    press(b, "Tab", true);
    expect(doc(b)).toBe(shape(table(row(cell(p("x")), cell(ul(li(p("one")), li(p("two"))))))));
  });

  it("still moves to the next cell from a list item that cannot be indented", () => {
    const b = mount(table(row(cell(ul(li(p("one")))), cell(p("x")))));
    caretAfter(b, "one");
    press(b, "Tab");
    expect(b.editor.state.selection.$from.parent.textContent).toBe("x");
  });

  it("indents a line in a code block", () => {
    const b = mount(table(row(cell(code("let a")), cell(p("x")))));
    placeSelection(b, 4); // start of the code line
    press(b, "Tab");
    expect(b.editor.state.doc.textBetween(0, b.editor.state.doc.content.size, "|")).toContain("  let a");
    expect(caretPath(b)[0]).toBe("code_block");
  });

  it("dedents a line in a code block on Shift-Tab", () => {
    const b = mount(table(row(cell(p("x")), cell(code("  let a")))));
    caretAfter(b, "let");
    press(b, "Tab", true);
    expect(doc(b)).toBe(shape(table(row(cell(p("x")), cell(code("let a"))))));
  });

  it("moves to the next cell from a paragraph", () => {
    const b = mount(table(row(cell(p("a")), cell(p("x")))));
    caretAfter(b, "a");
    press(b, "Tab");
    expect(b.editor.state.selection.$from.parent.textContent).toBe("x");
  });

  it("moves between cells of a table that sits in a list item", () => {
    const b = mount(ul(li(p("one")), li(p("two"), table(row(cell(p("a")), cell(p("x")))))));
    caretAfter(b, "a");
    press(b, "Tab");
    expect(b.editor.state.selection.$from.parent.textContent).toBe("x");
    expect(doc(b)).toBe(shape(ul(li(p("one")), li(p("two"), table(row(cell(p("a")), cell(p("x"))))))));
  });
});

describe("leaving a table that ends the page", () => {
  it("ArrowDown in the last row opens a paragraph below the table", () => {
    const b = mount(p("before"), table(row(cell(p("h")), cell(p("i"))), row(cell(p("a")), cell(p("x")))));
    caretAfter(b, "a");
    press(b, "ArrowDown");
    expect(doc(b)).toBe(shape(
      p("before"),
      table(row(cell(p("h")), cell(p("i"))), row(cell(p("a")), cell(p("x")))),
      p(),
    ));
    expect(caretPath(b)).toEqual(["paragraph"]);
  });

  it("ArrowDown adds nothing when the table is followed by something", () => {
    const b = mount(table(row(cell(p("a")))), p("after"));
    caretAfter(b, "a");
    press(b, "ArrowDown");
    expect(doc(b)).toBe(shape(table(row(cell(p("a")))), p("after")));
  });

  it("ArrowDown adds nothing from a row above the last", () => {
    const b = mount(table(row(cell(p("h"))), row(cell(p("a")))));
    caretAfter(b, "h");
    press(b, "ArrowDown");
    expect(doc(b)).toBe(shape(table(row(cell(p("h"))), row(cell(p("a"))))));
  });

  it("ArrowDown adds nothing above the last block of a cell", () => {
    const b = mount(table(row(cell(p("a"), p("b")))));
    caretAfter(b, "a");
    press(b, "ArrowDown");
    expect(doc(b)).toBe(shape(table(row(cell(p("a"), p("b"))))));
  });

  it("a click below the table opens a paragraph there", () => {
    const b = mount(p("before"), table(row(cell(p("a")))));
    const event = new MouseEvent("mousedown", { button: 0, clientY: 50, bubbles: true, cancelable: true });
    b.editor.view.dom.dispatchEvent(event);
    expect(event.defaultPrevented).toBe(true);
    expect(doc(b)).toBe(shape(p("before"), table(row(cell(p("a")))), p()));
    expect(caretPath(b)).toEqual(["paragraph"]);
  });

  it("a click in a cell adds nothing", () => {
    const b = mount(p("before"), table(row(cell(p("a")))));
    const td = b.editor.view.dom.querySelector("td")!;
    td.dispatchEvent(new MouseEvent("mousedown", { button: 0, clientY: 50, bubbles: true, cancelable: true }));
    expect(doc(b)).toBe(shape(p("before"), table(row(cell(p("a"))))));
  });

  it("a click below adds nothing in view mode", () => {
    bound = mountBoundEditor({ editable: false });
    bound.editor.commands.setContent({ type: "doc", content: [table(row(cell(p("a"))))] });
    bound.editor.view.dom.dispatchEvent(new MouseEvent("mousedown", { button: 0, clientY: 50, bubbles: true }));
    expect(doc(bound)).toBe(shape(table(row(cell(p("a"))))));
  });
});
