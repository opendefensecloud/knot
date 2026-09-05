import { describe, expect, it } from "vitest";

import { computePopupPosition, POPUP_MARGIN } from "./popupPosition";

/** A caret-sized anchor rect at (left, top) — the shape coordsAtPos gives us. */
function anchorAt(left: number, top: number, height = 20) {
  return { left, top, bottom: top + height };
}

describe("computePopupPosition", () => {
  const viewport = { width: 1280, height: 800 };

  it("places the popup at the anchor's left edge when it fits", () => {
    const pos = computePopupPosition({
      anchor: anchorAt(300, 200),
      popup: { width: 180, height: 240 },
      viewport,
    });
    expect(pos.left).toBe(300);
  });

  it("clamps to the viewport's right edge when the popup would overflow", () => {
    // Caret near the right edge of a wide window: 1200 + 380 = 1580 > 1280.
    const pos = computePopupPosition({
      anchor: anchorAt(1200, 200),
      popup: { width: 380, height: 240 },
      viewport,
    });
    expect(pos.left).toBe(1280 - 380 - POPUP_MARGIN);
  });

  it("never places the popup left of the margin", () => {
    // Popup wider than the viewport — the right clamp alone would go negative.
    const pos = computePopupPosition({
      anchor: anchorAt(10, 200),
      popup: { width: 1400, height: 240 },
      viewport,
    });
    expect(pos.left).toBe(POPUP_MARGIN);
  });

  it("places the popup below the anchor, offset by the gap", () => {
    const pos = computePopupPosition({
      anchor: anchorAt(300, 200),
      popup: { width: 180, height: 240 },
      viewport,
      gap: 4,
    });
    expect(pos.top).toBe(224); // bottom (220) + gap (4)
  });

  it("flips above the anchor when there is no room below", () => {
    // Caret near the bottom: 700 + 20 + 4 + 240 = 964 > 800.
    const pos = computePopupPosition({
      anchor: anchorAt(300, 700),
      popup: { width: 180, height: 240 },
      viewport,
      gap: 4,
    });
    expect(pos.top).toBe(700 - 240 - 4); // anchor top - height - gap
  });

  it("stays below when flipping up would leave the viewport", () => {
    // Tall popup, caret near the top: neither fits, prefer below (scrollable
    // direction) over a negative offset the user can never reach.
    const pos = computePopupPosition({
      anchor: anchorAt(300, 30),
      popup: { width: 180, height: 900 },
      viewport,
      gap: 4,
    });
    expect(pos.top).toBe(54); // bottom (50) + gap (4)
  });
});
