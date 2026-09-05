/** Breathing room kept between a popup and the viewport edge. */
export const POPUP_MARGIN = 8;

type Anchor = { left: number; top: number; bottom: number };
type Size = { width: number; height: number };

/**
 * Where to put a `position: fixed` popup anchored to a caret rect.
 *
 * Pure so it can be tested without a layout engine. The callers
 * (`placeFixedPopup`) supply the measured sizes.
 *
 * Both extensions used to write `rect.left` straight through. That was safe
 * only because the fixed-width editor column kept the caret ~450px from the
 * right edge; in wide mode the caret reaches the window edge and an unclamped
 * popup lands off-screen — and being `fixed`, it can't be scrolled to.
 */
export function computePopupPosition({
  anchor,
  popup,
  viewport,
  gap = 4,
}: {
  anchor: Anchor;
  popup: Size;
  viewport: Size;
  gap?: number;
}): { left: number; top: number } {
  const rightLimit = viewport.width - popup.width - POPUP_MARGIN;
  const left = Math.max(POPUP_MARGIN, Math.min(anchor.left, rightLimit));

  // Prefer below the caret. Flip above only when below overflows AND above
  // actually fits — a popup taller than the space on either side stays below,
  // where the page can still scroll to it, rather than at a negative offset.
  const below = anchor.bottom + gap;
  const above = anchor.top - popup.height - gap;
  const overflowsBelow = below + popup.height > viewport.height - POPUP_MARGIN;
  const top = overflowsBelow && above >= POPUP_MARGIN ? above : below;

  return { left, top };
}

/**
 * Position an already-visible `position: fixed` popup against `anchor`.
 * The element must be displayed before this runs — a `display: none` element
 * measures 0×0 and would defeat both clamps.
 */
export function placeFixedPopup(el: HTMLElement, anchor: Anchor, gap = 4): void {
  const r = el.getBoundingClientRect();
  const { left, top } = computePopupPosition({
    anchor,
    popup: { width: r.width, height: r.height },
    viewport: { width: window.innerWidth, height: window.innerHeight },
    gap,
  });
  el.style.left = `${Math.round(left)}px`;
  el.style.top = `${Math.round(top)}px`;
}
