import { beforeEach, describe, expect, it } from "vitest";

import { readInitialSkin, useUi } from "./ui";

describe("ui skin", () => {
  beforeEach(() => {
    localStorage.clear();
    document.documentElement.removeAttribute("data-skin");
    document.documentElement.removeAttribute("data-theme");
    useUi.getState().setSkin("light");
  });

  it("stamps both the skin and its light/dark scheme on <html>", () => {
    useUi.getState().setSkin("nord");
    expect(useUi.getState().skin).toBe("nord");
    expect(useUi.getState().theme).toBe("dark");
    expect(document.documentElement.getAttribute("data-skin")).toBe("nord");
    expect(document.documentElement.getAttribute("data-theme")).toBe("dark");

    useUi.getState().setSkin("paper");
    expect(useUi.getState().theme).toBe("light");
    expect(document.documentElement.getAttribute("data-skin")).toBe("paper");
    expect(document.documentElement.getAttribute("data-theme")).toBe("light");
  });

  it("persists the skin to localStorage", () => {
    useUi.getState().setSkin("gruvbox");
    expect(localStorage.getItem("knot.skin")).toBe("gruvbox");
    useUi.getState().setSkin("light");
    expect(localStorage.getItem("knot.skin")).toBe("light");
  });

  it("reads a stored skin back", () => {
    localStorage.setItem("knot.skin", "everforest");
    expect(readInitialSkin()).toBe("everforest");
  });

  it("migrates a pre-skin dark preference to the Dark skin", () => {
    localStorage.removeItem("knot.skin");
    localStorage.setItem("knot.theme", "dark");
    expect(readInitialSkin()).toBe("dark");
  });

  it("prefers a stored skin over the legacy theme key", () => {
    localStorage.setItem("knot.skin", "paper");
    localStorage.setItem("knot.theme", "dark");
    expect(readInitialSkin()).toBe("paper");
  });

  it("falls back to Light for an unknown or missing skin", () => {
    localStorage.removeItem("knot.skin");
    expect(readInitialSkin()).toBe("light");
    localStorage.setItem("knot.skin", "no-such-skin");
    expect(readInitialSkin()).toBe("light");
  });

  it("refuses an unknown skin id and applies the default instead", () => {
    useUi.getState().setSkin("no-such-skin");
    expect(useUi.getState().skin).toBe("light");
    expect(document.documentElement.getAttribute("data-skin")).toBe("light");
  });
});
