/**
 * Scrolls to the heading with id `slug` (see `headingAnchors.ts`) so that
 * it lands just below the sticky band at the top of the page (the dock or
 * the formatting toolbar), not behind it.
 */

import { stickyCoverBottom } from "../docs/readingAnchor";

/** Gap between the band and the heading, in px. */
const GAP = 8;

/** The nearest ancestor of `el` that scrolls (overflow auto or scroll). */
function scrollParent(el: HTMLElement): HTMLElement | null {
  for (let node = el.parentElement; node; node = node.parentElement) {
    const { overflowY } = window.getComputedStyle(node);
    if (overflowY === "auto" || overflowY === "scroll") return node;
  }
  return null;
}

/** True when the heading exists in `root` (the editor it belongs to, so a
 *  version open in the history drawer scrolls its own pane) and the scroll
 *  was started. On the page itself it lands below the sticky band, which
 *  can shrink once the page scrolls (the header condenses), so the
 *  position is measured again on the next two frames and corrected; in
 *  another pane it lands below the pane's top. */
export function scrollToHeading(slug: string, root: ParentNode = document): boolean {
  const heading = Array.from(root.querySelectorAll<HTMLElement>("[id]")).find((el) => el.id === slug);
  if (!heading) return false;
  const doc = heading.ownerDocument;
  const pane = scrollParent(heading);
  const onPage = pane === null || pane.tagName === "MAIN";
  const scroller: HTMLElement | Window = pane ?? heading.closest("main") ?? window;
  const place = () => {
    if (!heading.isConnected) return;
    const floor = onPage ? (stickyCoverBottom(doc) ?? 0) : (pane?.getBoundingClientRect().top ?? 0);
    const top = heading.getBoundingClientRect().top - floor - GAP;
    if (Math.abs(top) > 1) scroller.scrollBy({ top, behavior: "auto" });
  };
  place();
  window.requestAnimationFrame(() => {
    place();
    window.requestAnimationFrame(place);
  });
  return true;
}
