import { Selection, TextSelection } from "@tiptap/pm/state";
import type { EditorView } from "@tiptap/pm/view";

import { firstIndexBelow, stickyCoverBottom } from "../docs/readingAnchor";

/** How far into a line the reading-line probe lands, in px. */
const PROBE_INSET = 16;
/** Probe spacing, below the smallest line height, and how far down to look. */
const PROBE_STEP = 6;
const PROBE_DEPTH = 160;

/** What DocPage may ask of the mounted editor. */
export type KnotEditorHandle = {
  placeCaretAtReadingLine(): void;
  /** Scroll the caret out from under the sticky band, if it is under it. */
  revealCaretBelowBand(): void;
};

/**
 * Keep the current selection only when it is text and on screen: a
 * NodeSelection (image, board) would be replaced by the first keystroke, and
 * a caret off-screen would make typing jump away from the reading point.
 */
export function chooseCaretStrategy(a: {
  isText: boolean;
  headTop: number;
  headBottom: number;
  visibleTop: number;
  visibleBottom: number;
}): "keep" | "reading-line" {
  return a.isText && a.headTop >= a.visibleTop - 1 && a.headBottom <= a.visibleBottom + 1
    ? "keep"
    : "reading-line";
}

/**
 * Focus the editor at the first line the reader can see below the sticky
 * band, so typing starts where they were reading. Changes the selection only:
 * no scrollIntoView, no document change.
 */
export function placeCaretAtReadingLine(view: EditorView, scope: ParentNode): void {
  const scroller = view.dom.closest("main");
  if (!scroller) return;
  const scrollerRect = scroller.getBoundingClientRect();
  const proseRect = view.dom.getBoundingClientRect();
  const visibleTop = Math.max(scrollerRect.top, stickyCoverBottom(scope) ?? -Infinity, proseRect.top);
  const visibleBottom = scrollerRect.bottom;

  // A click in read-only text moves the browser's selection but not the
  // editor's (the view is not focused), so prefer the DOM's: "click where you
  // want to edit, then ⌘E" keeps the caret at the click.
  const selection = domSelectionIn(view) ?? view.state.selection;
  const head = view.coordsAtPos(selection.head);
  const strategy = chooseCaretStrategy({
    isText: selection instanceof TextSelection && selection.$head.parent.inlineContent,
    headTop: head.top,
    headBottom: head.bottom,
    visibleTop,
    visibleBottom,
  });
  if (strategy === "keep") {
    if (!selection.eq(view.state.selection)) view.dispatch(view.state.tr.setSelection(selection));
    view.focus();
    return;
  }

  // Probe a few characters into the line, never at its very start: a
  // position at the start of a wrapped line is also the end of the line
  // above, and the browser draws the caret up there, under the band. Step
  // down past a line that pokes up under the band, and past the gaps
  // between blocks, until a line shows in full.
  let pos: number | null = null;
  for (let y = visibleTop + 4; y < Math.min(visibleBottom, visibleTop + PROBE_DEPTH); y += PROBE_STEP) {
    const hit = view.posAtCoords({ left: textLeftAt(view, proseRect, y) + PROBE_INSET, top: y });
    if (hit && view.coordsAtPos(hit.pos).top >= visibleTop - 1) {
      pos = hit.pos;
      break;
    }
  }
  if (pos === null) {
    const blocks = view.dom.children;
    const index = firstIndexBelow(blocks.length, (i) => blocks[i]!.getBoundingClientRect().bottom, visibleTop);
    if (index >= 0) pos = view.posAtDOM(blocks[index]!, 0);
  }
  if (pos === null) {
    view.focus();
    return;
  }

  const $pos = view.state.doc.resolve(Math.min(pos, view.state.doc.content.size));
  // textOnly: never land as a NodeSelection on a board, diagram or attachment.
  const next = Selection.findFrom($pos, 1, true) ?? Selection.findFrom($pos, -1, true);
  if (next) view.dispatch(view.state.tr.setSelection(next));
  view.focus();
}

/**
 * Left edge of the text at height `y`. The editor root is wider than its
 * text: wide mode centres blocks at the measure, and lists, quotes and table
 * cells indent theirs. A probe left of the text block hits the block's edge
 * and skips its wrapped lines, so start from the textblock under the
 * column's middle.
 */
function textLeftAt(view: EditorView, proseRect: DOMRect, y: number): number {
  const doc = view.dom.ownerDocument;
  if (typeof doc.elementFromPoint !== "function") return proseRect.left;
  const hit = doc.elementFromPoint(proseRect.left + proseRect.width / 2, y);
  const block = hit?.closest("p, h1, h2, h3, h4, h5, h6, pre, th, td");
  return block && view.dom.contains(block) ? block.getBoundingClientRect().left : proseRect.left;
}

/**
 * The toolbar can grow after the caret is placed: in a table it adds a row
 * of table buttons on its next render, and the caret's line ends up under
 * it. ProseMirror's own scrollIntoView knows nothing of sticky bands, so
 * scroll `<main>` by the overlap.
 */
export function revealCaretBelowBand(view: EditorView, scope: ParentNode): void {
  const scroller = view.dom.closest("main");
  const bandBottom = stickyCoverBottom(scope);
  if (!scroller || bandBottom === null) return;
  const caretTop = view.coordsAtPos(view.state.selection.head).top;
  if (caretTop < bandBottom) scroller.scrollTop -= bandBottom - caretTop + REVEAL_GAP;
}

/** Space left between the band and a revealed caret line, in px. */
const REVEAL_GAP = 4;

/** The browser's selection as an editor selection, when it lies in `view`. */
function domSelectionIn(view: EditorView): Selection | null {
  const domSelection = view.dom.ownerDocument.getSelection();
  const { anchorNode, focusNode } = domSelection ?? {};
  if (!domSelection || !anchorNode || !focusNode) return null;
  if (!view.dom.contains(anchorNode) || !view.dom.contains(focusNode)) return null;
  try {
    const anchor = view.posAtDOM(anchorNode, domSelection.anchorOffset);
    const head = view.posAtDOM(focusNode, domSelection.focusOffset);
    return TextSelection.create(view.state.doc, anchor, head);
  } catch {
    // A node view's internals can have no document position.
    return null;
  }
}
