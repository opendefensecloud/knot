/**
 * The toolbar's link button must show, and Apply must change, the same link.
 *
 * Once links stopped being inclusive, a caret at a link's end — where pasting
 * a URL leaves it — stopped counting as "in" the link for `isActive`, while
 * Apply and Remove kept acting on it. And Apply with nothing selected relied
 * on the inclusive mark to link every character typed after it. See
 * toolbarLink.ts.
 */

import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { linkRuns, placeSelection, typeText } from "../../test/editorInput";
import { applyLinkUrl, toolbarLinkState } from "./toolbarLink";

const SENTENCE = '<p>see <a href="https://example.com">docs</a> here</p>';
// Positions in SENTENCE: "see " is 1–5, "docs" is 5–9, " here" is 9–14.

describe("toolbarLinkState", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it.each([
    ["at its end", 9],
    ["at its start", 5],
    ["inside it", 7],
  ])("shows the link with the caret %s", (_where, position) => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(SENTENCE);
    placeSelection(bound, position);

    expect(toolbarLinkState(bound.editor)).toEqual({ active: true, href: "https://example.com" });
  });

  it("shows no link with the caret away from one", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(SENTENCE);
    placeSelection(bound, 12);

    expect(toolbarLinkState(bound.editor)).toEqual({ active: false, href: null });
  });

  it("shows the link Apply will change when the caret sits between two links", () => {
    // `<a>one</a><a>two</a>`: getMarkRange prefers the text after the caret,
    // so Apply re-points "two". The button must show "two" as well.
    bound = mountBoundEditor();
    bound.editor.commands.setContent(
      '<p><a href="https://one.example">one</a><a href="https://two.example">two</a></p>',
    );
    placeSelection(bound, 4);

    expect(toolbarLinkState(bound.editor).href).toBe("https://two.example");
    applyLinkUrl(bound.editor, "https://changed.example");
    expect(linkRuns(bound)).toEqual([
      { text: "one", href: "https://one.example" },
      { text: "two", href: "https://changed.example" },
    ]);
  });

  it("shows the link when the selection covers it", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(SENTENCE);
    placeSelection(bound, 5, 9);

    expect(toolbarLinkState(bound.editor)).toEqual({ active: true, href: "https://example.com" });
  });
});

describe("applyLinkUrl", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("inserts the URL as its own link when nothing is selected", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);

    expect(applyLinkUrl(bound.editor, "https://new.example/a")).toBe(true);
    typeText(bound.editor.view, " more");
    expect(linkRuns(bound)).toEqual([
      { text: "see", href: null },
      { text: "https://new.example/a", href: "https://new.example/a" },
      { text: " more", href: null },
    ]);
  });

  it("links the selection and leaves the caret after it", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>read the docs</p>");
    placeSelection(bound, 6, 14);

    expect(applyLinkUrl(bound.editor, "https://example.com")).toBe(true);
    expect(bound.editor.state.selection.empty).toBe(true);
    typeText(bound.editor.view, " now");
    expect(linkRuns(bound)).toEqual([
      { text: "read ", href: null },
      { text: "the docs", href: "https://example.com" },
      { text: " now", href: null },
    ]);
  });

  it("re-points the link at the caret's end instead of inserting a new one", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(SENTENCE);
    placeSelection(bound, 9);

    expect(applyLinkUrl(bound.editor, "https://changed.example")).toBe(true);
    expect(linkRuns(bound)).toEqual([
      { text: "see ", href: null },
      { text: "docs", href: "https://changed.example" },
      { text: " here", href: null },
    ]);
  });

  it("leaves the caret in text after linking a select-all", () => {
    // The end of an AllSelection is not inside a textblock; a caret put
    // there made the next keystroke open a new paragraph.
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>hello</p><p>world</p>");
    bound.editor.commands.selectAll();

    expect(applyLinkUrl(bound.editor, "https://example.com")).toBe(true);
    expect(bound.editor.state.selection.$from.parent.inlineContent).toBe(true);
    typeText(bound.editor.view, "!");
    expect(bound.editor.state.doc.childCount).toBe(2);
  });

  it("rejects a URL setLink would reject, changing nothing", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);
    const documentBefore = bound.editor.getJSON();

    expect(applyLinkUrl(bound.editor, "javascript:alert(1)")).toBe(false);
    expect(bound.editor.getJSON()).toEqual(documentBefore);
  });
});
