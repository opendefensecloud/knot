import { FoldHorizontal, UnfoldHorizontal } from "lucide-react";

import { IconButton } from "../../components/ui/IconButton";
import { useViewport } from "../../hooks/useViewport";
import { useUi } from "../../stores/ui";

/**
 * Switches the document between the fixed reading column and the wide
 * layout. Not role-gated — width is a reading preference, so viewers get it
 * too, unlike the ⌘E edit toggle it sits beside.
 *
 * Hidden below the desktop breakpoint: wide mode's media query starts at the
 * same 1024px, and a control that provably does nothing is worse than none.
 */
export function DocWidthToggle() {
  const docWidth = useUi((s) => s.docWidth);
  const toggleDocWidth = useUi((s) => s.toggleDocWidth);
  const vp = useViewport();

  if (vp !== "desktop") return null;

  const wide = docWidth === "wide";
  return (
    <IconButton
      data-testid="toggle-doc-width"
      label={wide ? "Fixed width (⌘⇧F)" : "Wide layout (⌘⇧F)"}
      active={wide}
      onClick={toggleDocWidth}
    >
      {wide ? <FoldHorizontal size={16} aria-hidden /> : <UnfoldHorizontal size={16} aria-hidden />}
    </IconButton>
  );
}
