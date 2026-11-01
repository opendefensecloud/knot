import { Eye, MessageSquare, Pencil } from "lucide-react";
import type { ReactNode, Ref } from "react";

import { IconButton } from "../../components/ui/IconButton";

/**
 * Edit and Comments, for when the header's action row has scrolled away.
 * Rendered in exactly one place at a time: the view-mode dock, or the slot at
 * the end of the formatting toolbar while editing.
 */
export function DocDockControls({
  showEdit,
  editMode,
  onToggleEdit,
  showComments,
  commentsOpen,
  onToggleComments,
  editToggleRef,
  commentsRef,
  size,
}: {
  showEdit: boolean;
  editMode: boolean;
  onToggleEdit: () => void;
  showComments: boolean;
  commentsOpen: boolean;
  onToggleComments: () => void;
  editToggleRef: Ref<HTMLButtonElement>;
  commentsRef: Ref<HTMLButtonElement>;
  size: "sm" | "md";
}) {
  return (
    <div data-doc-controls className="flex items-center gap-0.5">
      {showEdit && (
        <IconButton
          data-testid="dock-toggle-edit-mode"
          data-dock-control="edit"
          ref={editToggleRef}
          size={size}
          label={editMode ? "Stop editing (⌘E)" : "Edit (⌘E)"}
          active={editMode}
          aria-keyshortcuts="Control+E Meta+E"
          onClick={onToggleEdit}
        >
          {editMode ? <Eye size={16} aria-hidden /> : <Pencil size={16} aria-hidden />}
        </IconButton>
      )}
      {showComments && (
        <IconButton
          data-testid="dock-open-comments"
          data-dock-control="comments"
          ref={commentsRef}
          size={size}
          label="Comments"
          active={commentsOpen}
          onClick={onToggleComments}
        >
          <MessageSquare size={16} aria-hidden />
        </IconButton>
      )}
    </div>
  );
}

/**
 * The view-mode dock: a slim sticky band at the top of the document column
 * once the action row has scrolled out of view. The host is zero-height, so
 * mounting it never shifts the text; the band itself is absolutely placed.
 *
 * z-10 like the toolbar (they are never up together), which keeps it under
 * the mobile drawer backdrop (z-20), the menu button (z-30) and the comment
 * rail and history drawer (z-40). The blur lives on the band, never on
 * .doc-shell or <main> (see layout.css).
 */
export function DocDockBar({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div data-testid="doc-dock-host" className="sticky top-0 z-10 h-0 print:hidden">
      <div
        data-testid="doc-dock"
        data-sticky-band
        // A group, not a toolbar: role="toolbar" promises roving arrow keys.
        role="group"
        aria-label="Document controls"
        className="knot-dock-in absolute inset-x-0 top-0 py-2 bg-bg/80 backdrop-blur border-b border-border"
      >
        {/* min-h-8 matches the toolbar's 32px buttons, so the band keeps its
            height when the toolbar takes over on entering edit mode. */}
        <div className="measure flex min-h-8 items-center gap-2">
          <span
            aria-hidden
            className="min-w-0 flex-1 truncate text-[13px] font-medium text-fg-muted max-sm:hidden"
          >
            {title || "Untitled"}
          </span>
          <div className="ml-auto">{children}</div>
        </div>
      </div>
    </div>
  );
}
