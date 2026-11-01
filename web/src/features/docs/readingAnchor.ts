/**
 * Keeps the text the reader is looking at in place while the layout above it
 * changes: the formatting toolbar mounting or unmounting on an edit-mode
 * toggle, or the comment rail insetting the page.
 *
 * The scroll container is the app's `<main>`, not the window. Corrections are
 * measured (where the anchor is now vs. where it was), so they cannot add to
 * a browser's own scroll anchoring: once that has compensated, the delta is 0.
 */

/** A block of the document and its viewport top when it was captured. */
export type ReadingAnchor = { element: Element; top: number };

export function findScroller(el: Element): HTMLElement | null {
  return el.closest("main");
}

/**
 * Bottom edge of the sticky band covering the top of the page: the view-mode
 * dock or the formatting toolbar, whichever is up. Both carry
 * `data-sticky-band`. Hidden or zero-height bands are skipped; null if none.
 */
export function stickyCoverBottom(scope: ParentNode): number | null {
  let bottom: number | null = null;
  for (const band of Array.from(scope.querySelectorAll("[data-sticky-band]"))) {
    const rect = band.getBoundingClientRect();
    if (rect.height === 0) continue;
    if (window.getComputedStyle(band).visibility === "hidden") continue;
    bottom = bottom === null ? rect.bottom : Math.max(bottom, rect.bottom);
  }
  return bottom;
}

/**
 * Lowest index in [0, count) whose bottom lies below `line`, else -1.
 * Block bottoms grow in document order, so a binary search keeps a long
 * document to O(log n) layout reads.
 */
export function firstIndexBelow(count: number, bottomAt: (i: number) => number, line: number): number {
  let low = 0;
  let high = count;
  while (low < high) {
    const mid = (low + high) >> 1;
    if (bottomAt(mid) > line) high = mid;
    else low = mid + 1;
  }
  return low < count ? low : -1;
}

/**
 * The top-level block of `prose` the reader is at, with its current viewport
 * top: the first block that starts below the sticky band (or the scroller's
 * top) and is on screen. A block that starts above the band is only used when
 * it fills the view — pinning its top would let the lines being read drift
 * when it re-wraps (the comment rail narrows the column).
 */
export function captureReadingAnchor(
  scroller: HTMLElement,
  prose: Element,
  scope: ParentNode,
): ReadingAnchor | null {
  const scrollerRect = scroller.getBoundingClientRect();
  const line = coverLine(scroller, scope);
  const blocks = prose.children;
  const index = firstIndexBelow(blocks.length, (i) => blocks[i]!.getBoundingClientRect().bottom, line);
  if (index < 0) return null;
  let element = blocks[index]!;
  let top = element.getBoundingClientRect().top;
  const next = blocks[index + 1];
  if (top < line && next) {
    const nextTop = next.getBoundingClientRect().top;
    if (nextTop < scrollerRect.bottom) {
      element = next;
      top = nextTop;
    }
  }
  return { element, top };
}

function coverLine(scroller: HTMLElement, scope: ParentNode): number {
  return Math.max(scroller.getBoundingClientRect().top, stickyCoverBottom(scope) ?? -Infinity);
}

/**
 * Pin `anchor` at its recorded viewport top: correct now, then again on each
 * animation frame while less than `durationMs` has passed (0 means exactly
 * one follow-up frame). Stops as soon as something else scrolls the
 * container (the reader, or a `scrollIntoView`) or the anchor leaves the
 * DOM. Returns a cancel function.
 *
 * With `scope`, the band is measured again on each correction, and an anchor
 * that would now sit under it (the toolbar wraps to several rows on a phone,
 * the dock is one) is pinned just below it instead.
 */
export function holdReadingPosition(
  scroller: HTMLElement,
  anchor: ReadingAnchor,
  { durationMs = 0, scope }: { durationMs?: number; scope?: ParentNode } = {},
): () => void {
  let expectedScrollTop = scroller.scrollTop;
  let frame = 0;
  let cancelled = false;

  const correct = (): boolean => {
    if (!anchor.element.isConnected) return false;
    if (Math.abs(scroller.scrollTop - expectedScrollTop) > 1) return false;
    const target = scope ? Math.max(anchor.top, coverLine(scroller, scope)) : anchor.top;
    const delta = anchor.element.getBoundingClientRect().top - target;
    if (Math.abs(delta) >= 1) scroller.scrollTop += delta;
    // Read back rather than add: the browser rounds scrollTop.
    expectedScrollTop = scroller.scrollTop;
    return true;
  };

  if (!correct()) return () => {};
  const startedAt = window.performance.now();
  const tick = (now: number) => {
    if (cancelled || !correct()) return;
    if (now - startedAt < durationMs) frame = window.requestAnimationFrame(tick);
  };
  frame = window.requestAnimationFrame(tick);
  return () => {
    cancelled = true;
    window.cancelAnimationFrame(frame);
  };
}
