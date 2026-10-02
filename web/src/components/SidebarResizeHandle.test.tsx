import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { Profiler, StrictMode, type ReactNode } from "react";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi } from "../stores/ui";

import { SidebarResizeHandle } from "./SidebarResizeHandle";

// jsdom has PointerEvent but no pointer capture. This stand-in remembers
// which pointers each element holds, as a browser would, so the handle can
// capture and the tests can see that it did. Test-only: production code
// calls the real API.
const captures = new WeakMap<Element, Set<number>>();
beforeAll(() => {
  Element.prototype.setPointerCapture = function (this: Element, id: number) {
    const held = captures.get(this) ?? new Set<number>();
    held.add(id);
    captures.set(this, held);
  };
  Element.prototype.releasePointerCapture = function (this: Element, id: number) {
    captures.get(this)?.delete(id);
  };
  Element.prototype.hasPointerCapture = function (this: Element, id: number) {
    return captures.get(this)?.has(id) ?? false;
  };
});
afterAll(() => {
  const proto = Element.prototype as Partial<Element>;
  delete proto.setPointerCapture;
  delete proto.releasePointerCapture;
  delete proto.hasPointerCapture;
});

const realWidth = window.innerWidth;

function setViewportWidth(w: number) {
  Object.defineProperty(window, "innerWidth", { value: w, configurable: true, writable: true });
}

/** What the grid and layout.css read: the width the sidebar renders at. */
function stampedWidth() {
  return document.documentElement.style.getPropertyValue("--knot-sidebar-w");
}

/** Where the handle lives in AppShell: straight inside the grid, whose
 *  sidebar column follows the stamped width. The same inline style. */
function Grid({ children }: { children: ReactNode }) {
  return (
    <div
      data-testid="grid"
      className="relative grid"
      style={{ gridTemplateColumns: "var(--knot-sidebar-w) 1fr" }}
    >
      {children}
    </div>
  );
}

function renderHandle() {
  render(
    <Grid>
      <SidebarResizeHandle controls="app-sidebar" />
    </Grid>,
  );
  return screen.getByTestId("sidebar-resize-handle");
}

/** Where the edge is drawn: the grid's sidebar column, and the handle's
 *  offset against the grid. Both are inline styles. */
function edge() {
  return {
    columns: screen.getByTestId("grid").style.gridTemplateColumns,
    left: screen.getByTestId("sidebar-resize-handle").style.left,
  };
}

/** The edge as AppShell and the handle render it: on the stamped width. */
const ON_THE_STAMP = {
  columns: "var(--knot-sidebar-w) 1fr",
  left: "calc(var(--knot-sidebar-w) - 3px)",
};

/** Counts the writes to <html>'s inline style, where --knot-sidebar-w is
 *  stamped. */
function watchRootStyle() {
  let writes = 0;
  const observer = new MutationObserver((records) => {
    writes += records.length;
  });
  observer.observe(document.documentElement, { attributeFilter: ["style"] });
  return {
    writes() {
      writes += observer.takeRecords().length;
      return writes;
    },
    stop() {
      observer.disconnect();
    },
  };
}

const mouse = { pointerId: 1, pointerType: "mouse", isPrimary: true, button: 0 };

function press(el: Element, clientX: number, init: PointerEventInit = {}) {
  fireEvent.pointerDown(el, { ...mouse, clientX, buttons: 1, ...init });
}

/** With the pointer captured, a browser delivers every move to the handle,
 *  wherever the pointer is — so the tests dispatch them there too. */
function moveTo(el: Element, clientX: number, init: PointerEventInit = {}) {
  fireEvent.pointerMove(el, { ...mouse, clientX, button: -1, buttons: 1, ...init });
}

function release(el: Element, clientX: number, init: PointerEventInit = {}) {
  fireEvent.pointerUp(el, { ...mouse, clientX, buttons: 0, ...init });
}

function nextFrame() {
  act(() => {
    vi.advanceTimersToNextFrame();
  });
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.style.removeProperty("--knot-sidebar-w");
  setViewportWidth(1280);
  useUi.setState({ sidebarWidth: 260 });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  setViewportWidth(realWidth);
});

describe("SidebarResizeHandle", () => {
  it("is a focusable vertical separator that controls the sidebar", () => {
    renderHandle();
    const handle = screen.getByRole("separator", { name: "Resize sidebar" });
    expect(handle).toHaveAttribute("data-testid", "sidebar-resize-handle");
    expect(handle).toHaveAttribute("aria-orientation", "vertical");
    expect(handle).toHaveAttribute("aria-controls", "app-sidebar");
    expect(handle).toHaveAttribute("tabindex", "0");
  });

  it("reports the rendered width and the room this window allows", () => {
    const handle = renderHandle();
    expect(handle).toHaveAttribute("aria-valuemin", "200");
    expect(handle).toHaveAttribute("aria-valuemax", "480");
    expect(handle).toHaveAttribute("aria-valuenow", "260");
  });

  it("reports the clamped width when the window is too narrow for the preference", () => {
    setViewportWidth(800);
    useUi.setState({ sidebarWidth: 480 });
    const handle = renderHandle();
    expect(handle).toHaveAttribute("aria-valuemax", "420");
    expect(handle).toHaveAttribute("aria-valuenow", "420");
  });

  it("keeps its range current as the window resizes", () => {
    useUi.setState({ sidebarWidth: 480 });
    const handle = renderHandle();
    expect(handle).toHaveAttribute("aria-valuenow", "480");

    setViewportWidth(800);
    act(() => {
      window.dispatchEvent(new Event("resize"));
    });
    expect(handle).toHaveAttribute("aria-valuemax", "420");
    expect(handle).toHaveAttribute("aria-valuenow", "420");
  });

  it("leaves focus where it was when pressed, so typing after a drag still reaches the editor", () => {
    const handle = renderHandle();
    // A browser moves focus as mousedown's default action, which jsdom does
    // not model; what can be checked here is that the handle cancels it.
    // fireEvent returns false when a handler called preventDefault().
    expect(fireEvent.mouseDown(handle, { button: 0 })).toBe(false);
  });

  it("lets a press through to the app's outside-click handlers", () => {
    const handle = renderHandle();
    // Cancelling pointerdown would suppress the mousedown that follows it,
    // which is what closes an open context menu or toolbar popover.
    expect(fireEvent.pointerDown(handle, { ...mouse, clientX: 260, buttons: 1 })).toBe(true);
  });

  describe("keyboard", () => {
    it.each([
      ["ArrowRight", false, 276],
      ["ArrowLeft", false, 244],
      ["ArrowRight", true, 324],
      ["ArrowLeft", true, 200], // 260 - 64 = 196, held at the minimum
      ["Home", false, 200],
      ["End", false, 480],
    ])("%s (shift: %s) moves a 260px sidebar to %ipx and saves it", (key, shiftKey, want) => {
      const handle = renderHandle();
      fireEvent.keyDown(handle, { key, shiftKey });
      expect(useUi.getState().sidebarWidth).toBe(want);
      expect(localStorage.getItem("knot.sidebarWidth")).toBe(String(want));
      expect(stampedWidth()).toBe(`${want}px`);
      expect(handle).toHaveAttribute("aria-valuenow", String(want));
    });

    it("holds a step at the maximum", () => {
      useUi.setState({ sidebarWidth: 470 });
      const handle = renderHandle();
      fireEvent.keyDown(handle, { key: "ArrowRight" });
      expect(useUi.getState().sidebarWidth).toBe(480);
    });

    it("ends at the widest this window allows", () => {
      setViewportWidth(800);
      const handle = renderHandle();
      fireEvent.keyDown(handle, { key: "End" });
      expect(useUi.getState().sidebarWidth).toBe(420);
    });

    it("steps from the width on screen, not from a preference the window cannot fit", () => {
      setViewportWidth(800);
      useUi.setState({ sidebarWidth: 480 }); // renders at 420
      const handle = renderHandle();
      fireEvent.keyDown(handle, { key: "ArrowLeft" });
      expect(useUi.getState().sidebarWidth).toBe(404);
    });

    it.each([
      ["ArrowRight", false],
      ["ArrowRight", true],
      ["End", false],
    ])(
      "keeps a wider saved width when %s (shift: %s) pushes against a narrow window's maximum",
      (key, shiftKey) => {
        // Nothing can move on screen, so nothing is saved: widening the
        // window again still brings the 480 back.
        setViewportWidth(800);
        localStorage.setItem("knot.sidebarWidth", "480");
        useUi.setState({ sidebarWidth: 480 }); // renders at 420, the maximum
        const handle = renderHandle();
        expect(fireEvent.keyDown(handle, { key, shiftKey })).toBe(false);
        expect(useUi.getState().sidebarWidth).toBe(480);
        expect(localStorage.getItem("knot.sidebarWidth")).toBe("480");
      },
    );

    it("does not raise the saved width past a narrow window's maximum", () => {
      setViewportWidth(800);
      useUi.setState({ sidebarWidth: 420 });
      const handle = renderHandle();
      fireEvent.keyDown(handle, { key: "ArrowRight" });
      expect(useUi.getState().sidebarWidth).toBe(420);
    });

    it("takes over the keys it handles", () => {
      const handle = renderHandle();
      // fireEvent returns false when a handler called preventDefault().
      expect(fireEvent.keyDown(handle, { key: "ArrowRight" })).toBe(false);
      expect(fireEvent.keyDown(handle, { key: "Home" })).toBe(false);
    });

    it.each([
      [{ key: "ArrowUp" }],
      [{ key: "a" }],
      [{ key: "Tab" }],
      [{ key: "ArrowLeft", altKey: true }], // Back, on Windows and Linux
      [{ key: "ArrowLeft", metaKey: true }], // Back, on macOS
      [{ key: "ArrowRight", ctrlKey: true }],
    ])("leaves %j to the browser", (init) => {
      const handle = renderHandle();
      expect(fireEvent.keyDown(handle, init)).toBe(true);
      expect(useUi.getState().sidebarWidth).toBe(260);
      expect(localStorage.getItem("knot.sidebarWidth")).toBeNull();
    });
  });

  it("resets to the default width on double-click, and saves it", () => {
    useUi.setState({ sidebarWidth: 400 });
    const handle = renderHandle();
    fireEvent.doubleClick(handle);
    expect(useUi.getState().sidebarWidth).toBe(260);
    expect(localStorage.getItem("knot.sidebarWidth")).toBe("260");
    expect(stampedWidth()).toBe("260px");
  });

  describe("dragging", () => {
    beforeEach(() => {
      vi.useFakeTimers({ toFake: ["requestAnimationFrame", "cancelAnimationFrame"] });
    });

    afterEach(() => {
      vi.useRealTimers();
      document.documentElement.removeAttribute("data-sidebar-resizing");
    });

    it("moves the edge with the pointer", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 300);
      nextFrame();
      // The grid's sidebar column, and the handle 3px inside its edge.
      expect(edge()).toEqual({ columns: "300px 1fr", left: "297px" });
      expect(handle).toHaveAttribute("aria-valuenow", "300");
    });

    it("leaves --knot-sidebar-w alone for the whole drag, and stamps it once, on release", () => {
      // Every element inherits the variable: stamping it each frame would
      // restyle the whole document each frame. Stamped first, as main.tsx
      // and AppShell leave it for a 260px sidebar.
      document.documentElement.style.setProperty("--knot-sidebar-w", "260px");
      const handle = renderHandle();
      const root = watchRootStyle();
      press(handle, 260);
      for (let x = 270; x <= 400; x += 10) {
        moveTo(handle, x);
        nextFrame();
      }
      expect(root.writes()).toBe(0);
      expect(stampedWidth()).toBe("260px");
      expect(edge().columns).toBe("400px 1fr");

      release(handle, 400);
      expect(root.writes()).toBe(1);
      expect(stampedWidth()).toBe("400px");
      root.stop();
    });

    it("follows the pointer from where it was grabbed, without jumping", () => {
      const handle = renderHandle();
      press(handle, 257); // 3px inside the edge, still on the handle
      moveTo(handle, 297);
      nextFrame();
      expect(edge().columns).toBe("300px 1fr");
    });

    it("in a narrow window, starts from the width on screen", () => {
      setViewportWidth(800);
      useUi.setState({ sidebarWidth: 480 }); // renders at 420
      const handle = renderHandle();
      press(handle, 420);
      moveTo(handle, 390);
      nextFrame();
      expect(edge().columns).toBe("390px 1fr");
    });

    it("paints whole pixels for a fractional pointer position", () => {
      // Trackpads, zoom and hi-DPI screens report fractional clientX.
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 300.5);
      nextFrame();
      expect(edge()).toEqual({ columns: "301px 1fr", left: "298px" });
      expect(handle).toHaveAttribute("aria-valuenow", "301");
    });

    it("writes once a frame, with the latest position", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 280);
      moveTo(handle, 290);
      moveTo(handle, 300);
      expect(edge()).toEqual(ON_THE_STAMP);
      nextFrame();
      expect(edge()).toEqual({ columns: "300px 1fr", left: "297px" });
    });

    it("saves the width once, on release", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 300);
      nextFrame();
      expect(useUi.getState().sidebarWidth).toBe(260);
      expect(localStorage.getItem("knot.sidebarWidth")).toBeNull();

      release(handle, 300);
      expect(useUi.getState().sidebarWidth).toBe(300);
      expect(localStorage.getItem("knot.sidebarWidth")).toBe("300");
      expect(stampedWidth()).toBe("300px");
    });

    it("hands the edge back to the stamped width on release, exactly as it was rendered", () => {
      // React re-applies an inline style only when its prop changes, and
      // these never do: anything else left here would hold the edge for
      // good, and a window resize could no longer clamp the sidebar.
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 300);
      nextFrame();
      release(handle, 300);
      expect(stampedWidth()).toBe("300px");
      expect(edge()).toEqual(ON_THE_STAMP);
    });

    it("paints nothing after release, though a move was still waiting for its frame", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 300);
      nextFrame();
      moveTo(handle, 320); // released before this frame is drawn
      release(handle, 320);
      expect(useUi.getState().sidebarWidth).toBe(320);
      expect(stampedWidth()).toBe("320px");
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(vi.getTimerCount()).toBe(0);
      nextFrame();
      expect(edge()).toEqual(ON_THE_STAMP);
    });

    it("neither re-renders nor notifies the store until release", () => {
      const commits = vi.fn();
      render(
        <Grid>
          <Profiler id="handle" onRender={commits}>
            <SidebarResizeHandle controls="app-sidebar" />
          </Profiler>
        </Grid>,
      );
      const handle = screen.getByTestId("sidebar-resize-handle");
      const notifications = vi.fn();
      const unsubscribe = useUi.subscribe(notifications);
      commits.mockClear();

      press(handle, 260);
      for (let x = 270; x <= 400; x += 10) {
        moveTo(handle, x);
        nextFrame();
      }
      expect(commits).not.toHaveBeenCalled();
      expect(notifications).not.toHaveBeenCalled();

      release(handle, 400);
      expect(notifications).toHaveBeenCalledTimes(1);
      unsubscribe();
    });

    it.each([
      [1280, 2000, 480],
      [1280, -100, 200],
      [800, 2000, 420],
    ])("in a %ipx window, dragging to x=%i stops at %ipx", (vw, x, want) => {
      setViewportWidth(vw);
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, x);
      nextFrame();
      expect(edge().columns).toBe(`${want}px 1fr`);
      release(handle, x);
      expect(useUi.getState().sidebarWidth).toBe(want);
    });

    it("keeps the pointer once it leaves the handle", () => {
      const handle = renderHandle();
      press(handle, 260);
      expect(handle.hasPointerCapture(1)).toBe(true);
    });

    it("holds the resize cursor and blocks text selection until release", () => {
      const handle = renderHandle();
      press(handle, 260);
      expect(document.documentElement).toHaveAttribute("data-sidebar-resizing");
      expect(handle).toHaveAttribute("data-dragging");
      moveTo(handle, 600); // far off the handle, over the editor
      expect(document.documentElement).toHaveAttribute("data-sidebar-resizing");

      release(handle, 600);
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
      expect(handle).not.toHaveAttribute("data-dragging");
    });

    it("ignores a press with any button but the primary one", () => {
      const handle = renderHandle();
      press(handle, 260, { button: 2 });
      moveTo(handle, 400);
      nextFrame();
      release(handle, 400, { button: 2 });
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(stampedWidth()).toBe("");
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
      expect(useUi.getState().sidebarWidth).toBe(260);
    });

    it("ignores a second finger", () => {
      const handle = renderHandle();
      press(handle, 260, { pointerId: 2, pointerType: "touch", isPrimary: false });
      moveTo(handle, 400, { pointerId: 2, pointerType: "touch", isPrimary: false });
      nextFrame();
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(stampedWidth()).toBe("");
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
    });

    it("ignores another pointer while dragging", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 400, { pointerId: 2, isPrimary: false });
      nextFrame();
      expect(edge()).toEqual(ON_THE_STAMP);

      release(handle, 400, { pointerId: 2, isPrimary: false });
      expect(document.documentElement).toHaveAttribute("data-sidebar-resizing");
    });

    it("keeps the first drag when a pen touches down mid-drag", () => {
      // Primacy is per pointer type, so a pen's first contact is primary too.
      const handle = renderHandle();
      press(handle, 260);
      press(handle, 500, { pointerId: 2, pointerType: "pen" });
      moveTo(handle, 300);
      nextFrame();
      expect(edge().columns).toBe("300px 1fr");
    });

    it("ends a cancelled drag with the width it shows", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 320);
      nextFrame();
      fireEvent.pointerCancel(handle, mouse);
      expect(useUi.getState().sidebarWidth).toBe(320);
      expect(localStorage.getItem("knot.sidebarWidth")).toBe("320");
      expect(stampedWidth()).toBe("320px");
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
      expect(handle).not.toHaveAttribute("data-dragging");
    });

    it("ends the drag with the width it shows when the pointer capture is lost", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 320);
      nextFrame();
      fireEvent.lostPointerCapture(handle, mouse);
      expect(useUi.getState().sidebarWidth).toBe(320);
      expect(localStorage.getItem("knot.sidebarWidth")).toBe("320");
      expect(stampedWidth()).toBe("320px");
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
    });

    it("leaves the saved width alone for a click that does not move", () => {
      setViewportWidth(800);
      useUi.setState({ sidebarWidth: 480 }); // renders at 420
      const handle = renderHandle();
      press(handle, 420);
      release(handle, 420);
      expect(useUi.getState().sidebarWidth).toBe(480);
      expect(localStorage.getItem("knot.sidebarWidth")).toBeNull();
    });

    it("puts the edge back when the pointer returns to where it started", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 300);
      nextFrame();
      moveTo(handle, 260); // released before this frame is drawn
      release(handle, 260);
      expect(stampedWidth()).toBe("260px");
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(handle).toHaveAttribute("aria-valuenow", "260");
      expect(localStorage.getItem("knot.sidebarWidth")).toBeNull();
    });

    it.each([
      // [released at x, saved width afterwards]
      [480, 480], // back where it started: nothing to save
      [465, 465],
    ])(
      "released at x=%i after the window narrowed mid-drag, shows what the window allows",
      (releaseX, saved) => {
        useUi.setState({ sidebarWidth: 480 });
        const handle = renderHandle();
        press(handle, 480);
        moveTo(handle, 460);
        nextFrame();
        // Window tiling or a docked DevTools panel, with the button held.
        // The drag keeps the bounds it measured at the press until it ends.
        setViewportWidth(800);
        act(() => {
          window.dispatchEvent(new Event("resize"));
        });
        moveTo(handle, releaseX);
        nextFrame();
        release(handle, releaseX);
        expect(useUi.getState().sidebarWidth).toBe(saved);
        // maxFor(800) = 420: the content column keeps its 380px.
        expect(stampedWidth()).toBe("420px");
        expect(edge()).toEqual(ON_THE_STAMP);
        expect(handle).toHaveAttribute("aria-valuenow", "420");
      },
    );

    it("gives the document back when it unmounts mid-drag, keeping the width shown", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 320);
      cleanup();
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
      expect(useUi.getState().sidebarWidth).toBe(320);
      expect(stampedWidth()).toBe("320px");
      expect(vi.getTimerCount()).toBe(0); // no frame left to paint a dead handle
    });

    it("hands the column back when it unmounts mid-drag and the grid stays", () => {
      // AppShell drops the handle but keeps the grid when the sidebar closes.
      const { rerender } = render(
        <Grid>
          <SidebarResizeHandle controls="app-sidebar" />
        </Grid>,
      );
      const grid = screen.getByTestId("grid");
      const handle = screen.getByTestId("sidebar-resize-handle");
      press(handle, 260);
      moveTo(handle, 320);
      nextFrame();
      rerender(<Grid>{null}</Grid>);
      expect(grid.style.gridTemplateColumns).toBe("var(--knot-sidebar-w) 1fr");
      expect(stampedWidth()).toBe("320px");
      expect(useUi.getState().sidebarWidth).toBe(320);
    });

    it("does not reset a width that the second press of a double-click dragged", () => {
      const handle = renderHandle();
      press(handle, 260);
      release(handle, 260);
      press(handle, 260);
      moveTo(handle, 340);
      nextFrame();
      release(handle, 340);
      fireEvent.doubleClick(handle);
      expect(useUi.getState().sidebarWidth).toBe(340);
    });

    it.each([1, -1, 0.5])(
      "still resets when the second press of a double-click wobbles by %spx",
      (wobble) => {
        // A pixel of hand tremor between press and release, which a
        // trackpad click easily has: the browser still fires dblclick.
        useUi.setState({ sidebarWidth: 400 });
        const handle = renderHandle();
        press(handle, 400);
        release(handle, 400);
        press(handle, 400);
        moveTo(handle, 400 + wobble);
        nextFrame();
        release(handle, 400 + wobble);
        fireEvent.doubleClick(handle);
        expect(useUi.getState().sidebarWidth).toBe(260);
        expect(localStorage.getItem("knot.sidebarWidth")).toBe("260");
      },
    );

    it("still resets on a double-click after an earlier drag", () => {
      const handle = renderHandle();
      press(handle, 260);
      moveTo(handle, 340);
      nextFrame();
      release(handle, 340);
      press(handle, 340);
      release(handle, 340);
      press(handle, 340);
      release(handle, 340);
      fireEvent.doubleClick(handle);
      expect(useUi.getState().sidebarWidth).toBe(260);
    });

    it("drags under StrictMode's double mount", () => {
      render(
        <StrictMode>
          <Grid>
            <SidebarResizeHandle controls="app-sidebar" />
          </Grid>
        </StrictMode>,
      );
      const handle = screen.getByTestId("sidebar-resize-handle");
      press(handle, 260);
      moveTo(handle, 300);
      nextFrame();
      expect(edge()).toEqual({ columns: "300px 1fr", left: "297px" });
      release(handle, 300);
      expect(useUi.getState().sidebarWidth).toBe(300);
      expect(edge()).toEqual(ON_THE_STAMP);
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
    });
  });
});
