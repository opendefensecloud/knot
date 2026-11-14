/**
 * End/Home on a Mac move to the line's end/start while editing (#86).
 *
 * macOS binds End and Home to "scroll to end/beginning of document", and
 * browsers follow it even with a caret in a contenteditable, so End threw the
 * reader to the bottom of the page. LineBoundaryKeys moves the DOM selection
 * to the visual line boundary instead, as CodeMirror does. jsdom has no
 * layout and no Selection.modify, so the tests stand one in and check what
 * the extension asks of it; the browser does the line measuring.
 */

import { NodeSelection } from "@tiptap/pm/state";
import { afterEach, describe, expect, it, vi } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { restorePlatform, stubPlatform } from "../../test/platform";

describe("LineBoundaryKeys", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
    restorePlatform();
    delete (document.getSelection() as { modify?: unknown } | null)?.modify;
  });

  function withParagraph(): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>hello world</p>");
    placeSelection(bound, 6);
    return bound;
  }

  /** Give the document's Selection a modify() to watch, as browsers have. */
  function stubModify() {
    const modify = vi.fn();
    Object.defineProperty(document.getSelection()!, "modify", { value: modify, configurable: true });
    return modify;
  }

  function press(b: BoundEditor, key: string, shiftKey = false): boolean {
    const event = new KeyboardEvent("keydown", { key, code: key, shiftKey });
    return b.editor.view.someProp("handleKeyDown", (fn) => fn(b.editor.view, event)) ?? false;
  }

  it.each([
    ["End", false, "move", "forward"],
    ["End", true, "extend", "forward"],
    ["Home", false, "move", "backward"],
    ["Home", true, "extend", "backward"],
  ] as const)("on a Mac, %s (shift: %s) asks the browser to %s %s to the line boundary", (key, shift, alter, direction) => {
    stubPlatform("MacIntel");
    const b = withParagraph();
    const modify = stubModify();

    expect(press(b, key, shift)).toBe(true);
    expect(modify).toHaveBeenCalledExactlyOnceWith(alter, direction, "lineboundary");
  });

  /**
   * A modify() that does what a browser would on a one-line paragraph: put
   * the caret (or the selection's focus) at the line's end or start. Also
   * takes over ProseMirror's scroll-to-selection, which jsdom cannot measure,
   * and returns it to watch.
   */
  function stubLineBoundaryModify(b: BoundEditor) {
    const scrollToSelection = vi.fn(() => true);
    b.editor.view.setProps({ handleScrollToSelection: scrollToSelection });
    const text = b.editor.view.dom.querySelector("p")!.firstChild as Text;
    const modify = vi.fn((alter: string, direction: string) => {
      const domSelection = document.getSelection()!;
      const offset = direction === "forward" ? text.length : 0;
      // From the editor's caret (pos 6 = "hello|"), whatever jsdom's own
      // selection was: it has no focus to keep the two in step.
      if (alter === "extend") domSelection.setBaseAndExtent(text, 5, text, offset);
      else domSelection.collapse(text, offset);
    });
    Object.defineProperty(document.getSelection()!, "modify", { value: modify, configurable: true });
    return { scrollToSelection };
  }

  it("hands the moved caret to the editor in the same keypress", () => {
    // Not on the next "selectionchange": a transaction in between (a remote
    // edit, a collaborator's cursor) would write the old caret back.
    stubPlatform("MacIntel");
    const b = withParagraph();
    stubLineBoundaryModify(b);

    expect(press(b, "End")).toBe(true);
    expect(b.editor.state.selection.empty).toBe(true);
    expect(b.editor.state.selection.head).toBe(12); // "hello world|"

    expect(press(b, "Home")).toBe(true);
    expect(b.editor.state.selection.head).toBe(1); // "|hello world"
  });

  it("extends the selection with Shift+End", () => {
    stubPlatform("MacIntel");
    const b = withParagraph();
    stubLineBoundaryModify(b);

    expect(press(b, "End", true)).toBe(true);
    expect(b.editor.state.selection.anchor).toBe(6);
    expect(b.editor.state.selection.head).toBe(12);
  });

  it("scrolls the moved caret into view, as the browser's own End would", () => {
    // A long unwrapped line in a code block scrolls sideways: the caret at
    // its end must not be left out of sight.
    stubPlatform("MacIntel");
    const b = withParagraph();
    const { scrollToSelection } = stubLineBoundaryModify(b);

    press(b, "End");
    expect(scrollToSelection).toHaveBeenCalledOnce();
  });

  it("leaves End to the browser elsewhere, where it already moves within the line", () => {
    stubPlatform("Linux x86_64");
    const b = withParagraph();
    const modify = stubModify();

    expect(press(b, "End")).toBe(false);
    expect(modify).not.toHaveBeenCalled();
  });

  it("leaves End alone without Selection.modify", () => {
    stubPlatform("MacIntel");
    const b = withParagraph();

    expect(press(b, "End")).toBe(false);
  });

  it("leaves End alone on a selected node (an image, a board)", () => {
    stubPlatform("MacIntel");
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>before</p><hr><p>after</p>");
    const { state } = bound.editor;
    // The horizontal rule sits right after the first paragraph (8 = 1 + 6 + 1).
    bound.editor.view.dispatch(state.tr.setSelection(NodeSelection.create(state.doc, 8)));
    const modify = stubModify();

    expect(press(bound, "End")).toBe(false);
    expect(modify).not.toHaveBeenCalled();
  });
});
