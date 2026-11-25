/**
 * The five callout kinds, shared by the box, the toolbar and paste.
 */

import { describe, expect, it } from "vitest";

import { CALLOUT_KINDS, CALLOUT_LABEL, calloutKind, normaliseTitle } from "./callouts";

describe("callouts", () => {
  it("knows GitHub's and GitLab's five kinds, case-insensitively", () => {
    expect(CALLOUT_KINDS).toEqual(["note", "tip", "important", "warning", "caution"]);
    expect(calloutKind("Tip")).toBe("tip");
    expect(calloutKind("danger")).toBeNull();
    expect(calloutKind(undefined)).toBeNull();
    expect(CALLOUT_LABEL.warning).toBe("Warning");
  });

  it("keeps titles as trimmed single-line text", () => {
    expect(normaliseTitle(" a\nb ")).toBe("a b");
    expect(normaliseTitle("  ")).toBe("");
  });
});
