import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { useLayoutEffect } from "react";
import { MemoryRouter } from "react-router-dom";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi } from "../stores/ui";

import { AppShell } from "./AppShell";

// The tree fetches documents and reads the session, and the palette runs
// searches on timers; neither has anything to do with how the shell lays
// itself out, which is all these tests look at.
vi.mock("../features/docs/DocTree", () => ({ DocTree: () => null }));
vi.mock("./CommandPalette", () => ({ CommandPalette: () => null }));

const realWidth = window.innerWidth;

function setViewportWidth(w: number) {
  Object.defineProperty(window, "innerWidth", { value: w, configurable: true, writable: true });
}

/** What the grid and layout.css read: the width the sidebar renders at. */
function stampedWidth() {
  return document.documentElement.style.getPropertyValue("--knot-sidebar-w");
}

function renderShell() {
  render(
    <MemoryRouter>
      <AppShell />
    </MemoryRouter>,
  );
  const sidebar = screen.getByTestId("sidebar");
  return { sidebar, grid: sidebar.parentElement! };
}

function fromAnotherTab(key: string, newValue: string | null) {
  act(() => {
    window.dispatchEvent(new StorageEvent("storage", { key, newValue }));
  });
}

function resizeWindow(w: number) {
  setViewportWidth(w);
  act(() => {
    window.dispatchEvent(new Event("resize"));
  });
}

beforeEach(() => {
  localStorage.clear();
  document.documentElement.style.removeProperty("--knot-sidebar-w");
  setViewportWidth(1280);
  useUi.setState({ sidebarOpen: true, sidebarWidth: 260 });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  setViewportWidth(realWidth);
});

describe("AppShell sidebar", () => {
  it("gives the sidebar a resize handle that controls it", () => {
    const { sidebar } = renderShell();
    expect(sidebar).toHaveAttribute("id", "app-sidebar");
    expect(screen.getByRole("separator", { name: "Resize sidebar" })).toHaveAttribute(
      "aria-controls",
      "app-sidebar",
    );
  });

  it("sizes the sidebar column from the stamped width, not a fixed one", () => {
    const { grid } = renderShell();
    expect(grid.style.gridTemplateColumns).toBe("var(--knot-sidebar-w) 1fr");
  });

  it("has no handle on a phone, where the sidebar is a fixed-width drawer", () => {
    setViewportWidth(500);
    renderShell();
    expect(screen.queryByTestId("sidebar-resize-handle")).toBeNull();
  });

  it("has no handle while the sidebar is closed, and gives its column away", () => {
    useUi.setState({ sidebarOpen: false });
    const { grid } = renderShell();
    expect(screen.queryByTestId("sidebar-resize-handle")).toBeNull();
    // Zero for the grid, and for layout.css, which then knows the content
    // column has the whole window.
    expect(grid.style.getPropertyValue("--knot-sidebar-w")).toBe("0px");
  });

  it("stamps the width this window allows when it mounts", () => {
    // Stamped before first paint for a wider window; the user then resized
    // it on the login page, where no shell was listening.
    document.documentElement.style.setProperty("--knot-sidebar-w", "480px");
    useUi.setState({ sidebarWidth: 480 });
    setViewportWidth(800);
    renderShell();
    expect(stampedWidth()).toBe("420px");
  });

  it("corrects a stale stamp in its mount commit, before the browser can paint it", () => {
    document.documentElement.style.setProperty("--knot-sidebar-w", "480px");
    useUi.setState({ sidebarWidth: 480 });
    setViewportWidth(800);
    // A later sibling's layout effect runs after the shell's own and before
    // any passive effect, so what it reads is what the first frame shows.
    let firstFrame = "";
    function FirstFrame() {
      useLayoutEffect(() => {
        firstFrame = stampedWidth();
      }, []);
      return null;
    }
    render(
      <MemoryRouter>
        <AppShell />
        <FirstFrame />
      </MemoryRouter>,
    );
    expect(firstFrame).toBe("420px");
  });

  it("picks up a width another tab saved while no shell was listening", () => {
    // This tab read 260 when it loaded, then sat on the login page while
    // another tab saved 480; the storage event went to no one.
    localStorage.setItem("knot.sidebarWidth", "480");
    renderShell();
    expect(useUi.getState().sidebarWidth).toBe(480);
    expect(stampedWidth()).toBe("480px");
    expect(screen.getByTestId("sidebar-resize-handle")).toHaveAttribute("aria-valuenow", "480");
  });

  it("keeps this session's width when storage cannot be read", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError: storage is disabled");
    });
    useUi.setState({ sidebarWidth: 340 });
    renderShell();
    expect(useUi.getState().sidebarWidth).toBe(340);
    expect(stampedWidth()).toBe("340px");
  });

  it("re-stamps as the window resizes, and a wider window brings the saved width back", () => {
    useUi.setState({ sidebarWidth: 480 });
    renderShell();

    resizeWindow(800);
    expect(stampedWidth()).toBe("420px");

    resizeWindow(1280);
    expect(stampedWidth()).toBe("480px");
  });

  it("keeps a width chosen after mount through a window resize", () => {
    renderShell();
    act(() => useUi.getState().setSidebarWidth(340));
    resizeWindow(1200);
    expect(stampedWidth()).toBe("340px");
  });

  it("applies a width saved in another tab without writing it back", () => {
    renderShell();
    fromAnotherTab("knot.sidebarWidth", "340");
    expect(useUi.getState().sidebarWidth).toBe(340);
    expect(stampedWidth()).toBe("340px");
    expect(localStorage.getItem("knot.sidebarWidth")).toBeNull();
  });

  it.each([
    ["wide", 260],
    [null, 260], // the key was removed over there
    ["9999", 480],
  ])("validates a width from another tab: %j becomes %ipx", (newValue, want) => {
    useUi.setState({ sidebarWidth: 340 });
    renderShell();
    fromAnotherTab("knot.sidebarWidth", newValue);
    expect(useUi.getState().sidebarWidth).toBe(want);
    expect(stampedWidth()).toBe(`${want}px`);
  });

  it("ignores another tab's other keys", () => {
    useUi.setState({ sidebarWidth: 340 });
    renderShell();
    fromAnotherTab("knot.skin", "nord");
    expect(useUi.getState().sidebarWidth).toBe(340);
    expect(stampedWidth()).toBe("340px");
  });

  it("still mirrors the document width mode from another tab", () => {
    useUi.setState({ docWidth: "fixed" });
    renderShell();
    fromAnotherTab("knot.docWidth", "wide");
    expect(useUi.getState().docWidth).toBe("wide");
    expect(document.documentElement).toHaveAttribute("data-doc-width", "wide");
    expect(useUi.getState().sidebarWidth).toBe(260);
  });
});

describe("AppShell sidebar, dragging its edge", () => {
  // jsdom has no pointer capture; the handle only needs to be able to ask.
  beforeAll(() => {
    Element.prototype.setPointerCapture = () => {};
    Element.prototype.releasePointerCapture = () => {};
  });
  afterAll(() => {
    const proto = Element.prototype as Partial<Element>;
    delete proto.setPointerCapture;
    delete proto.releasePointerCapture;
  });

  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["requestAnimationFrame", "cancelAnimationFrame"] });
  });

  afterEach(() => {
    vi.useRealTimers();
    document.documentElement.removeAttribute("data-sidebar-resizing");
  });

  const mouse = { pointerId: 1, pointerType: "mouse", isPrimary: true, button: 0 };
  const handle = () => screen.getByTestId("sidebar-resize-handle");

  function press(clientX: number) {
    fireEvent.pointerDown(handle(), { ...mouse, clientX, buttons: 1 });
  }

  /** The pointer is captured, so a browser delivers every move here. */
  function moveTo(clientX: number) {
    fireEvent.pointerMove(handle(), { ...mouse, clientX, button: -1, buttons: 1 });
  }

  function nextFrame() {
    act(() => {
      vi.advanceTimersToNextFrame();
    });
  }

  function release(clientX: number) {
    fireEvent.pointerUp(handle(), { ...mouse, clientX, buttons: 0 });
  }

  it("sizes the grid's own sidebar column, and hands it back to the stamped width on release", () => {
    const { grid } = renderShell();
    press(260);
    moveTo(340);
    nextFrame();
    expect(grid.style.gridTemplateColumns).toBe("340px 1fr");
    expect(stampedWidth()).toBe("260px");

    release(340);
    expect(stampedWidth()).toBe("340px");
    expect(grid.style.gridTemplateColumns).toBe("var(--knot-sidebar-w) 1fr");
  });

  it("keeps the edge under the pointer when the window resizes mid-drag", () => {
    useUi.setState({ sidebarWidth: 480 });
    const { grid } = renderShell();
    press(480);
    moveTo(470);
    nextFrame();
    // The shell re-stamps for the narrower window as always; the column the
    // drag holds does not follow it.
    resizeWindow(800);
    expect(stampedWidth()).toBe("420px");
    expect(grid.style.gridTemplateColumns).toBe("470px 1fr");

    release(470);
    // Saved as dragged, shown as this window allows.
    expect(useUi.getState().sidebarWidth).toBe(470);
    expect(stampedWidth()).toBe("420px");
    expect(grid.style.gridTemplateColumns).toBe("var(--knot-sidebar-w) 1fr");
  });

  it.each([
    // [the drag when the layout switches, a move not yet drawn, width saved]
    ["its last move drawn", null, 320],
    ["a move still waiting for its frame", 330, 330],
  ])(
    "leaves the grid to React when the window narrows into the phone layout mid-drag, with %s",
    (_, waiting, saved) => {
      const { grid } = renderShell();
      press(260);
      moveTo(320);
      nextFrame();
      if (waiting !== null) moveTo(waiting);

      resizeWindow(500);
      // The drawer layout has no columns: React cleared the grid's inline
      // style, and the drag, ended by the handle unmounting, puts nothing
      // back over it.
      expect(screen.queryByTestId("sidebar-resize-handle")).toBeNull();
      expect(grid.style.gridTemplateColumns).toBe("");
      expect(document.documentElement).not.toHaveAttribute("data-sidebar-resizing");
      expect(useUi.getState().sidebarWidth).toBe(saved);

      // Back in a desktop window, the shell is drawn from the stamp as usual.
      resizeWindow(1280);
      expect(stampedWidth()).toBe(`${saved}px`);
      expect(grid.style.gridTemplateColumns).toBe("var(--knot-sidebar-w) 1fr");
      expect(handle().style.left).toBe("calc(var(--knot-sidebar-w) - 3px)");
      expect(handle()).toHaveAttribute("aria-valuenow", String(saved));
    },
  );
});
