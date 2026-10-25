import { useDraggable, useDroppable } from "@dnd-kit/core";
import { useQueryClient } from "@tanstack/react-query";
import { ChevronRight, FileText, MoreHorizontal, Plus } from "lucide-react";
import { useCallback, useRef, useState } from "react";
import { Link, useNavigate } from "react-router-dom";

import { ContextMenu, type ContextMenuItem } from "../../components/ContextMenu";
import { IconButton } from "../../components/ui/IconButton";
import { useUi } from "../../stores/ui";

import { archiveConfirmText, archiveErrorText } from "./archive";
import { docsApi } from "./docs.api";
import {
  countDescendants,
  INDENT,
  rowDescription,
  type FlatRow,
  type keyboardMove,
  type MoveArgs,
  type Projection,
  type TreeNode,
  type Zone,
} from "./tree";

/** The row the pointer is over, the part of it, and where a drop puts the page. */
export type Drop = { overId: string; zone: Zone; projection: Projection };

/** A move by the row menu or by Alt+arrow. */
export type MoveCmd = Parameters<typeof keyboardMove>[3];

/** The row menu's move items, in menu order. */
const MOVE_ITEMS: { cmd: MoveCmd; label: string; testId: string }[] = [
  { cmd: "up", label: "Move up", testId: "ctx-move-up" },
  { cmd: "down", label: "Move down", testId: "ctx-move-down" },
  { cmd: "indent", label: "Indent", testId: "ctx-indent" },
  { cmd: "outdent", label: "Outdent", testId: "ctx-outdent" },
];

/** The move each arrow key makes with Alt held. */
const ALT_ARROWS: Partial<Record<string, MoveCmd>> = {
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowRight: "indent",
  ArrowLeft: "outdent",
};

/** What every row of the sidebar tree shares. */
export type TreeState = {
  /** The page open in the editor. */
  openId?: string;
  canEdit: boolean;
  isOwner: boolean;
  retentionDays: number;
  /** The rendered rows by id, from `flattenVisible`. */
  rows: ReadonlyMap<string, FlatRow>;
  collapsed: ReadonlySet<string>;
  onToggle: (id: string) => void;
  onNewChild: (parentId: string) => void;
  /** The page being dragged. Its subtree isn't rendered meanwhile. */
  draggingId: string | null;
  drop: Drop | null;
  /** Where a menu or Alt+arrow move would put the page, or null when it can't move that way. */
  planMove: (id: string, cmd: MoveCmd) => MoveArgs | null;
  /** Makes a menu or Alt+arrow move. */
  onMove: (id: string, cmd: MoveCmd) => void;
};

export function TreeRow({ node, depth, tree }: { node: TreeNode; depth: number; tree: TreeState }) {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const { canEdit, isOwner } = tree;
  // Opened from the keyboard, the menu gives focus back to the link when it closes.
  const [menu, setMenu] = useState<{ x: number; y: number; byKeyboard?: boolean } | null>(null);
  const linkRef = useRef<HTMLAnchorElement | null>(null);

  const { attributes, listeners, setNodeRef: setDragNode, isDragging } = useDraggable({
    id: node.id,
    disabled: !canEdit,
  });
  const { setNodeRef: setDropNode } = useDroppable({ id: node.id, disabled: !canEdit });
  const setRowNode = useCallback(
    (el: HTMLDivElement | null) => {
      setDragNode(el);
      setDropNode(el);
    },
    [setDragNode, setDropNode],
  );

  const row = tree.rows.get(node.id);
  const drop = tree.drop?.overId === node.id ? tree.drop : null;
  const expanded = !tree.collapsed.has(node.id);
  const descId = `tree-desc-${node.id}`;

  async function onRename() {
    const next = window.prompt("Rename to:", node.title);
    if (!next || next === node.title) return;
    const r = await docsApi.patch(node.id, { title: next });
    if ("error" in r) notify("error", "Rename failed");
    else await qc.invalidateQueries({ queryKey: ["docs"] });
  }

  async function onArchive() {
    if (!window.confirm(archiveConfirmText(node.title, countDescendants(node), tree.retentionDays))) return;
    const r = await docsApi.archive(node.id);
    if ("error" in r) {
      notify("error", archiveErrorText(r.error.code));
      return;
    }
    notify("info", "Moved to archive");
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["archive"] });
    // An open page in the moved subtree switches to its archived view.
    await qc.invalidateQueries({ queryKey: ["doc"] });
  }

  async function onToggleTemplate() {
    const next = !node.is_template;
    const r = await docsApi.setTemplate(node.id, next);
    if ("error" in r) {
      notify("error", next ? "Couldn't save as template" : "Couldn't remove template");
      return;
    }
    notify("info", next ? "Saved as template" : "Removed from templates");
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["templates"] });
  }

  const items: ContextMenuItem[] = [];
  if (canEdit) {
    items.push({ label: "Rename", testId: "ctx-rename", onSelect: () => void onRename() });
    for (const m of MOVE_ITEMS) {
      items.push({
        label: m.label,
        testId: m.testId,
        // Worked out only while the menu shows: it walks the whole tree.
        disabled: menu !== null && tree.planMove(node.id, m.cmd) === null,
        onSelect: () => tree.onMove(node.id, m.cmd),
      });
    }
  }
  if (isOwner) {
    items.push({
      label: "Permissions…",
      testId: "ctx-permissions",
      onSelect: () => void nav(`/doc/${node.id}/permissions`),
    });
    items.push({
      label: node.is_template ? "Remove from templates" : "Save as template",
      testId: "ctx-template",
      onSelect: () => void onToggleTemplate(),
    });
  }
  if (canEdit) {
    items.push({ label: "Move to archive", testId: "ctx-delete", destructive: true, onSelect: () => void onArchive() });
  }

  return (
    <li
      // The row menu is portalled out of this <li> in the DOM but not in the
      // React tree, so its events still bubble here. A contextmenu from it
      // would open the browser's own menu over ours: on Windows, Chrome and
      // Edge send the Menu key's contextmenu on keyup, to the item that took
      // focus when the keydown opened the menu. The row's own are left alone.
      onContextMenu={(e) => {
        if (!e.currentTarget.contains(e.target as Node)) e.preventDefault();
      }}
    >
      {/* The row is the drag source and a drop target. dnd-kit's
          `attributes` aren't spread on it: they would make it a
          role="button" tab stop around the link, the row's focus target. */}
      <div
        ref={setRowNode}
        {...listeners}
        data-testid={`tree-row-${node.id}`}
        data-drop-zone={drop?.zone}
        data-drop-depth={drop?.projection.depth}
        className={`relative group flex items-center gap-1 rounded h-7 pr-1 touch-manipulation transition-colors ease-swift duration-150 ${
          tree.openId === node.id
            ? "bg-muted text-fg"
            : "text-fg-muted hover:text-fg hover:bg-muted/60"
        } ${drop?.zone === "into" ? "ring-1 ring-accent bg-accent/10" : ""}`}
        style={{ paddingLeft: 4 + depth * INDENT, opacity: isDragging ? 0.5 : undefined }}
      >
        {drop && drop.zone !== "into" && (
          <span
            aria-hidden
            className={`pointer-events-none absolute right-1 h-0.5 bg-accent rounded ${
              drop.zone === "above" ? "top-0" : "bottom-0"
            }`}
            style={{ left: 4 + drop.projection.depth * INDENT }}
          />
        )}
        {node.children.length > 0 ? (
          <button
            type="button"
            aria-label={expanded ? "Collapse" : "Expand"}
            onClick={(e) => {
              e.preventDefault();
              tree.onToggle(node.id);
            }}
            className="h-5 w-5 inline-flex items-center justify-center text-fg-muted hover:text-fg rounded shrink-0"
          >
            <ChevronRight
              size={12}
              aria-hidden
              className={`transition-transform duration-150 ${expanded ? "rotate-90" : ""}`}
            />
          </button>
        ) : (
          <span className="h-5 w-5 shrink-0" aria-hidden />
        )}
        <FileText size={14} aria-hidden className="text-fg-muted shrink-0" />
        <Link
          ref={linkRef}
          data-testid={`doc-row-${node.id}`}
          to={`/doc/${node.id}`}
          // Viewers can't move pages, so they don't hear how to drag one.
          aria-describedby={canEdit ? `${attributes["aria-describedby"]} ${descId}` : descId}
          onContextMenu={(e) => {
            e.preventDefault();
            if (items.length === 0) return;
            setMenu({ x: e.clientX, y: e.clientY });
          }}
          onKeyDown={(e) => {
            const cmd = ALT_ARROWS[e.key];
            const altOnly = e.altKey && !e.shiftKey && !e.ctrlKey && !e.metaKey;
            const shiftOnly = e.shiftKey && !e.altKey && !e.ctrlKey && !e.metaKey;
            if (cmd && canEdit && altOnly) {
              // Cancelled even when the page can't move that way: Alt+← and
              // Alt+→ are Back and Forward in Windows and Linux browsers.
              e.preventDefault();
              tree.onMove(node.id, cmd);
            } else if ((e.key === "F10" && shiftOnly) || e.key === "ContextMenu") {
              // The keyboard's way to the row menu, which opens below the link.
              e.preventDefault();
              if (items.length === 0) return;
              const r = e.currentTarget.getBoundingClientRect();
              setMenu({ x: r.left, y: r.bottom, byKeyboard: true });
            }
          }}
          className="flex-1 min-w-0 truncate text-[13px] no-underline text-inherit py-1"
        >
          {node.title}
        </Link>
        <span id={descId} className="sr-only">
          {row && rowDescription(row)}
        </span>
        {canEdit && (
          <div className="opacity-0 group-hover:opacity-100 transition-opacity flex items-center gap-0.5">
            <IconButton
              label="More"
              size="sm"
              onClick={(e) => {
                e.preventDefault();
                setMenu({ x: e.clientX, y: e.clientY });
              }}
            >
              <MoreHorizontal size={14} aria-hidden />
            </IconButton>
            <IconButton
              label="Add subpage"
              size="sm"
              onClick={(e) => {
                e.preventDefault();
                tree.onNewChild(node.id);
              }}
            >
              <Plus size={14} aria-hidden />
            </IconButton>
          </div>
        )}
      </div>
      {menu && (
        <ContextMenu
          x={menu.x}
          y={menu.y}
          items={items}
          onClose={() => {
            setMenu(null);
            if (menu.byKeyboard) linkRef.current?.focus();
          }}
        />
      )}
      {/* While this page is dragged its subtree is unmounted, not hidden,
          so no row of it can become a drop target. */}
      {expanded && tree.draggingId !== node.id && node.children.length > 0 && (
        <ul className="list-none p-0 m-0">
          {node.children.map((c) => (
            <TreeRow key={c.id} node={c} depth={depth + 1} tree={tree} />
          ))}
        </ul>
      )}
    </li>
  );
}
