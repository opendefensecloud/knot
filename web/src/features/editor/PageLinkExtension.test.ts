/**
 * `[[` opens a page picker: picking a page inserts a doc link with its
 * title; the last row creates the typed page as a subpage and links it.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import { Editor } from "@tiptap/core";
import { Awareness } from "y-protocols/awareness";
import * as Y from "yjs";

import { typeText } from "../../test/editorInput";
import { createExtensions } from "./extensions";
import type { PageLinkOptions } from "./PageLinkExtension";

const PAGES = [
  { id: "t", title: "Team", parent_id: null },
  { id: "u", title: "Urlaub", parent_id: "t" },
  { id: "p", title: "Urlaubsplanung", parent_id: "u" },
  { id: "x", title: "Mein Urlaub", parent_id: null },
];

let cleanup: () => void = () => {};
afterEach(() => cleanup());

function mount(pageLinks: Partial<PageLinkOptions> = {}) {
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
      pageLinks: {
        listPages: () => Promise.resolve(PAGES),
        currentDocId: "cur",
        createSubpage: null,
        ...pageLinks,
      },
    }),
  });
  editor.view.endOfTextblock = () => true;
  cleanup = () => {
    editor.destroy();
    awareness.destroy();
    element.remove();
    ydoc.destroy();
  };
  return editor;
}

const tick = () => new Promise((r) => setTimeout(r, 0));

function popup(): HTMLElement | null {
  const el = document.querySelector<HTMLElement>(".knot-page-link-popup");
  return el && el.style.display !== "none" ? el : null;
}

const pageItems = () =>
  [...(popup()?.querySelectorAll<HTMLElement>('[data-testid="page-link-item"]') ?? [])].map(
    (row) => row.querySelector('[data-part="title"]')?.textContent,
  );

const createRow = () => popup()?.querySelector<HTMLElement>('[data-testid="page-link-create"]') ?? null;

function press(editor: Editor, key: string) {
  editor.view.dom.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
}

/** The first paragraph's text runs with their link hrefs. */
function runs(editor: Editor) {
  const out: { text: string | undefined; href: string | undefined }[] = [];
  editor.state.doc.firstChild?.forEach((n) => {
    const link = n.marks.find((m) => m.type.name === "link");
    out.push({ text: n.text, href: link ? (link.attrs.href as string) : undefined });
  });
  return out;
}

describe("[[ page links", () => {
  it("opens on [[ and filters by title", async () => {
    const editor = mount({ createSubpage: () => Promise.resolve(null) });
    typeText(editor.view, "[[urla");
    await tick();
    expect(pageItems()).toEqual(["Urlaub", "Urlaubsplanung", "Mein Urlaub"]);
    expect(createRow()?.textContent).toBe('Create subpage "urla"');
  });

  it("inserts a doc link with the page title on Enter", async () => {
    const editor = mount();
    typeText(editor.view, "[[urla");
    await tick();
    press(editor, "Enter");
    expect(runs(editor)).toEqual([
      { text: "Urlaub", href: "knot://doc/u" },
      { text: " ", href: undefined },
    ]);
    expect(popup()).toBeNull();
  });

  it("keeps spaces in the query", async () => {
    const editor = mount();
    typeText(editor.view, "[[Mein Ur");
    await tick();
    expect(pageItems()).toEqual(["Mein Urlaub"]);
  });

  it("does not open inside a code block", async () => {
    const editor = mount();
    editor.commands.setCodeBlock();
    typeText(editor.view, "[[urla");
    await tick();
    expect(popup()).toBeNull();
  });

  it("does not open inside inline code", async () => {
    const editor = mount();
    editor.commands.insertContent({ type: "text", text: "a ", marks: [{ type: "code" }] });
    typeText(editor.view, "[[urla");
    await tick();
    expect(popup()).toBeNull();
  });

  it("creates a subpage and links it", async () => {
    const createSubpage = vi.fn(() => Promise.resolve({ id: "n", title: "Neu" }));
    const editor = mount({ createSubpage });
    typeText(editor.view, "[[Neu");
    await tick();
    createRow()!.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
    await tick();
    expect(createSubpage).toHaveBeenCalledTimes(1);
    expect(createSubpage).toHaveBeenCalledWith("Neu");
    expect(runs(editor)).toEqual([
      { text: "Neu", href: "knot://doc/n" },
      { text: " ", href: undefined },
    ]);
  });

  it("creates only once on repeated picks", async () => {
    let resolve: (v: { id: string; title: string }) => void = () => {};
    const createSubpage = vi.fn(() => new Promise<{ id: string; title: string }>((r) => (resolve = r)));
    const editor = mount({ listPages: () => Promise.resolve([]), createSubpage });
    typeText(editor.view, "[[Neu");
    await tick();
    press(editor, "Enter");
    press(editor, "Enter");
    resolve({ id: "n", title: "Neu" });
    await tick();
    expect(createSubpage).toHaveBeenCalledTimes(1);
    expect(runs(editor).filter((r) => r.href === "knot://doc/n")).toHaveLength(1);
  });

  it("leaves the text when creation fails", async () => {
    const editor = mount({ listPages: () => Promise.resolve([]), createSubpage: () => Promise.resolve(null) });
    typeText(editor.view, "[[Neu");
    await tick();
    press(editor, "Enter");
    await tick();
    expect(runs(editor)).toEqual([{ text: "[[Neu", href: undefined }]);
  });

  it("shows no create row without permission", async () => {
    const editor = mount({ createSubpage: null });
    typeText(editor.view, "[[Neu");
    await tick();
    expect(createRow()).toBeNull();
  });

  it("owns arrow keys and Tab in a table cell", async () => {
    const editor = mount();
    editor.commands.insertTable({ rows: 2, cols: 2, withHeaderRow: false });
    typeText(editor.view, "[[urla");
    await tick();
    const caret = editor.state.selection.from;
    press(editor, "ArrowDown");
    expect(popup()?.querySelector('[aria-selected="true"] [data-part="title"]')?.textContent).toBe("Urlaubsplanung");
    expect(editor.state.selection.from).toBe(caret);
    press(editor, "Tab");
    expect(editor.state.selection.$from.node(-1).textContent).toBe("Urlaubsplanung ");
  });
});
