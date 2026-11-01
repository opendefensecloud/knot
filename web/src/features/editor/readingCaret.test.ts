import { describe, expect, it } from "vitest";

import { chooseCaretStrategy } from "./readingCaret";

const visible = { visibleTop: 49, visibleBottom: 700 };

describe("chooseCaretStrategy", () => {
  it("keeps a text selection that is on screen", () => {
    expect(chooseCaretStrategy({ isText: true, headTop: 300, headBottom: 320, ...visible })).toBe("keep");
  });

  it("moves to the reading line when the caret is off screen", () => {
    expect(chooseCaretStrategy({ isText: true, headTop: -400, headBottom: -380, ...visible })).toBe("reading-line");
    expect(chooseCaretStrategy({ isText: true, headTop: 900, headBottom: 920, ...visible })).toBe("reading-line");
  });

  it("moves to the reading line when the caret sits under the sticky band", () => {
    expect(chooseCaretStrategy({ isText: true, headTop: 20, headBottom: 40, ...visible })).toBe("reading-line");
  });

  it("never keeps a node selection", () => {
    expect(chooseCaretStrategy({ isText: false, headTop: 300, headBottom: 320, ...visible })).toBe("reading-line");
  });
});
