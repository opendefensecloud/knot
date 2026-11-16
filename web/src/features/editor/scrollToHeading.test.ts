/**
 * A click in the table of contents puts the heading just below the sticky
 * band (dock or toolbar), not behind it.
 */

import { afterEach, describe, expect, it, vi } from "vitest";

vi.mock("../docs/readingAnchor", () => ({ stickyCoverBottom: () => 60 }));

import { scrollToHeading } from "./scrollToHeading";

afterEach(() => {
  document.body.innerHTML = "";
});

describe("scrollToHeading", () => {
  it("scrolls the heading below the sticky band", () => {
    document.body.innerHTML = `<main><h2 id="tasks-1">Tasks</h2></main>`;
    const main = document.querySelector("main")!;
    const scrollBy = vi.fn();
    main.scrollBy = scrollBy;
    const heading = document.getElementById("tasks-1")!;
    heading.getBoundingClientRect = () => ({ top: 500 }) as DOMRect;
    expect(scrollToHeading("tasks-1")).not.toBeNull();
    expect(scrollBy).toHaveBeenCalledWith({ top: 500 - 68, behavior: "auto" });
  });

  it("looks only inside its own editor and scrolls that pane", () => {
    document.body.innerHTML = `
      <main><div id="live"><h2 id="tasks">Tasks</h2></div>
        <div id="pane" style="overflow-y: auto"><div id="old"><h2 id="tasks">Tasks</h2></div></div>
      </main>`;
    const main = document.querySelector("main")!;
    const pane = document.getElementById("pane")!;
    const mainScroll = vi.fn();
    const paneScroll = vi.fn();
    main.scrollBy = mainScroll;
    pane.scrollBy = paneScroll;
    pane.getBoundingClientRect = () => ({ top: 100 }) as DOMRect;
    const old = document.querySelector<HTMLElement>("#old h2")!;
    old.getBoundingClientRect = () => ({ top: 400 }) as DOMRect;
    expect(scrollToHeading("tasks", document.getElementById("old")!)).not.toBeNull();
    expect(mainScroll).not.toHaveBeenCalled();
    // Below the pane's top: the page's sticky band is not over the pane.
    expect(paneScroll).toHaveBeenCalledWith({ top: 400 - 100 - 8, behavior: "auto" });
  });

  it("says false when there is no such heading", () => {
    expect(scrollToHeading("nope")).toBeNull();
  });
});
