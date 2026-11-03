/**
 * Suggestion anchors against a live ySync binding.
 *
 * Suggestions cannot reuse the comment encoder: it ties every position to the
 * character before it (assoc -1), so text typed just before a selection would
 * fall inside the range and be deleted on accept. These tests pin the
 * suggestion-specific encoding, byte for byte against the Rust resolver's
 * fixture, and the decoder's behaviour at the page's very first character.
 */

import { afterEach, describe, expect, it } from "vitest";
import * as Y from "yjs";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { relativePositionToBase64 } from "../comments/anchor";
import { decodeSuggestionRange, encodeSuggestionAnchors } from "./anchor";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function hexToBase64(hex: string): string {
  const bytes = hex.match(/../g)!.map((pair) => parseInt(pair, 16));
  return btoa(String.fromCharCode(...bytes));
}

function toHex(bytes: Uint8Array): string {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/**
 * The Yjs update the Rust resolver tests decode (Task 1,
 * `decodes_yjs_made_fixtures`): clientID 7, paragraph > XmlText "äb€cd",
 * bold on "b€".
 */
const RUST_FIXTURE_UPDATE_HEX =
  "0107070007010764656661756c74030970617261677261706807000700060400070102c3a48407020462e282ac840704026364c60702070304626f6c64027b7dc60704070504626f6c64046e756c6c00";

describe("suggestion anchors", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  /** Mount, type `text` into the empty first paragraph, return the fixture. */
  function withText(text: string): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.insertContent(text);
    return bound;
  }

  it("encodes_bytes_identical_to_rust_fixture", () => {
    bound = mountBoundEditor();
    const { editor, ydoc } = bound;
    // Before any write, so every item in the update carries client 7.
    ydoc.clientID = 7;

    // Two steps, not one insert with marks: inserting marked text in one step
    // lays the clocks out differently, and the fixture pins this layout.
    editor.commands.insertContent("äb€cd");
    editor.chain().setTextSelection({ from: 2, to: 4 }).toggleBold().run();

    // "€c": the paragraph opens at 0, so "ä" is 1, "b" 2, "€" 3, "c" 4.
    expect(editor.state.doc.textBetween(3, 5)).toBe("€c");
    const anchors = encodeSuggestionAnchors(editor, ydoc, 3, 5);

    expect(anchors).toEqual({
      // item (7, 4) = "€", assoc 0
      start: hexToBase64("00070400"),
      // item (7, 5) = "c", assoc -1
      end: hexToBase64("00070541"),
    });
    expect(toHex(Y.encodeStateAsUpdate(ydoc))).toBe(RUST_FIXTURE_UPDATE_HEX);
  });

  it("encodes an insertion point as two equal anchors tied to the char before", () => {
    const { editor, ydoc } = withText("Hello world");

    const anchors = encodeSuggestionAnchors(editor, ydoc, 6, 6);

    expect(anchors).not.toBeNull();
    expect(anchors!.start).toBe(anchors!.end);
    expect(decodeSuggestionRange(editor, ydoc, anchors!.start, anchors!.end)).toEqual({
      from: 6,
      to: 6,
      anchorLost: false,
    });

    // Text typed right after the point pushes nothing in front of it.
    editor.view.dispatch(editor.state.tr.insertText("!!", 6));
    expect(decodeSuggestionRange(editor, ydoc, anchors!.start, anchors!.end)).toEqual({
      from: 6,
      to: 6,
      anchorLost: false,
    });
  });

  it("edge_typing_stays_outside_range", () => {
    const { editor, ydoc } = withText("Hello brave world");
    expect(editor.state.doc.textBetween(7, 12)).toBe("brave");

    const anchors = encodeSuggestionAnchors(editor, ydoc, 7, 12);
    expect(anchors).not.toBeNull();

    // Right after the last selected character, then right before the first
    // one. The end first, so the start position is still the original one.
    editor.view.dispatch(editor.state.tr.insertText("YY", 12));
    editor.view.dispatch(editor.state.tr.insertText("XX", 7));
    expect(editor.state.doc.textContent).toBe("Hello XXbraveYY world");

    const range = decodeSuggestionRange(editor, ydoc, anchors!.start, anchors!.end);
    expect(range).not.toBeNull();
    expect(editor.state.doc.textBetween(range!.from, range!.to)).toBe("brave");
  });

  it("decodes_start_anchor_on_first_character_of_page", () => {
    // y-tiptap's decoder treats any item anchor that resolves to position 1
    // as misresolved and returns null; a suggestion on the page's first word
    // would then always show as "removed".
    const { editor, ydoc } = withText("Hello world");

    const anchors = encodeSuggestionAnchors(editor, ydoc, 1, 6);
    expect(anchors).not.toBeNull();

    const range = decodeSuggestionRange(editor, ydoc, anchors!.start, anchors!.end);
    expect(range).toEqual({ from: 1, to: 6, anchorLost: false });
    expect(editor.state.doc.textBetween(range!.from, range!.to)).toBe("Hello");
  });

  it("follows the anchored text when earlier text is inserted", () => {
    const { editor, ydoc } = withText("Hello world");
    const anchors = encodeSuggestionAnchors(editor, ydoc, 7, 12);

    editor.view.dispatch(editor.state.tr.insertText("Once again: ", 1));

    const range = decodeSuggestionRange(editor, ydoc, anchors!.start, anchors!.end);
    expect(range).toEqual({ from: 19, to: 24, anchorLost: false });
    expect(editor.state.doc.textBetween(range!.from, range!.to)).toBe("world");
  });

  describe("an insertion point next to a line break", () => {
    // A hard_break is an XmlElement between two XmlTexts. A caret with no
    // text directly on either side would anchor to that element or to the
    // paragraph, which the server's resolve_range always refuses as lost.
    const br = { type: "hard_break" };
    /** What the encoder used to send for each caret: tied to the break at
     *  the caret, or to the end of the paragraph. */
    type Legacy = (para: Y.XmlElement) => Y.RelativePosition;
    const shapes: { name: string; content: unknown[]; caret: number; legacy: Legacy }[] = [
      {
        // "abc" 1..4, breaks at 4 and 5, "def" 6..9.
        name: "on the empty line between two breaks",
        content: [{ type: "text", text: "abc" }, br, br, { type: "text", text: "def" }],
        caret: 5,
        legacy: (para) => Y.createRelativePositionFromTypeIndex(para, 2, 0),
      },
      {
        // "abc" 1..4, break at 4; the paragraph's content ends at 5.
        name: "after a break at the end of the paragraph",
        content: [{ type: "text", text: "abc" }, br],
        caret: 5,
        legacy: (para) => new Y.RelativePosition((para as unknown as { _item: Y.Item })._item.id, null, null),
      },
      {
        // Break at 1, "abc" 2..5.
        name: "before a break at the start of the paragraph",
        content: [br, { type: "text", text: "abc" }],
        caret: 1,
        legacy: (para) => Y.createRelativePositionFromTypeIndex(para, 0, 0),
      },
    ];

    for (const { name, content, caret, legacy } of shapes) {
      it(`is refused ${name}`, () => {
        bound = mountBoundEditor();
        const { editor, ydoc } = bound;
        editor.commands.setContent([{ type: "paragraph", content }] as never);

        expect(encodeSuggestionAnchors(editor, ydoc, caret, caret)).toBeNull();
      });

      it(`an existing anchor ${name} does not decode`, () => {
        bound = mountBoundEditor();
        const { editor, ydoc, fragment } = bound;
        editor.commands.setContent([{ type: "paragraph", content }] as never);
        const para = fragment.get(0) as Y.XmlElement;
        const rel = legacy(para);
        const b64 = relativePositionToBase64(rel);
        expect(Y.createAbsolutePositionFromRelativePosition(rel, ydoc)?.type).toBe(para);

        expect(decodeSuggestionRange(editor, ydoc, b64, b64)).toBeNull();
      });
    }

    it("still encodes a caret right after a break, before text", () => {
      bound = mountBoundEditor();
      const { editor, ydoc } = bound;
      editor.commands.setContent([
        { type: "paragraph", content: [{ type: "text", text: "abc" }, br, { type: "text", text: "def" }] },
      ] as never);
      // "abc" 1..4, break at 4, "def" 5..8.
      const anchors = encodeSuggestionAnchors(editor, ydoc, 5, 5);
      expect(anchors).not.toBeNull();
      expect(decodeSuggestionRange(editor, ydoc, anchors!.start, anchors!.end)).toEqual({
        from: 5,
        to: 5,
        anchorLost: false,
      });
    });
  });

  it("returns null when the ySync mapping is missing", () => {
    const ydoc = new Y.Doc();
    const fakeEditor = { state: {} } as unknown as BoundEditor["editor"];
    expect(encodeSuggestionAnchors(fakeEditor, ydoc, 1, 2)).toBeNull();
    expect(decodeSuggestionRange(fakeEditor, ydoc, "AAAA", "AAAA")).toBeNull();
  });

  it("returns null for garbage anchors", () => {
    const { editor, ydoc } = withText("Hello world");
    expect(decodeSuggestionRange(editor, ydoc, "not base64!", "AAAA")).toBeNull();
  });
});
