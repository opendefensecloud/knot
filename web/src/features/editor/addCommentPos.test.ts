import { describe, expect, it } from "vitest";

import { ADD_COMMENT_BUTTON_WIDTH, computeAddCommentPos } from "./addCommentPos";

/** editor-host rect in viewport space: 900px wide, starting 100px down. */
const host = { top: 100, left: 260, width: 900 };

describe("computeAddCommentPos", () => {
  it("places the button above the selection when there is room", () => {
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 300 },
      host,
      toolbarBottom: null,
    });
    expect(pos.top).toBe(400 - 100 - 32); // caret top, host-relative, minus button height
  });

  it("places the button below the selection when it would sit above the editor", () => {
    // Selection on the first line: above would be host-relative -12.
    const pos = computeAddCommentPos({
      caret: { top: 120, bottom: 140, left: 300 },
      host,
      toolbarBottom: null,
    });
    expect(pos.top).toBe(140 - 100 + 4); // caret bottom, host-relative, plus gap
  });

  it("places the button below when above would collide with the pinned toolbar", () => {
    // The host has scrolled under a sticky toolbar whose bottom is at 160.
    // Above is host-relative +268 — the old check passed it — but in viewport
    // space the button would land at 368, under the toolbar, and swallow
    // clicks on Bold/H1.
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 300 },
      host: { top: 100, left: 260, width: 900 },
      toolbarBottom: 420,
    });
    expect(pos.top).toBe(420 - 100 + 4);
  });

  it("keeps the button above when the toolbar is clear of it", () => {
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 300 },
      host,
      toolbarBottom: 300,
    });
    expect(pos.top).toBe(268);
  });

  it("aligns the button with the selection's left edge", () => {
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 500 },
      host,
      toolbarBottom: null,
    });
    expect(pos.left).toBe(240); // 500 - 260
  });

  it("clamps the button to the host's right edge", () => {
    // Selection starting 40px from the right of a wide host: unclamped this
    // pushes the button past editor-host and gives <main> a scrollbar.
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 1120 },
      host,
      toolbarBottom: null,
    });
    expect(pos.left).toBe(900 - ADD_COMMENT_BUTTON_WIDTH);
  });

  it("never places the button left of the host", () => {
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 200 },
      host,
      toolbarBottom: null,
    });
    expect(pos.left).toBe(0);
  });

  it("clamps to zero rather than negative when the host is narrower than the button", () => {
    const pos = computeAddCommentPos({
      caret: { top: 400, bottom: 420, left: 300 },
      host: { top: 100, left: 260, width: 80 },
      toolbarBottom: null,
    });
    expect(pos.left).toBe(0);
  });
});
