import { Menu } from "lucide-react";
import { useEffect, useLayoutEffect, type CSSProperties } from "react";
import { Outlet } from "react-router-dom";

import { WhatsNewCard } from "../features/about/WhatsNewCard";
import { DocTree } from "../features/docs/DocTree";
import { useViewport } from "../hooks/useViewport";
import { parseSidebarWidth, stampSidebarWidth, useUi } from "../stores/ui";

import { CommandPalette } from "./CommandPalette";
import { SidebarResizeHandle } from "./SidebarResizeHandle";
import { Toast } from "./Toast";

const SIDEBAR_ID = "app-sidebar";

export function AppShell() {
  const sidebarOpen = useUi((s) => s.sidebarOpen);
  const toggleSidebar = useUi((s) => s.toggleSidebar);
  const vp = useViewport();
  const mobile = vp === "mobile";

  // Mirror the width preferences across tabs. setState rather than the
  // persisting setters: writing back into storage from a storage event
  // is a needless round trip, and this way there is no echo to reason about.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key === "knot.docWidth") {
        const next = e.newValue === "wide" ? "wide" : "fixed";
        document.documentElement.setAttribute("data-doc-width", next);
        useUi.setState({ docWidth: next });
      } else if (e.key === "knot.sidebarWidth") {
        const next = parseSidebarWidth(e.newValue);
        stampSidebarWidth(next);
        useUi.setState({ sidebarWidth: next });
      }
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  // The stamped sidebar width is the preference clamped to the window, so
  // it is re-derived whenever the window changes size — straight onto the
  // variable the grid reads, with no React state and no re-render. Also on
  // mount: main.tsx stamped it for the window as it was then, which a
  // resize on the login page, where nothing was listening, has since made
  // stale. A layout effect, so that correction lands before first paint.
  useLayoutEffect(() => {
    // Nothing heard another tab saving a width meanwhile either, so the
    // saved one is read again. Only a saved one: with none, or no storage
    // at all, the width this session already has stands.
    try {
      const saved = localStorage.getItem("knot.sidebarWidth");
      if (saved !== null) useUi.setState({ sidebarWidth: parseSidebarWidth(saved) });
    } catch {
      /* storage unavailable */
    }
    const restamp = () => stampSidebarWidth(useUi.getState().sidebarWidth);
    restamp();
    window.addEventListener("resize", restamp);
    return () => window.removeEventListener("resize", restamp);
  }, []);

  // The sidebar column reads --knot-sidebar-w, stamped on <html>. Closed, the
  // sidebar takes no column; zeroing the variable here rather than in the
  // template also tells layout.css the content column has the whole window.
  const gridStyle = {
    gridTemplateColumns: "var(--knot-sidebar-w) 1fr",
    ...(sidebarOpen ? {} : { "--knot-sidebar-w": "0px" }),
  } as CSSProperties;

  return (
    <div
      // Relative: the resize handle is positioned against the grid.
      //
      // overflow-clip: a backstop for #87. The shell is exactly the window,
      // so anything painting past it would only grow the root's scrollable
      // overflow and give the window a scrollbar of its own over two panes
      // that already scroll. The panes below are the real fix: they contain
      // their own absolutely positioned content. Clip, not hidden: hidden
      // would make the shell a scroll container that focus() or
      // scrollIntoView() could still shift, with no scrollbar to undo it.
      // It does not clip the position:fixed children (toast, palette,
      // drawer, comment rail), whose containing block is the viewport, nor
      // the resize handle, which sits inside the grid's box.
      className={`h-dvh overflow-clip font-sans text-fg ${mobile ? "block" : "relative grid"}`}
      style={mobile ? undefined : gridStyle}
    >
      {mobile && !sidebarOpen && (
        <button
          type="button"
          data-testid="menu-toggle"
          onClick={toggleSidebar}
          aria-label="Open menu"
          className="fixed top-3 left-3 z-30 h-9 w-9 rounded border border-border bg-surface text-fg shadow-sm hover:bg-muted transition-colors ease-swift duration-150 flex items-center justify-center"
        >
          <Menu size={18} aria-hidden />
        </button>
      )}
      {mobile && sidebarOpen && (
        <div
          data-testid="sidebar-backdrop"
          onClick={toggleSidebar}
          className="fixed inset-0 bg-black/40 z-20 backdrop-blur-sm"
        />
      )}
      {/* Relative on a desktop, as hardening for the same reason as <main>
          below (#87): any absolutely positioned descendant without a
          positioned host of its own then scrolls, and is clipped, with the
          tree instead of overflowing the window. Nothing in the tree does
          that today: its live region is a child of the flex column, and an
          absolutely positioned flex child sits where it would as the only
          item, at the top; the rows that carry sr-only text are relative
          themselves. The phone drawer is fixed, which already makes it the
          containing block. */}
      <aside
        id={SIDEBAR_ID}
        data-testid="sidebar"
        className={`bg-bg border-r border-border overflow-y-auto ${
          mobile
            ? `fixed top-0 h-dvh w-[260px] z-30 transition-[left] duration-200 ease-swift ${sidebarOpen ? "left-0" : "-left-[280px]"}`
            : "relative"
        }`}
      >
        <DocTree />
      </aside>
      {/* The drawer on a phone keeps its fixed width: it slides over the
          content instead of sharing the row with it, so there is no column
          to trade space with. Straight inside the grid, never wrapped: a
          drag sizes the sidebar column through the handle's parent. */}
      {!mobile && sidebarOpen && <SidebarResizeHandle controls={SIDEBAR_ID} />}
      {/* Relative so <main> is the containing block of every absolutely
          positioned element inside it that has no positioned host of its
          own, such as the sr-only live region at the end of the unpositioned
          .doc-shell. A scroll container only scrolls and clips descendants
          whose containing block it is or lies within; without this they
          resolve against the shell, keep their static position at the foot
          of the whole document, and overflow the window, which grew a third
          scrollbar that moved sidebar and content together into empty space
          (#87). Relative without a z-index makes no stacking context and,
          unlike transform and friends (see layout.css), leaves the fixed
          comment rail and history drawer anchored to the viewport. */}
      <main className={`relative overflow-y-auto bg-bg ${mobile ? "h-dvh" : ""}`}>
        <Outlet />
      </main>
      <WhatsNewCard />
      <Toast />
      <CommandPalette />
    </div>
  );
}
