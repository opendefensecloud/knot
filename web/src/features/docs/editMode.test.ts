import { afterEach, describe, expect, it } from "vitest";
import { editModeKey, isEditModeShortcut, markDocEditMode } from "./editMode";

afterEach(() => window.sessionStorage.clear());

describe("editMode helper", () => {
  it("builds the per-doc key", () => {
    expect(editModeKey("abc")).toBe("knot.editMode.abc");
  });

  it("marks a doc as edit-mode in sessionStorage", () => {
    markDocEditMode("abc");
    expect(window.sessionStorage.getItem("knot.editMode.abc")).toBe("1");
  });
});

describe("isEditModeShortcut", () => {
  const key = (k: string, mods: Partial<Record<"metaKey" | "ctrlKey" | "shiftKey" | "altKey", boolean>> = {}) => ({
    key: k,
    metaKey: false,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    ...mods,
  });

  it("matches Ctrl+E and Cmd+E", () => {
    expect(isEditModeShortcut(key("e", { ctrlKey: true }))).toBe(true);
    expect(isEditModeShortcut(key("e", { metaKey: true }))).toBe(true);
    expect(isEditModeShortcut(key("E", { metaKey: true }))).toBe(true);
  });

  it("leaves Shift and Alt variants, plain E and other keys alone", () => {
    expect(isEditModeShortcut(key("e", { ctrlKey: true, shiftKey: true }))).toBe(false);
    expect(isEditModeShortcut(key("e", { metaKey: true, altKey: true }))).toBe(false);
    expect(isEditModeShortcut(key("e"))).toBe(false);
    expect(isEditModeShortcut(key("f", { ctrlKey: true }))).toBe(false);
  });
});
