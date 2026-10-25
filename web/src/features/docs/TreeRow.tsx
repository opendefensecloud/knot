import { useDraggable, useDroppable } from "@dnd-kit/core";
import { useQueryClient } from "@tanstack/react-query";
import { ChevronRight, FileText, MoreHorizontal, Plus } from "lucide-react";
import { useCallback, useState } from "react";
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
  type Projection,
  type TreeNode,
  type Zone,
} from "./tree";

/** The row the pointer is over, the part of it, and where a drop puts the page. */
export type Drop = { overId: string; zone: Zone; projection: Projection };

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
};

export function TreeRow({ node, depth, tree }: { node: TreeNode; depth: number; tree: TreeState }) {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const { canEdit, isOwner } = tree;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

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
    <li>
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
          data-testid={`doc-row-${node.id}`}
          to={`/doc/${node.id}`}
          // Viewers can't move pages, so they don't hear how to drag one.
          aria-describedby={canEdit ? `${attributes["aria-describedby"]} ${descId}` : descId}
          onContextMenu={(e) => {
            e.preventDefault();
            if (items.length === 0) return;
            setMenu({ x: e.clientX, y: e.clientY });
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
        <ContextMenu x={menu.x} y={menu.y} items={items} onClose={() => setMenu(null)} />
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
