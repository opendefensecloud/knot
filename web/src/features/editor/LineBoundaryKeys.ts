import { Extension } from "@tiptap/core";
import { TextSelection } from "@tiptap/pm/state";
import type { EditorView } from "@tiptap/pm/view";

import { isApplePlatform } from "../../lib/platform";
import { domSelectionIn } from "./readingCaret";

type Alter = "move" | "extend";
type Direction = "forward" | "backward";

/**
 * End/Home (and Shift+End/Home) move to the end/start of the line while
 * editing on a Mac (#86).
 *
 * macOS binds End and Home to scrollToEndOfDocument/scrollToBeginningOfDocument,
 * and Chrome, Safari and Firefox follow it even with a caret in a
 * contenteditable: End threw the reader to the bottom of the page and left
 * the caret where it was. Elsewhere the browsers already move within the
 * line, so this does nothing there. ProseMirror and Tiptap bind neither key.
 *
 * Selection.modify(…, "lineboundary") is how CodeMirror's standard keymap
 * implements End/Home: the browser finds the visual line's edge, so a wrapped
 * paragraph goes to the end of the line on screen, not of the paragraph. It
 * moves the DOM selection only, and the caret is read straight back into the
 * editor state rather than left for the "selectionchange" that follows: that
 * event is async, and any transaction in between (a remote Yjs update, a
 * collaborator's cursor) would write ProseMirror's old selection back over
 * the DOM and lose the keypress. The read-back also scrolls the caret into
 * view, which ProseMirror does for a key it leaves to the browser but not
 * for one a handler took — the end of a long line in a code block scrolls
 * sideways. Writing the same position back leaves the DOM selection alone
 * (ProseMirror compares equivalent DOM positions first), so the browser
 * keeps drawing the caret at the end of the wrapped line, not at the start
 * of the next. Returning true has ProseMirror preventDefault the key, which
 * stops the scroll.
 *
 * Only for a text selection: on a selected node (image, board, table cells)
 * there is no line to move along, and the key keeps its default. View mode
 * never gets here — ProseMirror runs key handlers only while editable — so
 * End there still scrolls the page, as a reader expects.
 */
export const LineBoundaryKeys = Extension.create({
  name: "lineBoundaryKeys",

  addKeyboardShortcuts() {
    return {
      End: ({ editor }) => moveToLineBoundary(editor.view, "move", "forward"),
      "Shift-End": ({ editor }) => moveToLineBoundary(editor.view, "extend", "forward"),
      Home: ({ editor }) => moveToLineBoundary(editor.view, "move", "backward"),
      "Shift-Home": ({ editor }) => moveToLineBoundary(editor.view, "extend", "backward"),
    };
  },
});

function moveToLineBoundary(view: EditorView, alter: Alter, direction: Direction): boolean {
  // At call time, not at load: the platform cannot change under a page, but
  // tests switch it.
  if (!isApplePlatform()) return false;
  if (!(view.state.selection instanceof TextSelection)) return false;
  // Not in every engine's typings history, and missing in jsdom: without it
  // the key keeps its default rather than doing nothing.
  const domSelection = view.dom.ownerDocument.getSelection() as
    | (Omit<Selection, "modify"> & { modify?: Selection["modify"] })
    | null;
  if (!domSelection || typeof domSelection.modify !== "function") return false;
  domSelection.modify(alter, direction, "lineboundary");
  const movedSelection = domSelectionIn(view);
  // Unmappable (inside a node view's own DOM): "selectionchange" still
  // carries the move, as it would for any native caret movement.
  if (movedSelection) view.dispatch(view.state.tr.setSelection(movedSelection).scrollIntoView());
  return true;
}
