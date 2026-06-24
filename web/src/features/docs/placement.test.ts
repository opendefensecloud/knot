import { describe, expect, it } from "vitest";
import { placementParent } from "./placement";

const child = { id: "c", parent_id: "p" };
const root = { id: "r", parent_id: null };

describe("placementParent", () => {
  it("nested → the current doc's id", () => {
    expect(placementParent("nested", child)).toBe("c");
  });
  it("sibling → the current doc's parent_id", () => {
    expect(placementParent("sibling", child)).toBe("p");
  });
  it("sibling of a top-level doc → null (stays top level)", () => {
    expect(placementParent("sibling", root)).toBeNull();
  });
  it("no current doc → null for both", () => {
    expect(placementParent("nested", null)).toBeNull();
    expect(placementParent("sibling", null)).toBeNull();
  });
});
