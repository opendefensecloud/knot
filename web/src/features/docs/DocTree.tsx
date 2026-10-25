import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useNavigate, useParams } from "react-router-dom";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode, type RefObject } from "react";

import { FileText, FilePlus, LayoutTemplate, Plus, X } from "lucide-react";

import {
  DndContext,
  DragOverlay,
  MeasuringStrategy,
  MouseSensor,
  TouchSensor,
  useDroppable,
  useSensor,
  useSensors,
  type Announcements,
  type ClientRect,
  type CollisionDetection,
  type UniqueIdentifier,
} from "@dnd-kit/core";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { useUi } from "../../stores/ui";
import { IconButton } from "../../components/ui/IconButton";
import { type Doc } from "../../lib/validators";
import { type ApiError } from "../../lib/api";

import { SidebarVersion } from "../about/SidebarVersion";
import { WorkspaceHeader } from "../workspace/WorkspaceHeader";
import { workspaceApi } from "../workspace/workspace.api";
import { docsApi } from "./docs.api";
import { markDocEditMode } from "./editMode";
import { placementParent, type Placement } from "./placement";
import {
  buildTree,
  applyOptimisticMove,
  describeMove,
  dropZone,
  flattenVisible,
  projectDrop,
  INDENT,
  type FlatRow,
  type MoveArgs,
} from "./tree";
import { TreeRow, type Drop, type TreeState } from "./TreeRow";

const SCREEN_READER_INSTRUCTIONS = {
  draggable: "Drag to move. Use the row menu or Alt with the arrow keys to move by keyboard.",
};

/** Rows are measured while dragging too: the dragged page's subtree unmounts
 *  when the drag starts, and the rows below it move up. */
const MEASURING = { droppable: { strategy: MeasuringStrategy.Always } };

/** The droppable id of the tree's list. Doc ids are UUIDs, so it can't clash. */
const TREE_AREA = ":tree";

/** The row the collision detection found under the pointer, measured in the
 *  same pass, so the drop zone and dnd-kit's `over` always agree. */
type Hit = { id: string; rect: ClientRect; pointerY: number };

export function DocTree() {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const { id: openId } = useParams();

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
    mutationFn: async (a: { id: string; body: MoveArgs }) => docsApi.move(a.id, a.body),
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

  // No KeyboardSensor: moving by keyboard is a set of explicit commands, not
  // a keyboard drag. On touch a drag needs a press and hold, so a swipe still
  // scrolls the sidebar.
  const sensors = useSensors(
    useSensor(MouseSensor, { activationConstraint: { distance: 6 } }),
    useSensor(TouchSensor, { activationConstraint: { delay: 250, tolerance: 5 } }),
  );

  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => new Set());
  const toggle = useCallback((id: string) => {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (!next.delete(id)) next.add(id);
      return next;
    });
  }, []);

  const [draggingId, setDraggingId] = useState<string | null>(null);
  const docs = useMemo(() => (list.data && "ok" in list.data ? list.data.ok : []), [list.data]);
  const tree = useMemo(() => buildTree(docs), [docs]);
  const flat = useMemo(
    () => flattenVisible(tree, collapsed, draggingId ?? undefined),
    [tree, collapsed, draggingId],
  );
  const rows = useMemo(() => new Map(flat.map((r) => [r.id, r])), [flat]);
  const [dropped] = useState(lastDrop);
  const accessibility = useMemo(
    () => ({
      announcements: makeAnnouncements(() => flat, dropped.get),
      screenReaderInstructions: SCREEN_READER_INSTRUCTIONS,
    }),
    [flat, dropped],
  );

  const treeRef = useRef<HTMLUListElement | null>(null);
  const hitRef = useRef<Hit | null>(null);
  const collisionDetection = useCallback<CollisionDetection>((args) => {
    hitRef.current = rowAtPointer(args);
    return hitRef.current ? [{ id: hitRef.current.id }] : [];
  }, []);

  // The projection shown (the drop line and the row's data-drop-* attributes)
  // and sent on drop. Kept in a ref too: drag events can come before the
  // re-render that the last update asked for.
  const [drop, setDrop] = useState<Drop | null>(null);
  const dropRef = useRef<Drop | null>(null);
  function showDrop(next: Drop | null) {
    if (sameDrop(dropRef.current, next)) return;
    dropRef.current = next;
    setDrop(next);
  }

  /** Re-project from the latest hit: the pointer's height picks the gap,
   *  the horizontal drag the depth. */
  function track(activeId: UniqueIdentifier, deltaX: number) {
    const hit = hitRef.current;
    if (!hit) {
      showDrop(null);
      return;
    }
    const id = String(activeId);
    const zone = dropZone(hit.pointerY, hit.rect, hit.id === id);
    const projection = projectDrop(flat, docs, id, hit.id, zone, Math.round(deltaX / INDENT));
    showDrop(projection && { overId: hit.id, zone, projection });
  }

  const [clicks] = useState(clickSwallower);
  useEffect(() => () => clicks.release(), [clicks]);

  function endDrag() {
    hitRef.current = null;
    showDrop(null);
    setDraggingId(null);
    clicks.releaseSoon();
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

  const state: TreeState = {
    openId,
    canEdit,
    isOwner,
    retentionDays,
    rows,
    collapsed,
    onToggle: toggle,
    onNewChild: (pid) => create.mutate(pid),
    draggingId,
    drop,
  };
  const dragged = draggingId === null ? undefined : rows.get(draggingId);

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
        <DndContext
          sensors={sensors}
          collisionDetection={collisionDetection}
          measuring={MEASURING}
          accessibility={accessibility}
          onDragStart={(e) => {
            hitRef.current = null;
            showDrop(null);
            setDraggingId(String(e.active.id));
            clicks.arm(() => treeRef.current);
          }}
          onDragMove={(e) => track(e.active.id, e.delta.x)}
          onDragOver={(e) => track(e.active.id, e.delta.x)}
          onDragEnd={(e) => {
            const d = dropRef.current;
            dropped.set(d);
            endDrag();
            // No projection: a drop in place, a no-op or outside the tree.
            if (d) move.mutate({ id: String(e.active.id), body: d.projection.args });
          }}
          onDragCancel={endDrag}
        >
          <TreeList listRef={treeRef} disabled={!canEdit}>
            {tree.map((n) => (
              <TreeRow key={n.id} node={n} depth={0} tree={state} />
            ))}
          </TreeList>
          <DragOverlay dropAnimation={null}>
            {dragged && (
              <div
                aria-hidden
                className="flex items-center gap-1 h-7 px-2 rounded bg-surface border border-border shadow-lg text-[13px] text-fg cursor-grabbing"
              >
                <FileText size={14} aria-hidden className="text-fg-muted shrink-0" />
                <span className="truncate">{dragged.title}</span>
              </div>
            )}
          </DragOverlay>
        </DndContext>
      )}
      <SidebarVersion />
      {pickerOpen && (
        <NewDocPicker
          onClose={() => setPickerOpen(false)}
          current={(() => {
            const c = openId ? docs.find((d) => d.id === openId) : undefined;
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

/** The tree's list. It is a droppable only so that dnd-kit measures it with
 *  the rows (scroll-adjusted, with no DOM reads of ours during a drag): the
 *  collision detection takes the tree's bounds from it and never returns it. */
function TreeList({
  listRef,
  disabled,
  children,
}: {
  listRef: RefObject<HTMLUListElement | null>;
  disabled: boolean;
  children: ReactNode;
}) {
  const { setNodeRef } = useDroppable({ id: TREE_AREA, disabled });
  const setList = useCallback(
    (el: HTMLUListElement | null) => {
      setNodeRef(el);
      listRef.current = el;
    },
    [setNodeRef, listRef],
  );
  return (
    // 12px clear on the right, like the rest of the sidebar: rows are drag
    // targets, and the 8px resize handle on the edge needs that much room
    // around it (WCAG 2.5.8 target spacing).
    <ul ref={setList} className="pl-2 pr-3 pb-3 list-none m-0 flex-1">
      {children}
    </ul>
  );
}

/** Collision detection by pointer, not by the drag ghost: the row under the
 *  pointer, else the row whose vertical centre is closest. Nothing when the
 *  pointer is outside the tree, so a drop there cancels. A band one row high
 *  above the list counts as inside and means the gap above the first row (the
 *  closest one there), so moving a page to the very top isn't a 7px target
 *  (that row's top quarter) with a cancel just above it. */
function rowAtPointer({
  droppableContainers,
  droppableRects,
  pointerCoordinates: p,
}: Parameters<CollisionDetection>[0]): Hit | null {
  const area = droppableRects.get(TREE_AREA);
  if (!p || !area || p.x < area.left || p.x > area.right || p.y > area.bottom) return null;
  let best: (Hit & { dist: number }) | null = null;
  for (const { id } of droppableContainers) {
    const rect = id === TREE_AREA ? undefined : droppableRects.get(id);
    if (!rect) continue;
    const hit = { id: String(id), rect, pointerY: p.y };
    if (p.y >= rect.top && p.y < rect.bottom) return hit;
    const dist = Math.abs(rect.top + rect.height / 2 - p.y);
    if (!best || dist < best.dist) best = { ...hit, dist };
  }
  // Above the list, `best` is the first row: its height is the band's.
  if (!best || p.y < area.top - best.rect.height) return null;
  return { id: best.id, rect: best.rect, pointerY: p.y };
}

function sameDrop(a: Drop | null, b: Drop | null): boolean {
  if (a === null || b === null) return a === b;
  const [p, q] = [a.projection, b.projection];
  return (
    a.overId === b.overId &&
    a.zone === b.zone &&
    p.depth === q.depth &&
    p.minDepth === q.minDepth &&
    p.maxDepth === q.maxDepth &&
    p.args.parent_id === q.args.parent_id &&
    p.args.before_id === q.args.before_id &&
    p.args.after_id === q.args.after_id
  );
}

/** Swallows the click that ends a pointer drag when it lands in the tree.
 *  Released on a row link, that click would follow the href (a full page
 *  load): dnd-kit only stops its propagation. The listener is on `window` in
 *  the capture phase, because dnd-kit's own capture listener on `document`
 *  stops the click before anything in the tree could see it. It goes after
 *  the first click, or 100 ms after the drag ends. */
function clickSwallower() {
  let within: () => Element | null = () => null;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const swallow = (e: MouseEvent) => {
    if (within()?.contains(e.target as Node)) {
      e.preventDefault();
      e.stopPropagation();
    }
    release();
  };
  function release() {
    window.removeEventListener("click", swallow, { capture: true });
    clearTimeout(timer);
    timer = undefined;
  }
  return {
    /** From drag start on. */
    arm(tree: () => Element | null) {
      release();
      within = tree;
      window.addEventListener("click", swallow, { capture: true });
    },
    /** At drag end or cancel. */
    releaseSoon() {
      clearTimeout(timer);
      timer = setTimeout(release, 100);
    },
    release,
  };
}

/** The drop the last drag ended with, for its announcement: dnd-kit
 *  announces the end of a drag after `onDragEnd`, which has cleared the
 *  shown drop by then. */
function lastDrop() {
  let drop: Drop | null = null;
  return {
    get: (): Drop | null => drop,
    set(d: Drop | null) {
      drop = d;
    },
  };
}

/** dnd-kit's live-region messages, by title: an id means nothing to a
 *  listener. `getFlat` returns the rows on screen, `getDrop` the drop the
 *  drag ended with. The end says what the drop did, not which row it was
 *  over: an un-nest on one's own row moves the page, and a drop over
 *  another row may leave it where it was. */
export function makeAnnouncements(getFlat: () => FlatRow[], getDrop: () => Drop | null): Announcements {
  const name = (id: UniqueIdentifier) => {
    const title = getFlat().find((r) => r.id === String(id))?.title;
    return title === undefined ? "a page" : `“${title}”`;
  };
  return {
    onDragStart: ({ active }) => `Picked up ${name(active.id)}.`,
    onDragOver: ({ active, over }) =>
      !over
        ? `${name(active.id)} is no longer over a page.`
        : over.id === active.id
          ? `${name(active.id)} is over its own row.`
          : `${name(active.id)} is over ${name(over.id)}.`,
    onDragEnd: ({ active, over }) => {
      const drop = getDrop();
      if (drop) return describeMove(getFlat(), String(active.id), drop.projection.args);
      return !over
        ? `${name(active.id)} was dropped outside the tree. It didn't move.`
        : `${name(active.id)} was dropped. It didn't move.`;
    },
    onDragCancel: ({ active }) => `Moving ${name(active.id)} was cancelled. It didn't move.`,
  };
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
