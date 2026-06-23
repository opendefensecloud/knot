import { afterEach, describe, expect, it } from "vitest";
import { editModeKey, markDocEditMode } from "./editMode";

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
