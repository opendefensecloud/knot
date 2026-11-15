/**
 * Scrolls to the heading with id `slug` (see `headingAnchors.ts`) so that
 * it lands just below the sticky band at the top of the page (the dock or
 * the formatting toolbar), not behind it.
 */

import { stickyCoverBottom } from "../docs/readingAnchor";

/** Gap between the band and the heading, in px. */
const GAP = 8;

/** True when the heading exists and the scroll was started. */
export function scrollToHeading(slug: string, doc: Document = document): boolean {
  const heading = doc.getElementById(slug);
  if (!heading) return false;
  const band = stickyCoverBottom(doc) ?? 0;
  const top = heading.getBoundingClientRect().top - band - GAP;
  const scroller = heading.closest("main");
  if (scroller) scroller.scrollBy({ top, behavior: "smooth" });
  else window.scrollBy({ top, behavior: "smooth" });
  return true;
}
