import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useMemo, useState } from "react";

import {
  ChevronRight,
  FileText,
  FilePlus,
  LayoutTemplate,
  MoreHorizontal,
  Plus,
  X,
} from "lucide-react";

import {
  DndContext,
  DragOverlay,
  PointerSensor,
  KeyboardSensor,
  useSensor,
  useSensors,
  closestCenter,
  pointerWithin,
  type CollisionDetection,
  type DragEndEvent,
  type DragMoveEvent,
  type DragOverEvent,
  type DragStartEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  useSortable,
  sortableKeyboardCoordinates,
  type SortingStrategy,
} from "@dnd-kit/sortable";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useUi } from "../../stores/ui";
import { ContextMenu, type ContextMenuItem } from "../../components/ContextMenu";
import { IconButton } from "../../components/ui/IconButton";
import { type Doc } from "../../lib/validators";
import { type ApiError } from "../../lib/api";

import { WorkspaceHeader } from "../workspace/WorkspaceHeader";
import { workspaceApi } from "../workspace/workspace.api";
import { archiveConfirmText, archiveErrorText } from "./archive";
import { docsApi } from "./docs.api";
import { markDocEditMode } from "./editMode";
import { placementParent, type Placement } from "./placement";
import {
  buildTree,
  applyOptimisticMove,
  descendantIds,
  dropIntent,
  resolveDrop,
  type DropPlan,
  type TreeNode,
} from "./tree";

const noShift: SortingStrategy = () => null;

const pointerFirst: CollisionDetection = (args) => {
  const hits = pointerWithin(args);
  return hits.length > 0 ? hits : closestCenter(args);
};

export function DocTree() {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const { id: activeId } = useParams();

  const { workspace } = useEffectiveRole();
  const canEdit = workspace === "owner" || workspace === "editor";
  const isOwner = workspace === "owner";
  const [pickerOpen, setPickerOpen] = useState(false);

  const list = useQuery({
    queryKey: ["docs"],
    queryFn: () => docsApi.list(),
  });
  // Only for the "permanently deleted in N days" line of the archive prompt.
  const workspaceQuery = useQuery({ queryKey: ["workspace"], queryFn: () => workspaceApi.get() });
  const retentionDays =
    workspaceQuery.data && "ok" in workspaceQuery.data ? workspaceQuery.data.ok.archive_retention_days : 30;

  const create = useMutation({
    mutationFn: async (parent_id?: string) =>
      docsApi.create({ title: "Untitled", parent_id }),
    onSuccess: async (r) => {
      if ("error" in r) {
        notify("error", "Couldn't create document");
        return;
      }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      const created = r.ok as { id: string };
      markDocEditMode(created.id);
      await nav(`/doc/${created.id}`);
    },
  });

  const move = useMutation({
    mutationFn: async (a: { id: string; body: { parent_id?: string | null; before_id?: string; after_id?: string } }) =>
      docsApi.move(a.id, a.body),
    onMutate: async (a) => {
      await qc.cancelQueries({ queryKey: ["docs"] });
      const prev = qc.getQueryData<{ ok: Doc[] } | { error: ApiError }>(["docs"]);
      if (prev && "ok" in prev) {
        qc.setQueryData(["docs"], {
          ok: applyOptimisticMove(prev.ok, a.id, a.body),
        });
      }
      return { prev };
    },
    onError: (_e, _a, ctx) => {
      if (ctx?.prev) qc.setQueryData(["docs"], ctx.prev);
      notify("error", "Couldn't move");
    },
    onSettled: () => { void qc.invalidateQueries({ queryKey: ["docs"] }); },
  });

  const sensors = useSensors(
    useSensor(PointerSensor, { activationConstraint: { distance: 6 } }),
    useSensor(KeyboardSensor, { coordinateGetter: sortableKeyboardCoordinates }),
  );

  const flatIds = useMemo(
    () => (list.data && "ok" in list.data ? list.data.ok.map((d) => d.id) : []),
    [list.data],
  );

  const tree = list.data && "ok" in list.data ? buildTree(list.data.ok) : [];

  const [drop, setDrop] = useState<DropPlan | null>(null);
  const [dragging, setDragging] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());

  function toggleCollapsed(id: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function updateDrop(e: DragOverEvent | DragMoveEvent) {
    const over = e.over;
    const activeRect = e.active.rect.current.translated;
    if (!over || !activeRect) { setDrop(null); return; }
    const start = e.activatorEvent;
    const pointer =
      start instanceof MouseEvent
        ? { x: start.clientX + e.delta.x, y: start.clientY + e.delta.y }
        : { x: activeRect.left, y: activeRect.top + activeRect.height / 2 };
    const intent = dropIntent(pointer.y, { top: over.rect.top, height: over.rect.height });
    const plan = resolveDrop(tree, String(e.active.id), String(over.id), intent, {
      pointerX: pointer.x,
      titleX: document.querySelector(`[data-testid="doc-row-${String(over.id)}"]`)?.getBoundingClientRect().left ?? over.rect.left,
      isExpanded: (id) => !collapsed.has(id),
    });
    setDrop((prev) =>
      prev && plan && prev.overId === plan.overId && prev.intent === plan.intent && prev.depth === plan.depth
        ? prev
        : plan,
    );
  }

  function onDragStart(e: DragStartEvent) {
    setDragging(String(e.active.id));
  }

  function swallowClickAfterDrag() {
    const swallow = (ev: MouseEvent) => {
      ev.preventDefault();
      ev.stopPropagation();
    };
    window.addEventListener("click", swallow, { capture: true, once: true });
    window.setTimeout(() => window.removeEventListener("click", swallow, { capture: true }), 100);
  }

  function onDragEnd(e: DragEndEvent) {
    swallowClickAfterDrag();
    const plan = drop;
    setDrop(null);
    setDragging(null);
    const movedId = String(e.active.id);
    if (!plan) {
      const docs = list.data && "ok" in list.data ? list.data.ok : [];
      if (e.over && descendantIds(docs, movedId).has(String(e.over.id))) {
        notify("error", "Can't move a document inside itself");
      }
      return;
    }
    move.mutate({ id: movedId, body: plan.args });
  }

  function onDragCancel() {
    setDrop(null);
    setDragging(null);
  }

  // Named rather than an inline `async` handler: `onPickTemplate` is typed
  // to return void, so handing it a promise leaves rejections unhandled.
  // The caller below fires it with `void`.
  async function createFromTemplate(
    templateId: string,
    title: string,
    parentId: string | null,
  ) {
    setPickerOpen(false);
    const r = await docsApi.createFromTemplate(templateId, {
      title,
      ...(parentId ? { parent_id: parentId } : {}),
    });
    if ("error" in r) {
      notify("error", "Couldn't create from template");
      return;
    }
    await qc.invalidateQueries({ queryKey: ["docs"] });
    const created = r.ok as { id: string };
    markDocEditMode(created.id);
    await nav(`/doc/${created.id}`);
  }

  const draggingTitle =
    dragging && list.data && "ok" in list.data ? list.data.ok.find((d) => d.id === dragging)?.title : undefined;

  return (
    <div data-testid="doc-tree" className="flex flex-col h-full">
      <WorkspaceHeader />
      <div className="px-3 pt-3 pb-1 flex items-center justify-between">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-fg-muted">
          Documents
        </span>
        {canEdit && (
          <IconButton
            data-testid="new-doc"
            label="New document"
            size="sm"
            onClick={() => setPickerOpen(true)}
          >
            <Plus size={14} aria-hidden />
          </IconButton>
        )}
      </div>
      {list.isLoading && (
        <div className="px-3 py-2 text-sm text-fg-muted">Loading…</div>
      )}
      {list.data && "error" in list.data && (
        <div className="px-3 py-2 text-sm text-destructive">Failed.</div>
      )}
      {list.data && "ok" in list.data && tree.length === 0 && (
        <p className="px-3 py-2 text-sm text-fg-muted">No documents yet.</p>
      )}
      {list.data && "ok" in list.data && (
        <DndContext sensors={sensors} collisionDetection={pointerFirst} onDragStart={onDragStart} onDragOver={updateDrop} onDragMove={updateDrop} onDragEnd={onDragEnd} onDragCancel={onDragCancel}>
          <SortableContext items={flatIds} strategy={noShift}>
            {/* 12px clear on the right, like the rest of the sidebar: rows
                are drag targets, and the 8px resize handle on the edge needs
                that much room around it (WCAG 2.5.8 target spacing). */}
            <ul className="pl-2 pr-3 pb-3 list-none m-0 flex-1">
              {tree.map((n) => (
                <TreeRow
                  key={n.id}
                  node={n}
                  depth={0}
                  activeId={activeId}
                  canEdit={canEdit}
                  isOwner={isOwner}
                  retentionDays={retentionDays}
                  onNewChild={(pid) => create.mutate(pid)}
                  dropHint={drop && drop.overId === n.id ? drop : null}
                  drop={drop}
                  collapsed={collapsed}
                  onToggle={toggleCollapsed}
                />
              ))}
            </ul>
          </SortableContext>
          <DragOverlay dropAnimation={null}>
            {draggingTitle !== undefined ? (
              <div className="pointer-events-none inline-flex max-w-56 items-center gap-1 h-7 px-2 rounded bg-muted text-fg text-[13px] shadow-md">
                <FileText size={14} aria-hidden className="text-fg-muted shrink-0" />
                <span className="truncate">{draggingTitle}</span>
              </div>
            ) : null}
          </DragOverlay>
        </DndContext>
      )}
      {pickerOpen && (
        <NewDocPicker
          onClose={() => setPickerOpen(false)}
          current={(() => {
            const docs = list.data && "ok" in list.data ? list.data.ok : [];
            const c = activeId ? docs.find((d) => d.id === activeId) : undefined;
            return c ? { id: c.id, parent_id: c.parent_id, title: c.title } : null;
          })()}
          onPickBlank={(parentId) => {
            setPickerOpen(false);
            create.mutate(parentId ?? undefined);
          }}
          onPickTemplate={(templateId, title, parentId) => {
            void createFromTemplate(templateId, title, parentId);
          }}
        />
      )}
    </div>
  );
}

/** Modal for "New document": choose Blank or one of the workspace templates. */
export function NewDocPicker({
  onClose,
  onPickBlank,
  onPickTemplate,
  current,
}: {
  onClose: () => void;
  onPickBlank: (parentId: string | null) => void;
  onPickTemplate: (templateId: string, title: string, parentId: string | null) => void;
  current: { id: string; parent_id: string | null; title: string } | null;
}) {
  const [placement, setPlacement] = useState<Placement>("nested");
  const parentId = placementParent(placement, current);
  const templates = useQuery({
    queryKey: ["templates"],
    queryFn: () => docsApi.listTemplates(),
    refetchOnMount: "always",
    staleTime: 0,
  });
  const items = templates.data && "ok" in templates.data ? templates.data.ok : [];
  return (
    <div
      className="fixed inset-0 z-50 bg-black/40 backdrop-blur-sm flex items-start justify-center pt-24"
      onClick={onClose}
      data-testid="new-doc-modal"
    >
      <div
        className="bg-surface rounded-lg shadow-xl w-[520px] max-w-[90vw] border border-border"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center justify-between px-4 py-3 border-b border-border">
          <h2 className="text-base font-semibold text-fg">New document</h2>
          <button
            type="button"
            aria-label="Close"
            onClick={onClose}
            className="text-fg-muted hover:text-fg"
          >
            <X size={16} aria-hidden />
          </button>
        </div>
        <div className="p-3 max-h-[60vh] overflow-auto">
          {current && (
            <fieldset className="mb-3 rounded border border-border p-2">
              <legend className="px-1 text-[11px] font-semibold uppercase tracking-wider text-fg-muted">
                Location
              </legend>
              <label className="flex items-center gap-2 py-1 text-sm text-fg cursor-pointer">
                <input
                  type="radio"
                  name="new-doc-location"
                  data-testid="new-doc-loc-nested"
                  checked={placement === "nested"}
                  onChange={() => setPlacement("nested")}
                />
                <span className="truncate">Nested under &quot;{current.title}&quot;</span>
              </label>
              <label className="flex items-center gap-2 py-1 text-sm text-fg cursor-pointer">
                <input
                  type="radio"
                  name="new-doc-location"
                  data-testid="new-doc-loc-sibling"
                  checked={placement === "sibling"}
                  onChange={() => setPlacement("sibling")}
                />
                <span className="truncate">Same level as &quot;{current.title}&quot;</span>
              </label>
            </fieldset>
          )}
          <button
            type="button"
            data-testid="new-doc-blank"
            onClick={() => onPickBlank(parentId)}
            className="w-full flex items-center gap-3 rounded border border-border bg-bg p-3 text-left hover:bg-muted transition-colors"
          >
            <FilePlus size={20} className="text-fg-muted shrink-0" aria-hidden />
            <div>
              <div className="text-sm font-medium text-fg">Blank document</div>
              <div className="text-xs text-fg-muted">Start with an empty page.</div>
            </div>
          </button>
          {templates.isLoading && (
            <p className="mt-4 text-xs text-fg-muted" data-testid="templates-loading">
              Loading templates…
            </p>
          )}
          {!templates.isLoading && items.length > 0 && (
            <>
              <div className="mt-4 mb-2 text-xs font-semibold uppercase tracking-wider text-fg-muted">
                Templates
              </div>
              <ul className="grid grid-cols-2 gap-2 list-none m-0 p-0">
                {items.map((t) => (
                  <li key={t.id}>
                    <button
                      type="button"
                      data-testid={`template-card-${t.id}`}
                      onClick={() => onPickTemplate(t.id, t.title, parentId)}
                      className="w-full h-full flex flex-col items-start gap-1 rounded border border-border bg-bg p-3 text-left hover:bg-muted transition-colors"
                    >
                      <LayoutTemplate size={16} className="text-fg-muted" aria-hidden />
                      <div className="text-sm font-medium text-fg truncate w-full">
                        {t.title}
                      </div>
                    </button>
                  </li>
                ))}
              </ul>
            </>
          )}
          {!templates.isLoading && items.length === 0 && (
            <p className="mt-4 text-xs text-fg-muted">
              Save any doc as a template from its More menu to see it here.
            </p>
          )}
        </div>
      </div>
    </div>
  );
}

/** Subpages (at any depth) that go to the archive with `node`. */
function countDescendants(node: TreeNode): number {
  return node.children.reduce((sum, child) => sum + 1 + countDescendants(child), 0);
}

function TreeRow({
  node,
  depth,
  activeId,
  canEdit,
  isOwner,
  retentionDays,
  onNewChild,
  dropHint,
  drop,
  collapsed,
  onToggle,
}: {
  node: TreeNode;
  depth: number;
  activeId?: string;
  canEdit: boolean;
  isOwner: boolean;
  retentionDays: number;
  onNewChild: (parentId: string) => void;
  dropHint: DropPlan | null;
  drop: DropPlan | null;
  collapsed: Set<string>;
  onToggle: (id: string) => void;
}) {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const isActive = activeId === node.id;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const expanded = !collapsed.has(node.id);

  const { attributes, listeners, setNodeRef, setActivatorNodeRef, isDragging } =
    useSortable({
      id: node.id,
      disabled: !canEdit,
    });
  // The row itself is the keyboard drag handle. Without an activator node
  // the sensor starts a drag (and swallows the key) on Enter/Space anywhere
  // inside the row, so its link and buttons never got the key. Pointer
  // drags still start anywhere in the row.
  const setRowRef = (el: HTMLDivElement | null) => {
    setNodeRef(el);
    setActivatorNodeRef(el);
  };
  const sortableStyle = {
    opacity: isDragging ? 0.5 : 1,
  };

  async function onRename() {
    const next = window.prompt("Rename to:", node.title);
    if (!next || next === node.title) return;
    const r = await docsApi.patch(node.id, { title: next });
    if ("error" in r) notify("error", "Rename failed");
    else await qc.invalidateQueries({ queryKey: ["docs"] });
  }

  async function onArchive() {
    if (!window.confirm(archiveConfirmText(node.title, countDescendants(node), retentionDays))) return;
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

  async function onDuplicate() {
    const r = await docsApi.createFromTemplate(node.id, node.parent_id ? { parent_id: node.parent_id } : {});
    if ("error" in r) {
      notify("error", "Couldn't duplicate");
      return;
    }
    const copy = r.ok as { id: string };
    await docsApi.move(copy.id, { parent_id: node.parent_id, after_id: node.id });
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await nav(`/doc/${copy.id}`);
  }

  const items: ContextMenuItem[] = [];
  if (canEdit) {
    items.push({ label: "Rename", testId: "ctx-rename", onSelect: () => void onRename() });
    items.push({ label: "Duplicate", testId: "ctx-duplicate", onSelect: () => void onDuplicate() });
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
    <li style={sortableStyle}>
      <div
        ref={setRowRef}
        {...attributes}
        {...listeners}
        className={`relative group flex items-center gap-1 rounded h-7 pr-1 transition-colors ease-swift duration-150 ${
          isActive
            ? "bg-muted text-fg"
            : "text-fg-muted hover:text-fg hover:bg-muted/60"
        } ${dropHint?.intent === "into" ? "ring-1 ring-accent bg-accent/10" : ""}`}
        style={{ paddingLeft: 4 + depth * 12 }}
      >
        {dropHint?.intent === "before" && (
          <span
            className="pointer-events-none absolute right-1 top-0 h-0.5 bg-accent rounded"
            style={{ left: 4 + dropHint.depth * 12 }}
          />
        )}
        {dropHint?.intent === "after" && (
          <span
            className="pointer-events-none absolute right-1 bottom-0 h-0.5 bg-accent rounded"
            style={{ left: 4 + dropHint.depth * 12 }}
          />
        )}
        {node.children.length > 0 ? (
          <button
            type="button"
            aria-label={expanded ? "Collapse" : "Expand"}
            onClick={(e) => { e.preventDefault(); onToggle(node.id); }}
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
          onContextMenu={(e) => {
            e.preventDefault();
            if (items.length === 0) return;
            setMenu({ x: e.clientX, y: e.clientY });
          }}
          className="flex-1 min-w-0 truncate text-[13px] no-underline text-inherit py-1"
        >
          {node.title}
        </Link>
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
                onNewChild(node.id);
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
      {expanded && node.children.length > 0 && (
        <ul className="list-none p-0 m-0">
          {node.children.map((c) => (
            <TreeRow
              key={c.id}
              node={c}
              depth={depth + 1}
              activeId={activeId}
              canEdit={canEdit}
              isOwner={isOwner}
              retentionDays={retentionDays}
              onNewChild={onNewChild}
              dropHint={drop && drop.overId === c.id ? drop : null}
              drop={drop}
              collapsed={collapsed}
              onToggle={onToggle}
            />
          ))}
        </ul>
      )}
    </li>
  );
}
