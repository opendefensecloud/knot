import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  captureReadingAnchor,
  firstIndexBelow,
  holdReadingPosition,
  stickyCoverBottom,
} from "./readingAnchor";

function stubRect(el: Element, rect: () => Partial<DOMRect>) {
  vi.spyOn(el, "getBoundingClientRect").mockImplementation(
    () => ({ top: 0, bottom: 0, left: 0, right: 0, width: 0, height: 0, x: 0, y: 0, ...rect() }) as DOMRect,
  );
}

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  document.body.innerHTML = "";
});

describe("firstIndexBelow", () => {
  const bottoms = (list: number[]) => (i: number) => list[i]!;

  it("returns -1 for an empty list and when everything is above", () => {
    expect(firstIndexBelow(0, bottoms([]), 0)).toBe(-1);
    expect(firstIndexBelow(3, bottoms([10, 20, 30]), 30)).toBe(-1);
  });

  it("treats a bottom exactly on the line as above it", () => {
    expect(firstIndexBelow(3, bottoms([10, 20, 30]), 20)).toBe(2);
    expect(firstIndexBelow(3, bottoms([10, 20, 30]), 0)).toBe(0);
  });

  it("finds the block in a long document", () => {
    const list = Array.from({ length: 1000 }, (_, i) => (i + 1) * 40);
    let reads = 0;
    const index = firstIndexBelow(list.length, (i) => { reads += 1; return list[i]!; }, 20_010);
    expect(index).toBe(500);
    expect(reads).toBeLessThanOrEqual(11);
  });
});

describe("stickyCoverBottom", () => {
  it("is null without a band", () => {
    expect(stickyCoverBottom(document)).toBeNull();
  });

  it("skips hidden and zero-height bands", () => {
    document.body.innerHTML = `
      <div id="a" data-sticky-band></div>
      <div id="b" data-sticky-band style="visibility:hidden"></div>
      <div id="c" data-sticky-band></div>`;
    stubRect(document.getElementById("a")!, () => ({ top: 0, bottom: 0, height: 0 }));
    stubRect(document.getElementById("b")!, () => ({ top: 0, bottom: 90, height: 90 }));
    stubRect(document.getElementById("c")!, () => ({ top: 0, bottom: 49, height: 49 }));
    expect(stickyCoverBottom(document)).toBe(49);
  });
});

describe("reading position", () => {
  let scroller: HTMLElement;
  let prose: HTMLElement;
  let frames: FrameRequestCallback[];
  let now: number;

  beforeEach(() => {
    document.body.innerHTML = `
      <main id="main">
        <section id="scope">
          <div data-sticky-band id="band"></div>
          <div class="ProseMirror" id="prose"><p>1</p><p>2</p><p>3</p></div>
        </section>
      </main>`;
    scroller = document.getElementById("main")!;
    prose = document.getElementById("prose")!;
    stubRect(scroller, () => ({ top: 0, bottom: 700 }));
    stubRect(document.getElementById("band")!, () => ({ top: 0, bottom: 49, height: 49 }));
    frames = [];
    now = 0;
    vi.stubGlobal("requestAnimationFrame", (cb: FrameRequestCallback) => frames.push(cb));
    vi.stubGlobal("cancelAnimationFrame", () => { frames = []; });
    vi.spyOn(window.performance, "now").mockImplementation(() => now);
  });

  function runFrame(at: number) {
    now = at;
    const pending = frames;
    frames = [];
    pending.forEach((cb) => cb(at));
  }

  /** Blocks are 100px tall starting at `offset - scrollTop`. */
  function layoutBlocks(offset: () => number) {
    Array.from(prose.children).forEach((el, i) => {
      stubRect(el, () => {
        const top = offset() + i * 100 - scroller.scrollTop;
        return { top, bottom: top + 100, height: 100 };
      });
    });
  }

  it("captures the first block that starts below the band", () => {
    const scope = document.getElementById("scope")!;
    layoutBlocks(() => 49);
    scroller.scrollTop = 0;
    expect(captureReadingAnchor(scroller, prose, scope)).toEqual({ element: prose.children[0], top: 49 });
    // Block 1 now spans 0–100, starting under the 49px band: block 2 is
    // where the reader is. (jsdom does not clamp scrollTop.)
    scroller.scrollTop = 49;
    expect(captureReadingAnchor(scroller, prose, scope)).toEqual({ element: prose.children[1], top: 100 });
  });

  it("keeps a block that starts under the band when nothing else is on screen", () => {
    const scope = document.getElementById("scope")!;
    // 100px blocks in a 120px-tall scroller: block 2 starts below the fold.
    stubRect(scroller, () => ({ top: 0, bottom: 120 }));
    layoutBlocks(() => 0);
    scroller.scrollTop = 80;
    // Block 1 spans -80–20 (above the band), block 2 spans 20–120.
    expect(captureReadingAnchor(scroller, prose, scope)?.element).toBe(prose.children[1]);
    stubRect(scroller, () => ({ top: 0, bottom: 90 }));
    scroller.scrollTop = 0;
    layoutBlocks(() => 0);
    // Block 1 spans 0–100 and fills the 90px view; block 2 is off screen.
    expect(captureReadingAnchor(scroller, prose, scope)?.element).toBe(prose.children[0]);
  });

  it("scrolls by the anchor's shift, and leaves sub-pixel shifts alone", () => {
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    const anchor = { element: prose.children[2]!, top: 0 };
    offset = 65;
    holdReadingPosition(scroller, anchor);
    expect(scroller.scrollTop).toBe(265);

    offset = 65.5;
    holdReadingPosition(scroller, anchor);
    expect(scroller.scrollTop).toBe(265);
  });

  it("moves the anchor below a band that grew since the capture", () => {
    let bandBottom = 49;
    stubRect(document.getElementById("band")!, () => ({ top: 0, bottom: bandBottom, height: bandBottom }));
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    // Block 3 was captured at 60 under the 49px dock.
    const anchor = { element: prose.children[2]!, top: 60 };
    offset = 60;
    // A band that still clears the block leaves it where it was.
    holdReadingPosition(scroller, anchor, { scope: document.getElementById("scope")! });
    expect(scroller.scrollTop).toBe(200);
    // One that would cover it pins it just below: 130 - 60 = 70px up.
    bandBottom = 130;
    holdReadingPosition(scroller, anchor, { scope: document.getElementById("scope")! });
    expect(scroller.scrollTop).toBe(130);
  });

  it("re-checks once on the next frame when durationMs is 0", () => {
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    holdReadingPosition(scroller, { element: prose.children[2]!, top: 0 });
    offset = 30;
    runFrame(16);
    expect(scroller.scrollTop).toBe(230);
    expect(frames).toHaveLength(0);
  });

  it("keeps holding for durationMs, then stops", () => {
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    holdReadingPosition(scroller, { element: prose.children[2]!, top: 0 }, { durationMs: 250 });
    offset = 10;
    runFrame(100);
    expect(scroller.scrollTop).toBe(210);
    offset = 20;
    runFrame(300);
    expect(scroller.scrollTop).toBe(220);
    expect(frames).toHaveLength(0);
  });

  it("stops when something else scrolls", () => {
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    holdReadingPosition(scroller, { element: prose.children[2]!, top: 0 }, { durationMs: 250 });
    scroller.scrollTop = 500;
    offset = 40;
    runFrame(16);
    expect(scroller.scrollTop).toBe(500);
    expect(frames).toHaveLength(0);
  });

  it("stops when the anchor leaves the DOM", () => {
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    const element = prose.children[2]!;
    holdReadingPosition(scroller, { element, top: 0 }, { durationMs: 250 });
    element.remove();
    offset = 40;
    runFrame(16);
    expect(scroller.scrollTop).toBe(200);
  });

  it("cancel stops further frames", () => {
    let offset = 0;
    layoutBlocks(() => offset);
    scroller.scrollTop = 200;
    const cancel = holdReadingPosition(scroller, { element: prose.children[2]!, top: 0 }, { durationMs: 250 });
    cancel();
    offset = 40;
    runFrame(16);
    expect(scroller.scrollTop).toBe(200);
  });
});
