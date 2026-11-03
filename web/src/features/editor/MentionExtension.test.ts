/**
 * The @-mention popup owns the arrow keys, Enter and Tab while it is open —
 * also inside a table cell, where prosemirror-tables moves ArrowUp/ArrowDown
 * to the neighbouring cell and Table binds Tab to the next cell. Tiptap runs
 * a later extension's plugins first, and the table extensions are registered
 * after MentionExtension, so in a cell those keys never reached the popup.
 */

import { afterEach, describe, expect, it } from "vitest";
import { Editor } from "@tiptap/core";
import { Awareness } from "y-protocols/awareness";
import * as Y from "yjs";

import { typeText } from "../../test/editorInput";
import { createExtensions } from "./extensions";

const MEMBERS = [
  { user_id: "a", display_name: "Alice" },
  { user_id: "b", display_name: "Bob" },
  { user_id: "c", display_name: "Carol" },
];

let cleanup: () => void = () => {};
afterEach(() => cleanup());

function mount() {
  const ydoc = new Y.Doc();
  const awareness = new Awareness(ydoc);
  const element = document.createElement("div");
  document.body.appendChild(element);
  const editor = new Editor({
    element,
    extensions: createExtensions({
      doc: ydoc,
      awareness,
      user: { name: "Test User", color: "#336699" },
      fetchMembers: () => Promise.resolve(MEMBERS),
    }),
  });
  // jsdom has no layout; in a browser a caret in a one-line cell is at the
  // top and bottom of its textblock, which is when prosemirror-tables takes
  // the vertical arrows.
  editor.view.endOfTextblock = () => true;
  cleanup = () => {
    editor.destroy();
    awareness.destroy();
    element.remove();
    ydoc.destroy();
  };
  return editor;
}

async function openPopup(editor: Editor) {
  typeText(editor.view, "@");
  // The member list resolves asynchronously.
  await new Promise((r) => setTimeout(r, 0));
  const popup = document.querySelector<HTMLElement>(".knot-mention-popup");
  if (!popup || popup.style.display === "none") throw new Error("mention popup did not open");
  return popup;
}

function press(editor: Editor, key: string) {
  const event = new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true });
  editor.view.dom.dispatchEvent(event);
}

const highlighted = (popup: HTMLElement) =>
  popup.querySelector('[aria-selected="true"]')?.textContent;

describe("mention popup in a table cell", () => {
  function inCell(editor: Editor) {
    editor.commands.insertTable({ rows: 2, cols: 2, withHeaderRow: false });
    // insertTable leaves the caret in the first cell.
    expect(editor.state.selection.$from.parent.type.name).toBe("paragraph");
    expect(editor.state.selection.$from.node(-1).type.name).toBe("table_cell");
  }

  it("ArrowDown and ArrowUp move the highlight instead of the caret", async () => {
    const editor = mount();
    inCell(editor);
    const popup = await openPopup(editor);
    const caret = editor.state.selection.from;

    press(editor, "ArrowDown");
    expect(highlighted(popup)).toBe("Bob");
    press(editor, "ArrowDown");
    expect(highlighted(popup)).toBe("Carol");
    press(editor, "ArrowUp");
    expect(highlighted(popup)).toBe("Bob");
    expect(editor.state.selection.from).toBe(caret);
  });

  it("Tab picks the highlighted member instead of moving to the next cell", async () => {
    const editor = mount();
    inCell(editor);
    const popup = await openPopup(editor);

    press(editor, "ArrowDown");
    press(editor, "Tab");
    expect(popup.style.display).toBe("none");
    const cell = editor.state.selection.$from.node(-1);
    expect(cell.textContent).toBe("@Bob ");
  });
});
