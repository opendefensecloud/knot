import { Outlet } from "react-router-dom";

import { DocTree } from "../features/docs/DocTree";
import { useUi } from "../stores/ui";

import { Toast } from "./Toast";

export function AppShell() {
  const sidebarOpen = useUi((s) => s.sidebarOpen);
  return (
    <div
      style={{
        display: "grid",
        gridTemplateColumns: sidebarOpen ? "260px 1fr" : "0 1fr",
        height: "100vh",
        fontFamily: "system-ui, sans-serif",
      }}
    >
      <aside
        data-testid="sidebar"
        style={{
          borderRight: "1px solid #e5e5e5",
          overflow: "auto",
          background: "#fafafa",
        }}
      >
        <DocTree />
      </aside>
      <main style={{ overflow: "auto" }}>
        <Outlet />
      </main>
      <Toast />
    </div>
  );
}
