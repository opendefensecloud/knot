import { Link } from "react-router-dom";

import { useWhatsNew } from "./useWhatsNew";

/** The sidebar's foot: the running version, linking to the About page. */
export function SidebarVersion() {
  const q = useWhatsNew();
  if (!q.data || "error" in q.data) return null;
  return (
    // 12px clear on the right, like the rest of the sidebar, for the
    // resize handle on its edge. Sticky: a tree taller than the window
    // would otherwise carry it off the bottom of the sidebar, which scrolls.
    <div className="sticky bottom-0 mt-auto bg-bg px-3 py-2 border-t border-border">
      <Link
        to="/about"
        data-testid="sidebar-version"
        className="inline-flex items-center h-7 px-2 rounded text-[11px] text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
      >
        knot {q.data.ok.version}
      </Link>
    </div>
  );
}
