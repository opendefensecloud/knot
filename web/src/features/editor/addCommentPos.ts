/** "Add comment" at 12px plus px-2 gutters. Used as the right-clamp width. */
export const ADD_COMMENT_BUTTON_WIDTH = 112;

/** Height reserved above the caret for the button, matching its 12px/py-0.5 box. */
const BUTTON_HEIGHT = 32;
/** Gap below the selection when the button can't sit above it. */
const GAP_BELOW = 4;

type Caret = { top: number; bottom: number; left: number };
type Host = { top: number; left: number; width: number };

/**
 * Position the floating "Add comment" button, in `editor-host`-relative
 * coordinates (the button is `absolute` inside that `relative` host).
 *
 * Pure so the clamps can be tested without a layout engine.
 *
 * Two constraints the inline version missed:
 *  - `left` was clamped only on the low side, so a selection near the right
 *    edge pushed the button past the host. `<main>` is `overflow-y-auto`, so
 *    its `overflow-x` computes to `auto` and the whole app gained a
 *    horizontal scrollbar.
 *  - the "don't leak into the toolbar" comment measured against the
 *    ProseMirror top, not the toolbar. The toolbar is sticky and a sibling
 *    *before* the host, so the host scrolls under it while it stays pinned —
 *    a host-relative offset says nothing about where the toolbar now is.
 *    Compare against its live viewport rect instead.
 */
export function computeAddCommentPos({
  caret,
  host,
  toolbarBottom,
  buttonWidth = ADD_COMMENT_BUTTON_WIDTH,
}: {
  caret: Caret;
  host: Host;
  /** Live `getBoundingClientRect().bottom` of the sticky toolbar, or null in view mode. */
  toolbarBottom: number | null;
  buttonWidth?: number;
}): { top: number; left: number } {
  const aboveViewport = caret.top - BUTTON_HEIGHT;
  const fitsInHost = aboveViewport - host.top >= 0;
  const clearsToolbar = toolbarBottom === null || aboveViewport >= toolbarBottom;

  const top =
    fitsInHost && clearsToolbar
      ? aboveViewport - host.top
      : caret.bottom - host.top + GAP_BELOW;

  const left = Math.max(
    0,
    Math.min(caret.left - host.left, host.width - buttonWidth),
  );

  return { top, left };
}
