import { X } from "lucide-react";
import { useId } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { Button } from "../../components/ui/Button";
import { IconButton } from "../../components/ui/IconButton";
import { useViewport } from "../../hooks/useViewport";
import { useUi } from "../../stores/ui";

import { missedReleasesText } from "./about";
import { useMarkSeen, useWhatsNew } from "./useWhatsNew";

/**
 * The card a user gets the first time they open knot after an upgrade. It
 * stays until they dismiss it or open the release notes, and blocks nothing
 * meanwhile. Opening the notes leaves marking the version seen to the About
 * page, which first needs to know which releases were new.
 */
export function WhatsNewCard() {
  const q = useWhatsNew();
  const markSeen = useMarkSeen();
  const nav = useNavigate();
  const { pathname } = useLocation();
  const mobile = useViewport() === "mobile";
  const sidebarOpen = useUi((s) => s.sidebarOpen);
  const titleId = useId();

  const info = q.data && "ok" in q.data ? q.data.ok : null;
  if (!info || info.dev || info.unseen.length === 0 || pathname === "/about") return null;
  // On a phone the open menu drawer has the whole screen; the card waits.
  if (mobile && sidebarOpen) return null;

  return (
    <section
      aria-labelledby={titleId}
      data-testid="whats-new-card"
      // Under every drawer, dialog and toast: it is the one overlay nobody
      // asked for. In the content column's bottom-left corner, clear of the
      // sidebar and, given room, of the toast stack opposite; where the
      // column is too narrow for both, a toast passes over it for its few
      // seconds. A phone has no such room, so there it spans the top,
      // under the menu button.
      className={`fixed z-10 rounded-md border border-border bg-surface shadow-lg p-4 text-fg ${
        mobile
          ? "top-14 inset-x-3"
          : "bottom-4 left-[calc(var(--knot-sidebar-w)_+_1rem)] w-[min(18rem,calc(100vw_-_var(--knot-sidebar-w)_-_2rem))]"
      }`}
      style={{ animation: "slideIn 200ms ease-out" }}
    >
      <div className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <h2 id={titleId} className="text-sm font-semibold m-0">
            knot was updated to {info.version}
          </h2>
          <p className="text-[13px] text-fg-muted m-0 mt-0.5">{missedReleasesText(info.unseen.length)}</p>
        </div>
        <IconButton label="Close" size="sm" className="-mt-1 -mr-1 shrink-0" onClick={() => markSeen()}>
          <X size={14} aria-hidden />
        </IconButton>
      </div>
      <div className="mt-3 flex flex-wrap gap-2">
        <Button
          variant="primary"
          size="sm"
          data-testid="whats-new-open"
          onClick={() => void nav("/about")}
        >
          See what&apos;s new
        </Button>
        <Button size="sm" data-testid="whats-new-dismiss" onClick={() => markSeen()}>
          Dismiss
        </Button>
      </div>
    </section>
  );
}
