import { describe, expect, it } from "vitest";

import { checkedState } from "./TaskListExtension";

/**
 * The `checked` attribute reaches `list_item` from two sources that disagree
 * on type, so the editor has to accept both. Importing a Markdown file with a
 * checklist used to render plain bullets because only the boolean form was
 * understood.
 */
describe("checkedState", () => {
  it("accepts the booleans the editor's own input rules set", () => {
    expect(checkedState(true)).toBe(true);
    expect(checkedState(false)).toBe(false);
  });

  it("accepts the strings a server-parsed document stores in Yjs", () => {
    expect(checkedState("true")).toBe(true);
    expect(checkedState("false")).toBe(false);
  });

  it("reports a plain bullet as null", () => {
    expect(checkedState(null)).toBeNull();
    expect(checkedState(undefined)).toBeNull();
    expect(checkedState("")).toBeNull();
    expect(checkedState("yes")).toBeNull();
  });
});
