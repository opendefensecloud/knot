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

  it("says null when there is no such heading", () => {
    expect(scrollToHeading("nope")).toBeNull();
  });

  it("finds a heading from a GitLab-style link without the h- prefix", () => {
    document.body.innerHTML = `<main><h2 id="h-tasks">Tasks</h2></main>`;
    const main = document.querySelector("main")!;
    const scrollBy = vi.fn();
    main.scrollBy = scrollBy;
    const heading = document.getElementById("h-tasks")!;
    heading.getBoundingClientRect = () => ({ top: 300 }) as DOMRect;
    expect(scrollToHeading("tasks")).toBe(heading);
    expect(scrollBy).toHaveBeenCalledWith({ top: 300 - 68, behavior: "auto" });
  });

  it("prefers an exact id over the prefixed one", () => {
    document.body.innerHTML = `<main><h2 id="h-h-tasks">A</h2><h2 id="h-tasks">B</h2></main>`;
    document.querySelector("main")!.scrollBy = vi.fn();
    expect(scrollToHeading("h-tasks")?.textContent).toBe("B");
    expect(scrollToHeading("h-nope")).toBeNull();
  });

  it("finds a GitLab link to a heading whose own text starts with H-", () => {
    // "H-Bridge": GitLab writes #h-bridge, knot gives the id h-h-bridge.
    document.body.innerHTML = `<main><h2 id="h-h-bridge">H-Bridge</h2></main>`;
    document.querySelector("main")!.scrollBy = vi.fn();
    expect(scrollToHeading("h-bridge")?.id).toBe("h-h-bridge");
  });
});
