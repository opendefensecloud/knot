/**
 * A link ends where its text ends.
 *
 * Pasting a URL left the caret at the link's right edge, and everything typed
 * from there joined the link: paste `https://example.com`, type " and more",
 * and the whole run became one link whose href was still just the URL. The
 * cause was Tiptap making the link mark inclusive whenever autolink is on, so
 * it was not specific to paste — any caret at a link's edge (after a doc
 * link, an HTML paste, a click at the end of a link, the start of a
 * paragraph that opens with one) extended it. See KnotLink in extensions.ts.
 *
 * A pasted `https://host` also opened the `//` date picker, which took the
 * keyboard focus, so the typing never reached the document at all.
 */

import { afterEach, describe, expect, it } from "vitest";
import type { EditorView } from "@tiptap/pm/view";

import { fragmentXml, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import {
  linkRuns,
  pasteEvent,
  placeSelection,
  typeText,
} from "../../test/editorInput";

/** The date picker positions itself at the caret, and jsdom has no layout. */
function stubCaretCoords(view: EditorView) {
  view.coordsAtPos = () => ({ left: 0, right: 0, top: 0, bottom: 0 });
}

function datePickerIsOpen(bound: BoundEditor): boolean {
  return bound.editor.storage.knotDateTime.popup?.isOpen() ?? false;
}

function dateInputHasFocus(): boolean {
  const focused = document.activeElement;
  return focused instanceof HTMLInputElement && focused.type === "date";
}

describe("text after a pasted link", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("stays plain when typed straight after a pasted URL", () => {
    bound = mountBoundEditor();
    bound.editor.view.pasteText("https://example.com/docs", pasteEvent());
    typeText(bound.editor.view, " and more");

    expect(linkRuns(bound)).toEqual([
      { text: "https://example.com/docs", href: "https://example.com/docs" },
      { text: " and more", href: null },
    ]);
  });

  it("stays plain when it is punctuation with no space before it", () => {
    // Tiptap 3.31.4's own fix only covers a typed space; this is the case it
    // leaves open.
    bound = mountBoundEditor();
    bound.editor.view.pasteText("https://example.com/docs", pasteEvent());
    typeText(bound.editor.view, ".");

    expect(linkRuns(bound)).toEqual([
      { text: "https://example.com/docs", href: "https://example.com/docs" },
      { text: ".", href: null },
    ]);
  });

  it("stays plain after a URL pasted at the end of a sentence", () => {
    bound = mountBoundEditor();
    bound.editor.view.pasteText("see www.example.com", pasteEvent());
    typeText(bound.editor.view, " ok");

    expect(linkRuns(bound)).toEqual([
      { text: "see ", href: null },
      { text: "www.example.com", href: "http://www.example.com" },
      { text: " ok", href: null },
    ]);
  });

  it("stays plain after a pasted HTML anchor", () => {
    bound = mountBoundEditor();
    bound.editor.view.pasteHTML('<a href="https://example.com">Example</a>', pasteEvent());
    typeText(bound.editor.view, " more");

    expect(linkRuns(bound)).toEqual([
      { text: "Example", href: "https://example.com" },
      { text: " more", href: null },
    ]);
  });

  it("stays plain in the Y.Doc, which is what the server stores and exports", () => {
    bound = mountBoundEditor();
    bound.editor.view.pasteText("https://example.com/docs", pasteEvent());
    typeText(bound.editor.view, " and more");

    expect(fragmentXml(bound)).toMatch(/>https:\/\/example\.com\/docs<\/link> and more</);
  });

  it("leaves the caret after a URL pasted over selected text", () => {
    // Upstream links the selection and leaves it selected, so the next
    // keystroke replaced the new link outright.
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>read the docs</p>");
    placeSelection(bound, 6, 14); // "the docs"
    bound.editor.view.pasteText("https://example.com/docs", pasteEvent());

    expect(bound.editor.state.selection.empty).toBe(true);
    typeText(bound.editor.view, " now");
    expect(linkRuns(bound)).toEqual([
      { text: "read ", href: null },
      { text: "the docs", href: "https://example.com/docs" },
      { text: " now", href: null },
    ]);
  });

  it("leaves the caret in text after a URL pasted over a select-all", () => {
    // The end of an AllSelection is not inside a textblock; a caret put
    // there made the next keystroke open a new paragraph.
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>hello</p><p>world</p>");
    bound.editor.commands.selectAll();
    bound.editor.view.pasteText("https://example.com/docs", pasteEvent());

    expect(bound.editor.state.selection.$from.parent.inlineContent).toBe(true);
    typeText(bound.editor.view, "!");
    expect(bound.editor.state.doc.childCount).toBe(2);
    expect(bound.editor.state.doc.lastChild?.textContent).toBe("world!");
  });
});

describe("the // date picker after a URL scheme", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function mountWithLayoutStub(): BoundEditor {
    const mounted = mountBoundEditor();
    stubCaretCoords(mounted.editor.view);
    mounted.editor.view.dom.focus();
    return mounted;
  }

  it("still opens for a // typed after a space", () => {
    // The control for the tests below: the picker can open (and take focus)
    // in this setup, so its staying shut there means something.
    bound = mountWithLayoutStub();
    typeText(bound.editor.view, "due //");

    expect(datePickerIsOpen(bound)).toBe(true);
    expect(dateInputHasFocus()).toBe(true);
  });

  it("stays shut after a pasted https:// origin, so typing reaches the document", () => {
    bound = mountWithLayoutStub();
    bound.editor.view.pasteText("https://example.com", pasteEvent());

    expect(datePickerIsOpen(bound)).toBe(false);
    expect(dateInputHasFocus()).toBe(false);
    typeText(bound.editor.view, " and more");
    expect(linkRuns(bound)).toEqual([
      { text: "https://example.com", href: "https://example.com" },
      { text: " and more", href: null },
    ]);
  });

  it.each(["https://", "file:///"])("stays shut while typing %s", (scheme) => {
    bound = mountWithLayoutStub();
    typeText(bound.editor.view, `see ${scheme}`);

    expect(datePickerIsOpen(bound)).toBe(false);
  });
});

describe("typing at the edge of an existing link", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("does not extend a link from its end", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<p>see <a href="https://example.com">docs</a> here</p>');
    placeSelection(bound, 9); // after "docs"
    typeText(bound.editor.view, "!");

    expect(linkRuns(bound)).toEqual([
      { text: "see ", href: null },
      { text: "docs", href: "https://example.com" },
      { text: "! here", href: null },
    ]);
  });

  it("does not extend a link that opens the paragraph from its start", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<p><a href="https://example.com">docs</a> here</p>');
    placeSelection(bound, 1);
    typeText(bound.editor.view, "x");

    expect(linkRuns(bound)).toEqual([
      { text: "x", href: null },
      { text: "docs", href: "https://example.com" },
      { text: " here", href: null },
    ]);
  });

  it("still edits a link's text from inside it", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<p><a href="https://example.com">docs</a></p>');
    placeSelection(bound, 3); // between "do" and "cs"
    typeText(bound.editor.view, "X");

    expect(linkRuns(bound)).toEqual([{ text: "doXcs", href: "https://example.com" }]);
  });

  it("does not extend a doc link inserted from the toolbar", () => {
    // The toolbar's doc branch inserts the title as linked text and leaves
    // the caret at its end.
    bound = mountBoundEditor();
    bound.editor
      .chain()
      .insertContent({
        type: "text",
        text: "Roadmap",
        marks: [{ type: "link", attrs: { href: "knot://doc/42" } }],
      })
      .run();
    typeText(bound.editor.view, " is next");

    expect(linkRuns(bound)).toEqual([
      { text: "Roadmap", href: "knot://doc/42" },
      { text: " is next", href: null },
    ]);
  });

  it("does not write to the doc on ArrowRight at the end of a paragraph-ending link", () => {
    // Upstream's `exitable` handler typed a space into the shared doc here.
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<p>see <a href="https://example.com">docs</a></p>');
    placeSelection(bound, 9);
    const fragmentBefore = fragmentXml(bound);
    bound.editor.commands.keyboardShortcut("ArrowRight");

    expect(fragmentXml(bound)).toBe(fragmentBefore);
  });

  it("keeps the link mark non-inclusive with autolink still on", () => {
    bound = mountBoundEditor();
    const linkExtension = bound.editor.extensionManager.extensions.find(
      (extension) => extension.name === "link",
    );

    expect(bound.editor.schema.marks.link?.spec.inclusive).toBe(false);
    expect((linkExtension?.options as { autolink?: boolean } | undefined)?.autolink).toBe(true);
  });
});
