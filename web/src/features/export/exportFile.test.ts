import { describe, expect, it } from "vitest";

import { exportFileName, fallbackMessage, parseFallbacks } from "./exportFile";

describe("exportFileName", () => {
  it("follows the shared rule", () => {
    expect(exportFileName("Q4 / Plan: <draft>?", "docx")).toBe("Q4 - Plan- -draft--.docx");
    expect(exportFileName(" .. ", "odt")).toBe("Untitled.odt");
    expect(exportFileName("x".repeat(200), "docx")).toHaveLength(125);
    expect(exportFileName("Über 😀", "docx")).toBe("Über 😀.docx");
    expect(exportFileName("a".repeat(119) + "😀😀", "docx")).toBe("a".repeat(119) + "😀.docx");
    expect(exportFileName("a\u0001b\u009fc", "odt")).toBe("a-b-c.odt");
    expect(exportFileName("  Notes. ", "odt")).toBe("Notes.odt");
  });
});

describe("parseFallbacks", () => {
  it("reads diagrams and images, ignores the rest", () => {
    expect(parseFallbacks("diagrams=2, images=1, unknown=3")).toEqual({ diagrams: 2, images: 1 });
  });
  it("returns zeros for null and garbage", () => {
    expect(parseFallbacks(null)).toEqual({ diagrams: 0, images: 0 });
    expect(parseFallbacks("nonsense, diagrams=x")).toEqual({ diagrams: 0, images: 0 });
  });
});

describe("fallbackMessage", () => {
  it("builds the toast copy", () => {
    expect(fallbackMessage({ diagrams: 2, images: 1 }, false)).toBe(
      "Exported. 2 diagrams and 1 image couldn't be included.",
    );
    expect(fallbackMessage({ diagrams: 1, images: 0 }, false)).toBe(
      "Exported. 1 diagram couldn't be included.",
    );
    expect(fallbackMessage({ diagrams: 0, images: 0 }, true)).toBe(
      "Exported. Your latest edits weren't saved yet and may be missing.",
    );
    expect(fallbackMessage({ diagrams: 0, images: 0 }, false)).toBeNull();
  });
});
