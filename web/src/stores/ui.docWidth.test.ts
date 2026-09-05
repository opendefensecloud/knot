import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { readInitialDocWidth, useUi } from "./ui";

describe("ui docWidth", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("data-doc-width");
    useUi.getState().setDocWidth("fixed");
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("toggles between fixed and wide", () => {
    expect(useUi.getState().docWidth).toBe("fixed");
    useUi.getState().toggleDocWidth();
    expect(useUi.getState().docWidth).toBe("wide");
    useUi.getState().toggleDocWidth();
    expect(useUi.getState().docWidth).toBe("fixed");
  });

  it("stamps the mode on the document element so CSS can select it", () => {
    useUi.getState().setDocWidth("wide");
    expect(document.documentElement.getAttribute("data-doc-width")).toBe("wide");
    useUi.getState().setDocWidth("fixed");
    expect(document.documentElement.getAttribute("data-doc-width")).toBe("fixed");
  });

  it("persists to localStorage", () => {
    useUi.getState().setDocWidth("wide");
    expect(localStorage.getItem("knot.docWidth")).toBe("wide");
    useUi.getState().setDocWidth("fixed");
    expect(localStorage.getItem("knot.docWidth")).toBe("fixed");
  });

  it("reads a persisted wide preference back", () => {
    localStorage.setItem("knot.docWidth", "wide");
    expect(readInitialDocWidth()).toBe("wide");
  });

  it("falls back to fixed when the stored value is not a valid mode", () => {
    localStorage.setItem("knot.docWidth", "enormous");
    expect(readInitialDocWidth()).toBe("fixed");
  });

  it("falls back to fixed when storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError: storage is disabled");
    });
    expect(readInitialDocWidth()).toBe("fixed");
  });

  it("still applies the mode when storage rejects the write", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    expect(() => useUi.getState().setDocWidth("wide")).not.toThrow();
    expect(useUi.getState().docWidth).toBe("wide");
    expect(document.documentElement.getAttribute("data-doc-width")).toBe("wide");
  });
});
