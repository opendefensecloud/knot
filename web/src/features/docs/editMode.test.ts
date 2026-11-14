import { afterEach, describe, expect, it } from "vitest";

import { restorePlatform, stubPlatform } from "../../test/platform";
import {
  editModeAriaKeyshortcuts,
  editModeKey,
  editModeShortcutLabel,
  isEditModeShortcut,
  markDocEditMode,
} from "./editMode";

afterEach(() => {
  window.sessionStorage.clear();
  restorePlatform();
});

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

  describe("on Apple platforms", () => {
    const apple = true;

    it("matches ⌘E, in either case", () => {
      expect(isEditModeShortcut(key("e", { metaKey: true }), apple)).toBe(true);
      expect(isEditModeShortcut(key("E", { metaKey: true }), apple)).toBe(true);
    });

    it("leaves Ctrl+E to the editor: it is the system's end-of-line", () => {
      expect(isEditModeShortcut(key("e", { ctrlKey: true }), apple)).toBe(false);
    });

    it("leaves ⌃⌘E, Shift and Alt variants alone", () => {
      expect(isEditModeShortcut(key("e", { metaKey: true, ctrlKey: true }), apple)).toBe(false);
      expect(isEditModeShortcut(key("e", { metaKey: true, shiftKey: true }), apple)).toBe(false);
      expect(isEditModeShortcut(key("e", { metaKey: true, altKey: true }), apple)).toBe(false);
    });
  });

  describe("elsewhere", () => {
    const apple = false;

    it("matches Ctrl+E, in either case", () => {
      expect(isEditModeShortcut(key("e", { ctrlKey: true }), apple)).toBe(true);
      expect(isEditModeShortcut(key("E", { ctrlKey: true }), apple)).toBe(true);
    });

    it("leaves Super/Meta+E to the system", () => {
      expect(isEditModeShortcut(key("e", { metaKey: true }), apple)).toBe(false);
    });

    it("leaves Ctrl+Meta+E, Shift and Alt variants alone", () => {
      expect(isEditModeShortcut(key("e", { ctrlKey: true, metaKey: true }), apple)).toBe(false);
      expect(isEditModeShortcut(key("e", { ctrlKey: true, shiftKey: true }), apple)).toBe(false);
      expect(isEditModeShortcut(key("e", { ctrlKey: true, altKey: true }), apple)).toBe(false);
    });
  });

  it("ignores plain E and other keys on both platforms", () => {
    for (const apple of [true, false]) {
      expect(isEditModeShortcut(key("e"), apple)).toBe(false);
      expect(isEditModeShortcut(key("f", { ctrlKey: true, metaKey: true }), apple)).toBe(false);
      expect(isEditModeShortcut(key("f", { ctrlKey: true }), apple)).toBe(false);
      expect(isEditModeShortcut(key("f", { metaKey: true }), apple)).toBe(false);
    }
  });

  it("defaults to the platform it runs on", () => {
    stubPlatform("MacIntel");
    expect(isEditModeShortcut(key("e", { metaKey: true }))).toBe(true);
    expect(isEditModeShortcut(key("e", { ctrlKey: true }))).toBe(false);
    stubPlatform("Linux x86_64");
    expect(isEditModeShortcut(key("e", { metaKey: true }))).toBe(false);
    expect(isEditModeShortcut(key("e", { ctrlKey: true }))).toBe(true);
  });
});

describe("edit-mode shortcut labels", () => {
  it("shows ⌘E and announces Meta+E on Apple platforms", () => {
    stubPlatform("MacIntel");
    expect(editModeShortcutLabel()).toBe("⌘E");
    expect(editModeAriaKeyshortcuts()).toBe("Meta+E");
  });

  it("shows Ctrl+E and announces Control+E elsewhere", () => {
    stubPlatform("Win32");
    expect(editModeShortcutLabel()).toBe("Ctrl+E");
    expect(editModeAriaKeyshortcuts()).toBe("Control+E");
  });
});
