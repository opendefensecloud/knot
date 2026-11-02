import { describe, expect, it } from "vitest";

import { highlightAt } from "./highlightClick";

function root(html: string): HTMLElement {
  const el = document.createElement("div");
  el.innerHTML = html;
  return el;
}

describe("highlightAt", () => {
  it("takes the nearest highlight when one sits inside the other", () => {
    const dom = root('<div data-comment-id="t1"><p>Hi <span data-suggestion-id="s1">there</span></p></div>');
    expect(highlightAt(dom, dom.querySelector("[data-suggestion-id]"))).toEqual({ kind: "suggestion", id: "s1" });
    expect(highlightAt(dom, dom.querySelector("p"))).toEqual({ kind: "comment", id: "t1" });
  });

  it("gives a span carrying both ids to the highlight covering less text", () => {
    const dom = root(
      '<p><span data-comment-id="t1">Hello </span><span data-comment-id="t1" data-suggestion-id="s1">brave</span></p>',
    );
    const both = dom.querySelector("[data-suggestion-id]");
    expect(highlightAt(dom, both)).toEqual({ kind: "suggestion", id: "s1" });

    const wider = root(
      '<p><span data-suggestion-id="s1">Hello </span><span data-comment-id="t1" data-suggestion-id="s1">brave</span></p>',
    );
    expect(highlightAt(wider, wider.querySelector("[data-comment-id]"))).toEqual({ kind: "comment", id: "t1" });
  });

  it("gives a tie to the suggestion", () => {
    const dom = root('<p><span data-comment-id="t1" data-suggestion-id="s1">brave</span></p>');
    expect(highlightAt(dom, dom.querySelector("span"))).toEqual({ kind: "suggestion", id: "s1" });
  });

  it("returns null off any highlight", () => {
    const dom = root("<p>plain</p>");
    expect(highlightAt(dom, dom.querySelector("p"))).toBeNull();
    expect(highlightAt(dom, null)).toBeNull();
  });
});
