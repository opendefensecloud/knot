import { useEffect, useRef, useSyncExternalStore, type KeyboardEvent, type PointerEvent } from "react";

import {
  SIDEBAR_WIDTH_DEFAULT,
  effectiveSidebarWidth,
  sidebarWidthBounds,
  stampSidebarWidth,
  useUi,
} from "../stores/ui";

/** Keyboard steps: one for fine adjustment, Shift for a quick jump. */
const STEP = 16;
const SHIFT_STEP = 64;
/** How far a press may wander, in CSS px, and still be a click rather than
 *  a drag — about the OS double-click slop. A hand that shakes by a device
 *  pixel during a double-click still gets its reset. */
const DRAG_SLOP = 4;

function subscribeToResize(onChange: () => void) {
  window.addEventListener("resize", onChange);
  return () => window.removeEventListener("resize", onChange);
}

const windowWidth = () => window.innerWidth;

type Drag = {
  pointerId: number;
  move: (clientX: number) => void;
  /** Ends the drag, saving the width if it changed; true when the pointer
   *  travelled far enough to be a drag rather than a click. */
  end: () => boolean;
};

/**
 * One drag, from press to release. Deliberately plain DOM: the edge has to
 * keep up with the pointer, and React state per pointermove would re-render
 * the shell — tree, editor and all — at pointer rate. At most once a frame
 * the width goes straight onto the two elements that draw the edge, the
 * grid's sidebar column and the handle, and not to --knot-sidebar-w: every
 * element inherits that variable, so stamping it each frame would restyle
 * the whole document each frame, which on a long document is more work
 * than a frame has room for. The variable, the store and localStorage get
 * the width once, on release, and the edge goes back to following the
 * variable. Held inline like that, the edge also stays under the pointer
 * when a window resize restamps the variable mid-drag.
 */
function startDrag(handle: HTMLElement, pointerId: number, startX: number): Drag {
  // Measured once, here: nothing on the pointermove path reads layout.
  const viewportWidth = window.innerWidth;
  const { min, max } = sidebarWidthBounds(viewportWidth);
  const startWidth = effectiveSidebarWidth(useUi.getState().sidebarWidth, viewportWidth);
  // AppShell renders the handle straight into its grid, so the parent is the
  // grid whose first column is the sidebar.
  const grid = handle.parentElement!;
  // Both inline values the drag writes over are React's, and go back as
  // React rendered them. React re-applies a style only when its prop
  // changes, which these never do, so nothing else left on them would ever
  // be put right: in pixels, the edge would stop following the variable and
  // no window resize could clamp it again; cleared, the grid would lose its
  // columns.
  const rendered = { columns: grid.style.gridTemplateColumns, left: handle.style.left };
  let width = startWidth;
  // The furthest the pointer has been from where it pressed, either way.
  let travel = 0;
  let frame = 0;
  // What the last frame wrote, once one has.
  let pinned: { columns: string; left: string } | null = null;

  const paint = () => {
    frame = 0;
    // The handle 3px inside the edge, where its calc() puts it at rest.
    pinned = { columns: `${width}px 1fr`, left: `${width - 3}px` };
    grid.style.gridTemplateColumns = pinned.columns;
    handle.style.left = pinned.left;
    handle.setAttribute("aria-valuenow", String(width));
  };

  handle.setPointerCapture(pointerId);
  handle.setAttribute("data-dragging", "");
  document.documentElement.setAttribute("data-sidebar-resizing", "");

  return {
    pointerId,
    move(clientX) {
      travel = Math.max(travel, Math.abs(clientX - startX));
      // Relative to where the handle was grabbed, so the edge does not
      // jump to the pointer on the first move.
      width = Math.min(max, Math.max(min, startWidth + Math.round(clientX - startX)));
      if (!frame) frame = window.requestAnimationFrame(paint);
    },
    end() {
      // The last move may still be waiting for its frame. It is not drawn:
      // its width is what gets saved and stamped below, and a frame drawn
      // after that would pin the edge again.
      if (frame) window.cancelAnimationFrame(frame);
      handle.removeAttribute("data-dragging");
      document.documentElement.removeAttribute("data-sidebar-resizing");
      // Only a drag that moved is saved. Committing a click would write
      // the clamped width over a preference this window cannot fit, and
      // widening the window would no longer bring it back.
      if (width !== startWidth) {
        useUi.getState().setSidebarWidth(width);
      } else {
        // Unsaved, the stamp is the preference, clamped to the window as it
        // is now — which, if it narrowed mid-drag, no longer allows what the
        // bounds measured at the press did.
        stampSidebarWidth(useUi.getState().sidebarWidth);
      }
      // With the width stamped, the edge follows the variable again — in
      // this same task, so no frame shows the two apart. Only what still
      // holds what the drag wrote goes back: going into the phone layout,
      // React clears the grid's style itself, and the grid is React's again.
      if (pinned?.columns === grid.style.gridTemplateColumns) {
        grid.style.gridTemplateColumns = rendered.columns;
      }
      if (pinned?.left === handle.style.left) {
        handle.style.left = rendered.left;
      }
      // The frames wrote aria-valuenow behind React's back too, and React
      // may have nothing new to render over it: a window that narrowed
      // mid-drag can leave it rendering the width it already rendered.
      const shown = effectiveSidebarWidth(useUi.getState().sidebarWidth, window.innerWidth);
      handle.setAttribute("aria-valuenow", String(shown));
      return travel >= DRAG_SLOP;
    },
  };
}

/**
 * The draggable right edge of the sidebar: a WAI-ARIA window splitter.
 *
 * Positioned against AppShell's grid, which is the containing block, on
 * the same --knot-sidebar-w the grid column reads. A drag moves the column
 * and the handle together, inline, without React rendering anything.
 */
export function SidebarResizeHandle({ controls }: { controls: string }) {
  const pref = useUi((s) => s.sidebarWidth);
  // Only the bounds need the exact window width, so only this small
  // component re-renders on resize; AppShell keeps to its three buckets.
  const viewportWidth = useSyncExternalStore(subscribeToResize, windowWidth);
  const { min, max } = sidebarWidthBounds(viewportWidth);
  const width = effectiveSidebarWidth(pref, viewportWidth);
  const drag = useRef<Drag | null>(null);
  // Whether the latest press dragged, rather than clicked. Pressing again
  // and dragging within the double-click interval still fires dblclick on
  // release, and a reset then would throw away the drag the user just made.
  const pressDragged = useRef(false);

  // A drag still in flight when the handle goes away — the window narrowed
  // into the mobile drawer mid-drag, say — ends as a release would, or the
  // whole document would be left with the resize cursor and no selection.
  useEffect(
    () => () => {
      const d = drag.current;
      drag.current = null;
      d?.end();
    },
    [],
  );

  function onPointerDown(e: PointerEvent<HTMLDivElement>) {
    // Primary button of the primary pointer, one drag at a time: a right-
    // click opens the context menu, and neither a second finger nor a pen
    // touching down mid-drag (primary for its own type) may start another.
    if (e.button !== 0 || !e.isPrimary || drag.current) return;
    pressDragged.current = false;
    drag.current = startDrag(e.currentTarget, e.pointerId, e.clientX);
  }

  function onPointerMove(e: PointerEvent<HTMLDivElement>) {
    if (drag.current?.pointerId === e.pointerId) drag.current.move(e.clientX);
  }

  function finish(e: PointerEvent<HTMLDivElement>) {
    const d = drag.current;
    if (d?.pointerId !== e.pointerId) return;
    drag.current = null;
    pressDragged.current = d.end();
  }

  function onDoubleClick() {
    if (!pressDragged.current) useUi.getState().setSidebarWidth(SIDEBAR_WIDTH_DEFAULT);
  }

  function onKeyDown(e: KeyboardEvent<HTMLDivElement>) {
    // Alt+← and ⌘+← are the browser's Back; nothing here should eat them.
    if (e.altKey || e.ctrlKey || e.metaKey) return;
    const step = e.shiftKey ? SHIFT_STEP : STEP;
    let next: number;
    switch (e.key) {
      case "ArrowLeft": next = width - step; break;
      case "ArrowRight": next = width + step; break;
      case "Home": next = min; break;
      case "End": next = max; break;
      default: return;
    }
    e.preventDefault();
    // Against a limit nothing moves, so nothing is saved: at a narrow
    // window's maximum, saving would write the clamped width over a wider
    // preference that widening the window should bring back.
    const clamped = Math.min(max, Math.max(min, next));
    if (clamped !== width) useUi.getState().setSidebarWidth(clamped);
  }

  return (
    <div
      role="separator"
      data-testid="sidebar-resize-handle"
      aria-orientation="vertical"
      aria-label="Resize sidebar"
      aria-controls={controls}
      aria-valuemin={min}
      aria-valuemax={max}
      aria-valuenow={width}
      tabIndex={0}
      // A press leaves focus where it was, so the editor, the title or a
      // comment keeps its caret and typing straight after a drag still
      // lands there; keyboard users reach the edge with Tab. Cancelled on
      // mousedown, not pointerdown: cancelling pointerdown would also
      // suppress the mousedown that closes the app's open menus.
      onMouseDown={(e) => e.preventDefault()}
      onPointerDown={onPointerDown}
      onPointerMove={onPointerMove}
      onPointerUp={finish}
      // A cancelled drag — the browser took the touch, or a tablet rotated
      // — and a capture lost to anything else end like a release: with the
      // width that is on screen, and the document's cursor given back.
      onPointerCancel={finish}
      onLostPointerCapture={finish}
      onKeyDown={onKeyDown}
      onDoubleClick={onDoubleClick}
      // 8px to grab, from 3px inside the edge (the tree's scrollbar lives
      // there) to 5px into the content — in px, like the offset and the
      // border, so a larger browser font cannot shift it off them.
      // touch-none keeps a touch drag from panning the page instead. With
      // keyboard focus it rises above the comment rail and the history
      // drawer (z-40), which can cover the edge in a narrow window.
      className="group absolute inset-y-0 z-10 w-[8px] cursor-col-resize touch-none select-none focus-visible:z-[41] focus-visible:outline-none"
      style={{ left: "calc(var(--knot-sidebar-w) - 3px)" }}
    >
      {/* Invisible at rest — the 1px border is the affordance — and an
          accent line over that border on hover, during a drag and on
          keyboard focus, where it is the focus indicator. A forced-colours
          theme would repaint the accent as Canvas and erase the border it
          covers, so there the line takes the system highlight colour. */}
      <span
        aria-hidden
        className="pointer-events-none absolute inset-y-0 left-[2px] w-[2px] bg-accent opacity-0 transition-opacity duration-150 ease-swift group-hover:opacity-100 group-focus-visible:opacity-100 group-data-[dragging]:opacity-100 forced-colors:bg-[Highlight]"
      />
    </div>
  );
}
