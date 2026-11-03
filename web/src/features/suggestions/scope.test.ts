/**
 * What "Suggest change" may target: plain text inside one text block.
 *
 * The scope check runs before anything is sent, so a selection the server
 * could not apply safely (across blocks, over a line break or a chip) never
 * becomes a suggestion in the first place.
 */

import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { SCOPE_TOOLTIP, suggestionScope } from "./scope";

type Inline = { type: string; text?: string; marks?: { type: string; attrs?: Record<string, unknown> }[] };

const paragraph = (...content: Inline[]) => ({ type: "paragraph", content });
const text = (value: string): Inline => ({ type: "text", text: value });
const linked = (value: string, href: string): Inline => ({
  type: "text",
  text: value,
  marks: [{ type: "link", attrs: { href } }],
});

describe("suggestionScope", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function withContent(content: unknown): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(content as never);
    return bound;
  }

  it("scope_refuses_cross_block", () => {
    const b = withContent([paragraph(text("First")), paragraph(text("Second"))]);
    // Inside "First" (1..6) to inside "Second" (8..14).
    placeSelection(b, 3, 10);
    expect(suggestionScope(b.editor.state)).toEqual({ ok: false, reason: "cross_block" });
  });

  it("scope_refuses_hard_break", () => {
    const b = withContent([paragraph(text("ab"), { type: "hard_break" }, text("cd"))]);
    // "ab" 1..3, break at 3, "cd" 4..6.
    placeSelection(b, 2, 5);
    expect(suggestionScope(b.editor.state)).toEqual({ ok: false, reason: "inline_atom" });
  });

  it("refuses a selection over a line break in a code block", () => {
    // A code block keeps its lines as one text with "\n" in it, which the
    // server would replace as one plain range across lines.
    const b = withContent([{ type: "code_block", content: [text("one\ntwo")] }]);
    // "one" 1..4, "\n" at 4, "two" 5..8.
    placeSelection(b, 2, 6);
    expect(suggestionScope(b.editor.state)).toEqual({ ok: false, reason: "inline_atom" });

    // Within one line it is a plain replace.
    placeSelection(b, 5, 8);
    expect(suggestionScope(b.editor.state)).toMatchObject({ ok: true, kind: "replace", text: "two" });
  });

  it("scope_refuses_internal_link_marks", () => {
    for (const href of [
      "knot://doc/00000000-0000-0000-0000-000000000001",
      "knot://user/00000000-0000-0000-0000-000000000002",
      "knot://time/2026-11-02T12:00:00Z",
    ]) {
      bound?.destroy();
      const b = withContent([paragraph(text("See "), linked("chip", href), text(" now"))]);
      // "See " 1..5, "chip" 5..9, " now" 9..13. Touching one chip character
      // is enough.
      placeSelection(b, 2, 6);
      expect(suggestionScope(b.editor.state), href).toEqual({ ok: false, reason: "inline_atom" });
    }
  });

  it("refuses a caret inside a chip but allows one right after it", () => {
    const b = withContent([paragraph(text("See "), linked("@Alice", "knot://user/u1"), text(" now"))]);
    // "@Alice" spans 5..11.
    placeSelection(b, 7);
    expect(suggestionScope(b.editor.state)).toEqual({ ok: false, reason: "inline_atom" });

    placeSelection(b, 11);
    expect(suggestionScope(b.editor.state)).toMatchObject({ ok: true, kind: "insert", from: 11 });
  });

  it("scope_allows_external_link", () => {
    const b = withContent([paragraph(text("See "), linked("docs", "https://example.com"), text(" now"))]);
    placeSelection(b, 5, 9);
    expect(suggestionScope(b.editor.state)).toEqual({
      ok: true,
      kind: "replace",
      from: 5,
      to: 9,
      text: "docs",
      contextBefore: "See ",
    });
  });

  it("scope_empty_block_insert_refused", () => {
    const b = withContent([paragraph(text("Full")), { type: "paragraph" }]);
    // The empty paragraph opens at 6; its content position is 7.
    placeSelection(b, 7);
    expect(suggestionScope(b.editor.state)).toEqual({ ok: false, reason: "empty_block" });
  });

  it("scope_caret_is_insert", () => {
    const b = withContent([paragraph(text("Hello world"))]);
    placeSelection(b, 6);
    expect(suggestionScope(b.editor.state)).toEqual({
      ok: true,
      kind: "insert",
      from: 6,
      to: 6,
      text: "",
      contextBefore: "Hello",
    });
  });

  it("selection in a single block is a replace with its text", () => {
    const b = withContent([
      { type: "heading", attrs: { level: 2 }, content: [text("Title here")] },
      paragraph(text("Hello brave world")),
    ]);
    // Heading 0..12; the paragraph opens at 12, so "Hello " is 13..19.
    placeSelection(b, 19, 24);
    expect(suggestionScope(b.editor.state)).toEqual({
      ok: true,
      kind: "replace",
      from: 19,
      to: 24,
      text: "brave",
      contextBefore: "Hello ",
    });
  });

  it("caps contextBefore at 40 characters of the same block", () => {
    const long = "x".repeat(50) + "TAIL";
    const b = withContent([paragraph(text("Before block")), paragraph(text(long))]);
    // The second paragraph opens at 14; its text starts at 15.
    const end = 15 + long.length;
    placeSelection(b, end);
    const scope = suggestionScope(b.editor.state);
    expect(scope.ok).toBe(true);
    if (!scope.ok) return;
    expect(scope.contextBefore).toBe("x".repeat(36) + "TAIL");
  });

  it("does not split a surrogate pair at the start of contextBefore", () => {
    const content = "😀" + "y".repeat(39);
    const b = withContent([paragraph(text(content))]);
    // 😀 is two UTF-16 units, so the last 40 units start in its middle.
    placeSelection(b, 1 + content.length);
    const scope = suggestionScope(b.editor.state);
    expect(scope.ok).toBe(true);
    if (!scope.ok) return;
    expect(scope.contextBefore).toBe("y".repeat(39));
  });

  it("has a tooltip for every refusal", () => {
    expect(SCOPE_TOOLTIP).toEqual({
      cross_block: "Select text inside a single paragraph to suggest a change",
      inline_atom: "Suggestions can't include mentions, dates, page links or line breaks",
      empty_block: "Type in the block instead",
    });
  });
});
