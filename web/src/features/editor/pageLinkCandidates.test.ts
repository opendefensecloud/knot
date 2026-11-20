import { describe, expect, it } from "vitest";

import { PAGE_LINK_LIMIT, pageLinkCandidates } from "./pageLinkCandidates";

const P = (id: string, title: string, parent_id: string | null = null) => ({ id, title, parent_id });
const pages = [
  P("t", "Team"),
  P("u", "Urlaub", "t"),
  P("p", "Urlaubsplanung", "u"),
  P("x", "Mein Urlaub"),
  P("e", ""),
];

describe("pageLinkCandidates", () => {
  it("filters case-insensitively and puts prefix matches first", () => {
    expect(pageLinkCandidates(pages, "urlaub", null).map((c) => c.id)).toEqual(["u", "p", "x"]);
  });

  it("returns all pages in title order for an empty query", () => {
    expect(pageLinkCandidates(pages, "  ", null).map((c) => c.title)).toEqual([
      "Mein Urlaub",
      "Team",
      "Untitled",
      "Urlaub",
      "Urlaubsplanung",
    ]);
  });

  it("gives each candidate its ancestor path, root first", () => {
    expect(pageLinkCandidates(pages, "planung", null)[0]?.path).toEqual(["Team", "Urlaub"]);
  });

  it("leaves out the current page", () => {
    expect(pageLinkCandidates(pages, "urlaub", "u").map((c) => c.id)).toEqual(["p", "x"]);
  });

  it("caps the list", () => {
    const many = Array.from({ length: 20 }, (_, i) => P(`d${i}`, `Doc ${i}`));
    expect(pageLinkCandidates(many, "doc", null)).toHaveLength(PAGE_LINK_LIMIT);
  });

  it("survives a parent cycle", () => {
    expect(pageLinkCandidates([P("a", "A", "b"), P("b", "B", "a")], "a", null)[0]?.path).toEqual(["B"]);
  });
});
