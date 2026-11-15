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
    expect(scrollToHeading("tasks-1")).toBe(true);
    expect(scrollBy).toHaveBeenCalledWith({ top: 500 - 68, behavior: "smooth" });
  });

  it("says false when there is no such heading", () => {
    expect(scrollToHeading("nope")).toBe(false);
  });
});
