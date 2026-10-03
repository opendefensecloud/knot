import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { effectiveSidebarWidth, readInitialSidebarWidth, sidebarWidthBounds, useUi } from "./ui";

const realWidth = window.innerWidth;

function setViewportWidth(w: number) {
  Object.defineProperty(window, "innerWidth", { value: w, configurable: true, writable: true });
}

/** What the grid and layout.css read: the width the sidebar renders at. */
function stampedWidth() {
  return document.documentElement.style.getPropertyValue("--knot-sidebar-w");
}

afterEach(() => {
  vi.restoreAllMocks();
  setViewportWidth(realWidth);
});

describe("sidebar width bounds", () => {
  it.each([
    // [window width, widest the sidebar may be]
    [1280, 480], // plenty of room: the hard cap binds
    [860, 480], // exactly 480 + 380
    [800, 420], // the content column keeps its 380px
    [640, 260], // the tablet edge: the default 260 is all that fits
    [500, 200], // never below the minimum, even when nothing fits
  ])("in a %ipx window lets the sidebar grow to %ipx", (vw, max) => {
    expect(sidebarWidthBounds(vw)).toEqual({ min: 200, max });
  });
});

describe("effective sidebar width", () => {
  it.each([
    // [saved preference, window width, rendered width]
    [260, 1280, 260],
    [340, 1280, 340],
    [480, 800, 420], // a wide preference waits for a wider window
    [150, 1280, 200],
    [600, 1600, 480],
  ])("renders a %ipx preference in a %ipx window at %ipx", (pref, vw, want) => {
    expect(effectiveSidebarWidth(pref, vw)).toBe(want);
  });
});

describe("readInitialSidebarWidth", () => {
  beforeEach(() => {
    localStorage.clear();
  });

  it("defaults to 260 when nothing is saved", () => {
    expect(readInitialSidebarWidth()).toBe(260);
  });

  it("reads a saved width back", () => {
    localStorage.setItem("knot.sidebarWidth", "340");
    expect(readInitialSidebarWidth()).toBe(340);
  });

  it.each(["wide", "", "   ", "340px", "Infinity", "NaN"])(
    "falls back to 260 for the unusable value %j",
    (raw) => {
      localStorage.setItem("knot.sidebarWidth", raw);
      expect(readInitialSidebarWidth()).toBe(260);
    },
  );

  it.each([
    ["100", 200],
    ["-40", 200],
    ["9999", 480],
  ])("clamps the out-of-range value %j to %ipx", (raw, want) => {
    localStorage.setItem("knot.sidebarWidth", raw);
    expect(readInitialSidebarWidth()).toBe(want);
  });

  it("rounds a fractional value to whole pixels", () => {
    localStorage.setItem("knot.sidebarWidth", "340.6");
    expect(readInitialSidebarWidth()).toBe(341);
  });

  it("falls back to 260 when storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError: storage is disabled");
    });
    expect(readInitialSidebarWidth()).toBe(260);
  });
});

describe("ui sidebarWidth", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.style.removeProperty("--knot-sidebar-w");
    setViewportWidth(1280);
    useUi.setState({ sidebarWidth: 260 });
  });

  it("starts from the saved width", async () => {
    localStorage.setItem("knot.sidebarWidth", "340");
    vi.resetModules();
    const fresh = await import("./ui");
    expect(fresh.useUi.getState().sidebarWidth).toBe(340);
  });

  it("keeps the width in whole pixels between 200 and 480", () => {
    useUi.getState().setSidebarWidth(300.4);
    expect(useUi.getState().sidebarWidth).toBe(300);
    useUi.getState().setSidebarWidth(9999);
    expect(useUi.getState().sidebarWidth).toBe(480);
    useUi.getState().setSidebarWidth(50);
    expect(useUi.getState().sidebarWidth).toBe(200);
  });

  it("persists the width as a plain integer string", () => {
    useUi.getState().setSidebarWidth(340);
    expect(localStorage.getItem("knot.sidebarWidth")).toBe("340");
  });

  it("stamps the rendered width on <html> for the grid and layout.css", () => {
    useUi.getState().setSidebarWidth(340);
    expect(stampedWidth()).toBe("340px");
  });

  it("renders what the window allows but saves what was asked", () => {
    setViewportWidth(800);
    useUi.getState().setSidebarWidth(480);
    expect(useUi.getState().sidebarWidth).toBe(480);
    expect(localStorage.getItem("knot.sidebarWidth")).toBe("480");
    expect(stampedWidth()).toBe("420px");
  });

  it("still applies the width when storage rejects the write", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    expect(() => useUi.getState().setSidebarWidth(340)).not.toThrow();
    expect(useUi.getState().sidebarWidth).toBe(340);
    expect(stampedWidth()).toBe("340px");
  });
});
