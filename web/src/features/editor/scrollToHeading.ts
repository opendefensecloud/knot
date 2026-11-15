/**
 * Scrolls to the heading with id `slug` (see `headingAnchors.ts`) so that
 * it lands just below the sticky band at the top of the page (the dock or
 * the formatting toolbar), not behind it.
 */

import { stickyCoverBottom } from "../docs/readingAnchor";

/** Gap between the band and the heading, in px. */
const GAP = 8;

/** True when the heading exists and the scroll was started. The band can
 *  shrink once the page scrolls (the header condenses), so the position is
 *  measured again on the next two frames and corrected. */
export function scrollToHeading(slug: string, doc: Document = document): boolean {
  const heading = doc.getElementById(slug);
  if (!heading) return false;
  const scroller: Element | Window = heading.closest("main") ?? window;
  const place = () => {
    if (!heading.isConnected) return;
    const band = stickyCoverBottom(doc) ?? 0;
    const top = heading.getBoundingClientRect().top - band - GAP;
    if (Math.abs(top) > 1) scroller.scrollBy({ top, behavior: "auto" });
  };
  place();
  requestAnimationFrame(() => {
    place();
    requestAnimationFrame(place);
  });
  return true;
}
