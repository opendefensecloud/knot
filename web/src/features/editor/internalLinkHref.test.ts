/**
 * Doc links, @-mentions and date chips keep their `knot://` href in the DOM.
 *
 * Tiptap's Link renders `href=""` for any scheme its `isAllowedUri` refuses,
 * and refuses to parse an `<a>` without an allowed href. `knot://` was not
 * allowed, so the editor's DOM never said where these links point:
 *
 * - Safari restructures the DOM when you type right after one, ProseMirror
 *   re-reads that DOM, and the link turned into plain text — written into
 *   the shared document.
 * - Clicking a doc link did not navigate and clicking a date chip did not
 *   open its picker: both look the href up on the clicked `<a>`.
 * - Copying one within knot pasted plain text.
 * - The toolbar's doc picker could not link selected text: `setLink` refused
 *   the href and nothing happened.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import { Editor } from "@tiptap/core";
import { DOMParser } from "@tiptap/pm/model";
import type { EditorView } from "@tiptap/pm/view";
import { Awareness } from "y-protocols/awareness";
import * as Y from "yjs";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { linkRuns, pasteEvent, placeSelection, typeText } from "../../test/editorInput";
import { createExtensions } from "./extensions";

const DOC = "knot://doc/0b8f1c1e-2c1a-4c55-9a39-2f0e6f7f0a11";
const USER = "knot://user/5d6c7b8a-1111-4222-8333-944455556666";
const TIME = "knot://time/2026-11-02T09:00:00.000Z";

function withLink(bound: BoundEditor, href: string) {
  bound.editor.commands.setContent({
    type: "doc",
    content: [
      {
        type: "paragraph",
        content: [
          { type: "text", text: "see " },
          { type: "text", text: "Roadmap", marks: [{ type: "link", attrs: { href } }] },
        ],
      },
    ],
  });
}

function renderedAnchor(bound: BoundEditor): HTMLAnchorElement {
  const anchor = bound.editor.view.dom.querySelector("a");
  if (!anchor) throw new Error("no <a> rendered");
  return anchor;
}

/** What prosemirror-view does on a click: the `handleClickOn` props, with
 *  the clicked element as the event target. jsdom has no layout for
 *  `posAtCoords`, so the position is given. */
function clickOn(view: EditorView, target: Element, pos: number, init: MouseEventInit = {}) {
  const event = new MouseEvent("click", { button: 0, bubbles: true, cancelable: true, ...init });
  Object.defineProperty(event, "target", { value: target });
  const $pos = view.state.doc.resolve(pos);
  return view.someProp("handleClickOn", (handler) =>
    handler(view, pos, $pos.parent, $pos.before(), event, true),
  ) ?? false;
}

describe("internal links in the editor's DOM", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it.each([DOC, USER, TIME])("renders %s with its href", (href) => {
    bound = mountBoundEditor();
    withLink(bound, href);

    expect(renderedAnchor(bound).getAttribute("href")).toBe(href);
  });

  it.each([DOC, USER, TIME])("parses the rendered DOM of %s back into the same link", (href) => {
    // Safari's typing makes ProseMirror re-read exactly this DOM.
    bound = mountBoundEditor();
    withLink(bound, href);
    const reparsed = DOMParser.fromSchema(bound.editor.schema).parse(bound.editor.view.dom);

    expect(reparsed.eq(bound.editor.state.doc)).toBe(true);
  });

  it("keeps a doc link pasted from knot", () => {
    bound = mountBoundEditor();
    bound.editor.view.pasteHTML(`<a href="${DOC}">Roadmap</a>`, pasteEvent());

    expect(linkRuns(bound)).toEqual([{ text: "Roadmap", href: DOC }]);
  });

  it("links selected text to a doc, as the toolbar's doc picker does", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see the plan</p>");
    placeSelection(bound, 5, 13); // "the plan"
    const linked = bound.editor.chain().extendMarkRange("link").setLink({ href: DOC }).run();

    expect(linked).toBe(true);
    expect(linkRuns(bound)).toEqual([
      { text: "see ", href: null },
      { text: "the plan", href: DOC },
    ]);
  });

  it.each([
    "javascript:alert(1)",
    "knot://elsewhere/1",
    "knot:doc/1",
    "KNOT://doc/1",
    " knot://doc/1",
  ])("still refuses %s", (href) => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>text</p>");
    placeSelection(bound, 1, 5);

    expect(bound.editor.commands.setLink({ href })).toBe(false);
  });

  it("does not autolink typed knot:// text", () => {
    bound = mountBoundEditor();
    typeText(bound.editor.view, `see ${DOC} `);

    expect(linkRuns(bound)).toEqual([{ text: `see ${DOC} `, href: null }]);
  });
});

describe("clicking internal links", () => {
  let editor: Editor | null = null;

  afterEach(() => {
    editor?.destroy();
    editor = null;
  });

  /** mountBoundEditor has no `navigate`, which the doc-link click needs. */
  function mount(navigate: (to: string) => void): Editor {
    const doc = new Y.Doc();
    return new Editor({
      element: document.createElement("div"),
      extensions: createExtensions({
        doc,
        awareness: new Awareness(doc),
        user: { name: "t", color: "#336699" },
        navigate: navigate as never,
      }),
    });
  }

  it("navigates to the page a doc link points at", () => {
    const navigate = vi.fn();
    editor = mount(navigate);
    editor.commands.setContent({
      type: "doc",
      content: [
        {
          type: "paragraph",
          content: [{ type: "text", text: "Roadmap", marks: [{ type: "link", attrs: { href: DOC } }] }],
        },
      ],
    });
    // The decoration wraps the text in a span inside the <a>; that span is
    // what a click lands on.
    const target = editor.view.dom.querySelector("a")?.firstElementChild ?? editor.view.dom;

    expect(clickOn(editor.view, target, 3)).toBe(true);
    expect(navigate).toHaveBeenCalledWith(`/doc/${DOC.slice("knot://doc/".length)}`);
  });

  it("does not navigate for a doc link whose id is not a page id", () => {
    // Pasted HTML can carry anything after the prefix.
    const navigate = vi.fn();
    editor = mount(navigate);
    editor.commands.setContent(linkParagraph("Roadmap", `${DOC}/permissions`));
    const target = editor.view.dom.querySelector("a")?.firstElementChild ?? editor.view.dom;

    expect(clickOn(editor.view, target, 3)).toBe(false);
    expect(navigate).not.toHaveBeenCalled();
  });

  it("opens the picker of a clicked date chip", () => {
    editor = mount(vi.fn());
    editor.commands.setContent(linkParagraph("Mon 2 Nov", TIME));
    const target = editor.view.dom.querySelector("a")?.firstElementChild ?? editor.view.dom;

    expect(clickOn(editor.view, target, 3)).toBe(true);
    expect(editor.storage.knotDateTime.popup?.isOpen()).toBe(true);
  });

  it("leaves a date chip alone in read mode, where the picker would write to the doc", () => {
    editor = mount(vi.fn());
    editor.commands.setContent(linkParagraph("Mon 2 Nov", TIME));
    editor.setEditable(false);
    const target = editor.view.dom.querySelector("a")?.firstElementChild ?? editor.view.dom;

    expect(clickOn(editor.view, target, 3)).toBe(false);
    expect(editor.storage.knotDateTime.popup?.isOpen()).toBe(false);
  });

  it.each([
    ["middle", { button: 1 }],
    ["shift", { shiftKey: true }],
    ["alt", { altKey: true }],
  ])("leaves a date chip alone on a %s click", (_kind, init) => {
    editor = mount(vi.fn());
    editor.commands.setContent(linkParagraph("Mon 2 Nov", TIME));
    const target = editor.view.dom.querySelector("a")?.firstElementChild ?? editor.view.dom;

    expect(clickOn(editor.view, target, 3, init)).toBe(false);
    expect(editor.storage.knotDateTime.popup?.isOpen()).toBe(false);
  });
});

function linkParagraph(text: string, href: string) {
  return {
    type: "doc",
    content: [
      { type: "paragraph", content: [{ type: "text", text, marks: [{ type: "link", attrs: { href } }] }] },
    ],
  };
}
