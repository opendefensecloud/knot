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
  PointerSensor,
  KeyboardSensor,
  useSensor,
  useSensors,
  closestCenter,
  type DragEndEvent,
} from "@dnd-kit/core";
import {
  SortableContext,
  useSortable,
  verticalListSortingStrategy,
  sortableKeyboardCoordinates,
} from "@dnd-kit/sortable";
import { CSS } from "@dnd-kit/utilities";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useUi } from "../../stores/ui";
import { ContextMenu, type ContextMenuItem } from "../../components/ContextMenu";
import { IconButton } from "../../components/ui/IconButton";
import { type Doc } from "../../lib/validators";
import { type ApiError } from "../../lib/api";

import { WorkspaceHeader } from "../workspace/WorkspaceHeader";
import { docsApi } from "./docs.api";
import { markDocEditMode } from "./editMode";
import { buildTree, reorderInto, type TreeNode } from "./tree";

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
          ok: reorderInto(prev.ok, a.id, a.body.parent_id ?? null),
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

  function onDragEnd(e: DragEndEvent) {
    const movedId = String(e.active.id);
    if (!e.over) return;
    const targetId = String(e.over.id);
    if (movedId === targetId) return;
    // v0.1 UX: drop-onto-row = nest as child of target.
    move.mutate({ id: movedId, body: { parent_id: targetId } });
  }

  const tree = list.data && "ok" in list.data ? buildTree(list.data.ok) : [];

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
        <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
          <SortableContext items={flatIds} strategy={verticalListSortingStrategy}>
            <ul className="px-2 pb-3 list-none m-0 flex-1">
              {tree.map((n) => (
                <TreeRow
                  key={n.id}
                  node={n}
                  depth={0}
                  activeId={activeId}
                  canEdit={canEdit}
                  isOwner={isOwner}
                  onNewChild={(pid) => create.mutate(pid)}
                />
              ))}
            </ul>
          </SortableContext>
        </DndContext>
      )}
      {pickerOpen && (
        <NewDocPicker
          onClose={() => setPickerOpen(false)}
          onPickBlank={() => {
            setPickerOpen(false);
            create.mutate(undefined);
          }}
          onPickTemplate={async (templateId, title) => {
            setPickerOpen(false);
            const r = await docsApi.createFromTemplate(templateId, { title });
            if ("error" in r) {
              notify("error", "Couldn't create from template");
              return;
            }
            await qc.invalidateQueries({ queryKey: ["docs"] });
            const created = r.ok as { id: string };
            markDocEditMode(created.id);
            await nav(`/doc/${created.id}`);
          }}
        />
      )}
    </div>
  );
}

/** Modal for "New document": choose Blank or one of the workspace templates. */
function NewDocPicker({
  onClose,
  onPickBlank,
  onPickTemplate,
}: {
  onClose: () => void;
  onPickBlank: () => void;
  onPickTemplate: (templateId: string, title: string) => void;
}) {
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
          <button
            type="button"
            data-testid="new-doc-blank"
            onClick={onPickBlank}
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
                      onClick={() => onPickTemplate(t.id, t.title)}
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

function TreeRow({
  node,
  depth,
  activeId,
  canEdit,
  isOwner,
  onNewChild,
}: {
  node: TreeNode;
  depth: number;
  activeId?: string;
  canEdit: boolean;
  isOwner: boolean;
  onNewChild: (parentId: string) => void;
}) {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const isActive = activeId === node.id;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [expanded, setExpanded] = useState(true);

  const { attributes, listeners, setNodeRef, transform, isDragging } = useSortable({
    id: node.id,
    disabled: !canEdit,
  });
  const sortableStyle = {
    transform: CSS.Transform.toString(transform),
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
    if (!window.confirm(`Delete "${node.title}"?`)) return;
    const r = await docsApi.archive(node.id);
    if ("error" in r) notify("error", "Delete failed");
    else await qc.invalidateQueries({ queryKey: ["docs"] });
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
    items.push({ label: "Delete", testId: "ctx-delete", destructive: true, onSelect: () => void onArchive() });
  }

  return (
    <li ref={setNodeRef} style={sortableStyle} {...attributes} {...listeners}>
      <div
        className={`group flex items-center gap-1 rounded h-7 pr-1 transition-colors ease-swift duration-150 ${
          isActive
            ? "bg-muted text-fg"
            : "text-fg-muted hover:text-fg hover:bg-muted/60"
        }`}
        style={{ paddingLeft: 4 + depth * 12 }}
      >
        {node.children.length > 0 ? (
          <button
            type="button"
            aria-label={expanded ? "Collapse" : "Expand"}
            onClick={(e) => { e.preventDefault(); setExpanded((v) => !v); }}
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
              onNewChild={onNewChild}
            />
          ))}
        </ul>
      )}
    </li>
  );
}
