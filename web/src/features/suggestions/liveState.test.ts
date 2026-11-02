/**
 * How a pending suggestion's card reads the page right now: the text is the
 * same as when it was suggested, it changed, or it is gone.
 *
 * The server makes the same decision again on accept; this is what the card
 * shows and what the owner's "seen text" is taken from.
 */

import { afterEach, describe, expect, it } from "vitest";
import * as Y from "yjs";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { relativePositionFromBase64, relativePositionToBase64 } from "../comments/anchor";
import { encodeSuggestionAnchors } from "./anchor";
import { classifySuggestion, type ClassifiableSuggestion } from "./liveState";

describe("classifySuggestion", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function replaceOf(b: BoundEditor, from: number, to: number): ClassifiableSuggestion {
    const anchors = encodeSuggestionAnchors(b.editor, b.ydoc, from, to);
    expect(anchors).not.toBeNull();
    return {
      kind: "replace",
      position_y: anchors!.start,
      position_y_end: anchors!.end,
      original_text: b.editor.state.doc.textBetween(from, to),
    };
  }

  it("classify_same_changed_removed", () => {
    bound = mountBoundEditor();
    const b = bound;
    b.editor.commands.insertContent("Hello brave world");
    const s = replaceOf(b, 7, 12);
    expect(s.original_text).toBe("brave");

    expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "same", currentText: "brave", from: 7 });

    // Someone deletes the "a" in the middle of the word.
    b.editor.view.dispatch(b.editor.state.tr.delete(9, 10));
    expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "changed", currentText: "brve", from: 7 });

    // Then the whole word.
    b.editor.view.dispatch(b.editor.state.tr.delete(7, 11));
    expect(b.editor.state.doc.textContent).toBe("Hello  world");
    expect(classifySuggestion(b.editor, b.ydoc, s).state).toBe("removed");
  });

  it("a delete suggestion classifies like a replace", () => {
    bound = mountBoundEditor();
    const b = bound;
    b.editor.commands.insertContent("Hello brave world");
    const s = { ...replaceOf(b, 7, 12), kind: "delete" as const };
    expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "same", currentText: "brave", from: 7 });
  });

  function insertAt(b: BoundEditor, pos: number): ClassifiableSuggestion {
    const anchors = encodeSuggestionAnchors(b.editor, b.ydoc, pos, pos);
    expect(anchors).not.toBeNull();
    return { kind: "insert", position_y: anchors!.start, position_y_end: anchors!.end, original_text: "" };
  }

  function twoParagraphs(b: BoundEditor): void {
    b.editor.commands.setContent([
      { type: "paragraph", content: [{ type: "text", text: "First" }] },
      { type: "paragraph", content: [{ type: "text", text: "Second" }] },
    ] as never);
    // "First" is 1..6; the second paragraph spans 7..15, its text 8..14.
    expect(b.editor.state.doc.textBetween(8, 14)).toBe("Second");
  }

  /**
   * What Yjs itself makes of the anchor, before knot's decoder: null once the
   * anchored items were garbage-collected, the deleted type when they were
   * kept. Pins which of the two paths a case below takes.
   */
  function yjsResolves(b: BoundEditor, s: ClassifiableSuggestion): boolean {
    return Y.createAbsolutePositionFromRelativePosition(relativePositionFromBase64(s.position_y), b.ydoc) !== null;
  }

  // Deleted items are garbage-collected unless an UndoManager tracks the
  // transaction that deleted them. The editor's yUndoPlugin tracks the owner's
  // own edits, so for those Yjs still resolves an anchor into the deleted
  // text. Both paths must classify the same way, so each case runs twice.
  for (const keep of [false, true]) {
    const mode = keep ? "kept items" : "collected items";

    it(`classify_insert_in_deleted_paragraph_is_removed (${mode})`, () => {
      bound = mountBoundEditor();
      const b = bound;
      twoParagraphs(b);

      const s = insertAt(b, 11);
      expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "same", currentText: "", from: 11 });

      // Delete the whole second paragraph: through the editor (tracked by its
      // yUndoPlugin, so kept), or by an untracked transaction (collected).
      if (keep) {
        b.editor.view.dispatch(b.editor.state.tr.delete(7, 15));
      } else {
        b.fragment.delete(1, 1);
      }
      expect(b.editor.state.doc.childCount).toBe(1);
      expect(yjsResolves(b, s)).toBe(keep);

      // The decoder refuses an anchor inside a deleted block, as the server's
      // is_deleted + in_live_page check does, so there is no position.
      expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "removed", currentText: "", from: null });
    });

    it(`an insert whose text node was swapped out is removed (${mode})`, () => {
      bound = mountBoundEditor();
      const b = bound;
      b.editor.commands.insertContent("Hello brave world");
      if (keep) new Y.UndoManager(b.fragment);

      const s = insertAt(b, 7);
      expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "same", currentText: "", from: 7 });

      // Replace the paragraph's XmlText with a new one holding the same text,
      // in one transaction: the paragraph survives, the anchored text does not.
      const para = b.fragment.get(0) as Y.XmlElement;
      b.ydoc.transact(() => {
        para.delete(0, 1);
        para.insert(0, [new Y.XmlText("Hello brave world")]);
      });
      expect(b.editor.state.doc.textContent).toBe("Hello brave world");
      expect(yjsResolves(b, s)).toBe(keep);

      expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "removed", currentText: "", from: null });
    });
  }

  it("an anchor that resolves between blocks is removed", () => {
    bound = mountBoundEditor();
    const b = bound;
    twoParagraphs(b);
    // Tied to the second paragraph element rather than to text: it decodes to
    // position 7, between the two blocks, which no suggestion can apply at.
    const between = relativePositionToBase64(Y.createRelativePositionFromTypeIndex(b.fragment, 1, 0));
    const s: ClassifiableSuggestion = { kind: "insert", position_y: between, position_y_end: between, original_text: "" };
    expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "removed", currentText: "", from: 7 });
  });

  it("an insert at the start of the page stays same", () => {
    bound = mountBoundEditor();
    const b = bound;
    b.editor.commands.insertContent("Hello");
    const anchors = encodeSuggestionAnchors(b.editor, b.ydoc, 1, 1);
    const s: ClassifiableSuggestion = {
      kind: "insert",
      position_y: anchors!.start,
      position_y_end: anchors!.end,
      original_text: "",
    };
    expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "same", currentText: "", from: 1 });
  });

  it("an undecodable anchor is removed with no position", () => {
    bound = mountBoundEditor();
    const b = bound;
    b.editor.commands.insertContent("Hello");
    const s: ClassifiableSuggestion = { kind: "replace", position_y: "", position_y_end: "", original_text: "Hello" };
    expect(classifySuggestion(b.editor, b.ydoc, s)).toEqual({ state: "removed", currentText: "", from: null });
  });
});
