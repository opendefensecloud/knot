import { Menu } from "lucide-react";
import { useEffect } from "react";
import { Outlet } from "react-router-dom";

import { DocTree } from "../features/docs/DocTree";
import { useViewport } from "../hooks/useViewport";
import { useUi } from "../stores/ui";

import { CommandPalette } from "./CommandPalette";
import { Toast } from "./Toast";

export function AppShell() {
  const sidebarOpen = useUi((s) => s.sidebarOpen);
  const toggleSidebar = useUi((s) => s.toggleSidebar);
  const vp = useViewport();
  const mobile = vp === "mobile";

  // Mirror the width preference across tabs. setState rather than the
  // persisting setDocWidth: writing back into storage from a storage event
  // is a needless round trip, and this way there is no echo to reason about.
  useEffect(() => {
    const onStorage = (e: StorageEvent) => {
      if (e.key !== "knot.docWidth") return;
      const next = e.newValue === "wide" ? "wide" : "fixed";
      document.documentElement.setAttribute("data-doc-width", next);
      useUi.setState({ docWidth: next });
    };
    window.addEventListener("storage", onStorage);
    return () => window.removeEventListener("storage", onStorage);
  }, []);

  return (
    <div
      className={`h-dvh font-sans text-fg ${mobile ? "block" : "grid"}`}
      style={!mobile ? { gridTemplateColumns: sidebarOpen ? "260px 1fr" : "0 1fr" } : undefined}
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
      <aside
        data-testid="sidebar"
        className={`bg-bg border-r border-border overflow-y-auto ${
          mobile
            ? `fixed top-0 h-dvh w-[260px] z-30 transition-[left] duration-200 ease-swift ${sidebarOpen ? "left-0" : "-left-[280px]"}`
            : "static"
        }`}
      >
        <DocTree />
      </aside>
      <main className={`overflow-y-auto bg-bg ${mobile ? "h-dvh" : ""}`}>
        <Outlet />
      </main>
      <Toast />
      <CommandPalette />
    </div>
  );
}
