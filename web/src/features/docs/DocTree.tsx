import { useMutation, useMutationState, useQuery, useQueryClient } from "@tanstack/react-query";
import { useLocation, useNavigate, useParams } from "react-router-dom";
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
import { DuplicateDialog, duplicateErrorText } from "./DuplicateDialog";
import { markDocEditMode } from "./editMode";
import { placementParent, type Placement } from "./placement";
import {
  buildTree,
  applyOptimisticMove,
  countDescendants,
  describeMove,
  dropZone,
  flattenVisible,
  keyboardMove,
  projectDrop,
  INDENT,
  type FlatRow,
  type MoveArgs,
  type TreeNode,
} from "./tree";
import { TreeRow, type Drop, type MoveCmd, type TreeState } from "./TreeRow";

const SCREEN_READER_INSTRUCTIONS = {
  draggable: "Drag to move. Use the row menu or Alt with the arrow keys to move by keyboard.",
};

/** Rows are measured while dragging too: the dragged page's subtree unmounts
 *  when the drag starts, and the rows below it move up. */
const MEASURING = { droppable: { strategy: MeasuringStrategy.Always } };

/** The scope the move mutations queue in, one at a time. */
const MOVE_SCOPE = "docs-move";

/** The key of the move mutations, to count the pending ones. */
const MOVE_KEY = [MOVE_SCOPE];

/** The key of the duplicate mutations, to tell whether one is on its way. */
const DUPLICATE_KEY = ["docs-duplicate"];

/** The page the Duplicate dialog asks about. */
type DuplicateAsk = { id: string; title: string; subpages: number };

/** What a duplicate mutation copies, and the page's title for the live
 *  region. */
type DuplicateArgs = { id: string; title: string; include_subpages: boolean };

/** What the live region says while a page without subpages is copied. */
function duplicatingText(title: string): string {
  return `Duplicating “${title}”…`;
}

/** The droppable id of the tree's list. Doc ids are UUIDs, so it can't clash. */
const TREE_AREA = ":tree";

/** The row the collision detection found under the pointer, measured in the
 *  same pass, so the drop zone and dnd-kit's `over` always agree. */
type Hit = { id: string; rect: ClientRect; pointerY: number };

export function DocTree() {
  const qc = useQueryClient();
  const nav = useNavigate();
  const location = useLocation();
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

  // What the last menu or Alt+arrow move did, that a move was refused, or
  // that a copy is being made, for the live region.
  const [announcement, setAnnouncement] = useState("");

  /** How many moves are pending: the one asking, and any queued with it. */
  const pendingMoves = () => qc.isMutating({ mutationKey: MOVE_KEY });

  // Drags, the row menu and Alt+arrows queue their moves in one scope, so
  // they are sent one at a time, in the order they were made: each one is
  // planned from the list the moves before it show.
  const move = useMutation({
    mutationKey: MOVE_KEY,
    scope: { id: MOVE_SCOPE },
    mutationFn: async (a: { id: string; body: MoveArgs }) => {
      const r = await docsApi.move(a.id, a.body);
      // Thrown, so a refused move (409, 422) is rolled back like a failed request.
      if ("error" in r) throw new Error(r.error.code);
      return r.ok;
    },
    onMutate: async (a) => {
      await qc.cancelQueries({ queryKey: ["docs"] });
      const prev = qc.getQueryData<{ ok: Doc[] } | { error: ApiError }>(["docs"]);
      if (prev && "ok" in prev) {
        qc.setQueryData(["docs"], {
          ok: applyOptimisticMove(prev.ok, a.id, a.body),
        });
      }
      const title = prev && "ok" in prev ? prev.ok.find((d) => d.id === a.id)?.title : undefined;
      return { prev, title: title ?? "" };
    },
    onError: (_e, _a, ctx) => {
      // The list from before this move goes back only when no move is queued
      // after it: that list lacks the later moves' optimistic updates. Either
      // way the refetch after the last queued move shows the server's list.
      if (ctx?.prev && pendingMoves() === 1) qc.setQueryData(["docs"], ctx.prev);
      notify("error", "Couldn't move");
      setAnnouncement(`Couldn't move “${ctx?.title ?? ""}”.`);
    },
    onSettled: () => {
      // After the last queued move only: the list fetched in between would
      // drop the optimistic updates of the moves still on their way.
      if (pendingMoves() === 1) void qc.invalidateQueries({ queryKey: ["docs"] });
    },
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
      announcements: makeAnnouncements(() => flat, () => docs, dropped.get),
      screenReaderInstructions: SCREEN_READER_INSTRUCTIONS,
    }),
    [flat, docs, dropped],
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
   *  the horizontal drag the depth. A level takes a full INDENT, so the 6px
   *  that start a drag never change it. While the pointer is less than one
   *  INDENT from the press there is no drop: a click that slips (sideways,
   *  or a few px up into the row above, however it jitters on the way)
   *  moves nothing. */
  function track(activeId: UniqueIdentifier, deltaX: number) {
    const hit = hitRef.current;
    if (!hit || slip.isNear()) {
      showDrop(null);
      return;
    }
    const id = String(activeId);
    const zone = dropZone(hit.pointerY, hit.rect, hit.id === id);
    const projection = projectDrop(flat, docs, id, hit.id, zone, Math.trunc(deltaX / INDENT));
    showDrop(projection && { overId: hit.id, zone, projection });
  }

  const [clicks] = useState(clickSwallower);
  const [slip] = useState(slipTracker);
  useEffect(
    () => () => {
      clicks.release();
      slip.stop();
    },
    [clicks, slip],
  );

  // The page a menu or Alt+arrow move keeps focus on, until the move and the
  // refetch after it have rendered. Focus is lost when the menu the move came
  // from closes, and when the page's row remounts, its link with it: under
  // another parent, or back under the old one when the move fails.
  const refocus = useRef<string | null>(null);

  function moveBy(id: string, cmd: MoveCmd) {
    const args = keyboardMove(flat, docs, id, cmd);
    if (!args) return;
    // Indenting into a collapsed page opens it: the page stays in view, with focus.
    if (args.parent_id !== null && collapsed.has(args.parent_id)) toggle(args.parent_id);
    setAnnouncement(describeMove(flat, docs, id, args));
    refocus.current = id;
    move.mutate({ id, body: args });
  }

  useEffect(() => {
    const id = refocus.current;
    if (id === null) return;
    // Only focus that was lost: never take it from where the user put it.
    const lost = document.activeElement === null || document.activeElement === document.body;
    if (lost) rowLink(treeRef.current, id)?.focus();
    if (!move.isPending && !list.isFetching) refocus.current = null;
  });

  function endDrag() {
    hitRef.current = null;
    showDrop(null);
    setDraggingId(null);
    clicks.releaseSoon();
    slip.stop();
  }

  // The page the Duplicate dialog asks about, and why its last try failed.
  const [dupAsk, setDupAsk] = useState<DuplicateAsk | null>(null);
  const [dupError, setDupError] = useState<string | null>(null);
  // The same page, for the answers to read: one that comes after its dialog
  // has closed goes to a toast instead.
  const dupOpen = useRef<string | null>(null);
  // The pages whose copy is being made: their rows say so, and their
  // "Duplicate…" is off meanwhile.
  const copyingIds = useMutationState({
    filters: { mutationKey: DUPLICATE_KEY, status: "pending" },
    select: (m) => (m.state.variables as DuplicateArgs).id,
  });
  const copying = useMemo(() => new Set(copyingIds), [copyingIds]);
  /** Whether a copy of page `id` is on its way. Asked of the mutation cache,
   *  which counts a request at once: `copying` only renders a tick later. */
  const copyingNow = (id: string) =>
    qc.isMutating({
      mutationKey: DUPLICATE_KEY,
      predicate: (m) => (m.state.variables as DuplicateArgs | undefined)?.id === id,
    }) > 0;

  function closeDuplicate(id: string) {
    dupOpen.current = null;
    setDupAsk(null);
    setDupError(null);
    rowLink(treeRef.current, id)?.focus();
  }

  /** Says why a duplicate failed: in its dialog while that is open, else in
   *  a toast. The list is fetched again: the copy may have been stored all
   *  the same, when the request timed out (408) or its answer was lost. */
  function duplicateFailed(id: string, text: string) {
    void qc.invalidateQueries({ queryKey: ["docs"] });
    if (dupOpen.current === id) setDupError(text);
    else notify("error", text);
  }

  const duplicate = useMutation({
    mutationKey: DUPLICATE_KEY,
    mutationFn: (a: DuplicateArgs) => docsApi.duplicate(a.id, { include_subpages: a.include_subpages }),
    onSuccess: async (r, a) => {
      if ("error" in r) {
        duplicateFailed(a.id, duplicateErrorText(r.error));
        return;
      }
      await qc.invalidateQueries({ queryKey: ["docs"] });
      if (dupOpen.current === a.id) closeDuplicate(a.id);
      // Another page's Duplicate dialog is open: the copy shows in the list,
      // and the app stays where it is, dialog and all.
      if (dupOpen.current !== null) return;
      markDocEditMode(r.ok.id);
      await nav(`/doc/${r.ok.id}`);
    },
    // No answer, or one that isn't a page.
    onError: (_e, a) => duplicateFailed(a.id, duplicateErrorText()),
    // The live region speaks only when its text changes: cleared, a later
    // copy of the same page is heard again.
    onSettled: (_r, _e, a) =>
      setAnnouncement((now) => (now === duplicatingText(a.title) ? "" : now)),
  });

  /** "Duplicate…": a page with subpages asks whether they go too; any other
   *  is copied at once. */
  function startDuplicate(node: TreeNode) {
    const subpages = countDescendants(node);
    if (subpages === 0) {
      setAnnouncement(duplicatingText(node.title));
      duplicate.mutate({ id: node.id, title: node.title, include_subpages: false });
      return;
    }
    dupOpen.current = node.id;
    setDupError(null);
    setDupAsk({ id: node.id, title: node.title, subpages });
  }

  function confirmDuplicate(ask: DuplicateAsk, includeSubpages: boolean) {
    // Ignored while this page's copy is on its way, or a double click makes
    // two. Another page's copy doesn't hold it up, as in the row menu.
    if (copyingNow(ask.id)) return;
    setDupError(null);
    duplicate.mutate({ id: ask.id, title: ask.title, include_subpages: includeSubpages });
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
    planMove: (id, cmd) => keyboardMove(flat, docs, id, cmd),
    onMove: moveBy,
    onDuplicate: startDuplicate,
    copying,
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
            slip.start(e.activatorEvent);
          }}
          onDragMove={(e) => {
            slip.moved(e.delta);
            track(e.active.id, e.delta.x);
          }}
          onDragOver={(e) => track(e.active.id, e.delta.x)}
          onDragEnd={(e) => {
            // Released less than one INDENT from the press: no drop, even if
            // the last move showed one.
            const d = slip.isNear() ? null : dropRef.current;
            const id = String(e.active.id);
            // A click on the row's link whose pointer slipped far enough to
            // start a drag: it opens the page, as the link does. The browser
            // can't: the button went up over the drag ghost, which is gone
            // by the time the click would follow, so it sends no click.
            const opens =
              !d &&
              slip.isClick() &&
              (rowLink(treeRef.current, id)?.contains(e.activatorEvent.target as Node) ?? false);
            dropped.set(d);
            endDrag();
            // No projection: a drop in place, a no-op, outside the tree, or
            // less than one INDENT from the press.
            if (d) move.mutate({ id, body: d.projection.args });
            else if (opens) {
              // As the link would, it replaces the history entry when the
              // page is already open.
              const path = `/doc/${id}`;
              void nav(path, { replace: location.pathname === path });
            }
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
      <div data-testid="tree-live" role="status" aria-live="polite" className="sr-only">
        {announcement}
      </div>
      {/* Rendered here, outside the tree, not from the row it is about: the
          row's <li> cancels every contextmenu that bubbles up from a portal
          of its own (see TreeRow.tsx), so the browser's menu would never open
          in the dialog. */}
      {dupAsk && (
        <DuplicateDialog
          title={dupAsk.title}
          subpages={dupAsk.subpages}
          pending={copying.has(dupAsk.id)}
          error={dupError}
          onCancel={() => closeDuplicate(dupAsk.id)}
          onConfirm={(include) => confirmDuplicate(dupAsk, include)}
        />
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

/** A page's row link in the tree, found by the page's id. */
function rowLink(tree: HTMLElement | null, id: string): HTMLElement | null {
  return tree?.querySelector<HTMLElement>(`[data-testid="doc-row-${id}"]`) ?? null;
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

type Point = { x: number; y: number };

/** Where a mouse or touch event is in the viewport: the mouse's position,
 *  or the first touch's (for a touchend, the one lifted). */
function pointOf(e: Event): Point | null {
  if ("touches" in e) {
    const { touches, changedTouches } = e as TouchEvent;
    const t = touches[0] ?? changedTouches[0];
    return t ? { x: t.clientX, y: t.clientY } : null;
  }
  return e instanceof MouseEvent ? { x: e.clientX, y: e.clientY } : null;
}

/** Tells a slip from a move, and a click from a drag. A click whose pointer
 *  slips past 6px (mouse), or a tap held 250 ms (touch), starts a drag.
 *  From the press, it measures two distances:
 *  - how far the pointer is from the press, in a straight line: while that
 *    is under one INDENT the drag is near, and a drop moves nothing, however
 *    the pointer got there (a hand that jitters on the way included);
 *  - how far the pointer has travelled along its whole path, through every
 *    position the drag reports, to the release: while that is under one
 *    INDENT the drag is short (and so near too).
 *  A short drag is a click, unless a modifier key or another button was
 *  pressed (the browser opens a link in a new tab or window then), or a
 *  context menu opened on the way (a long press opens the row menu on
 *  touch). A drag that goes out and comes back near is no click. */
function slipTracker() {
  const events = ["mouseup", "touchend", "contextmenu"] as const;
  let press: Point | null = null;
  let at: Point | null = null;
  let travel = 0;
  let plain = false;
  let menu = false;
  function to(p: Point) {
    if (at) travel += Math.hypot(p.x - at.x, p.y - at.y);
    at = p;
  }
  // On `window` in the capture phase: before the sensor ends the drag.
  const seen = (e: Event) => {
    if (e.type === "contextmenu") menu = true;
    const p = pointOf(e);
    if (p) to(p);
  };
  function stop() {
    for (const type of events) window.removeEventListener(type, seen, { capture: true });
  }
  // Near and short only for a press it saw: without one it can't measure.
  const isNear = () => press !== null && at !== null && Math.hypot(at.x - press.x, at.y - press.y) < INDENT;
  const isShort = () => press !== null && travel < INDENT;
  return {
    /** At drag start, with the press that started it. */
    start(e: Event) {
      stop();
      press = pointOf(e);
      at = press;
      travel = 0;
      const m = e as MouseEvent; // a TouchEvent has the same modifier keys
      plain = (!(e instanceof MouseEvent) || m.button === 0) && !m.altKey && !m.ctrlKey && !m.metaKey && !m.shiftKey;
      menu = false;
      for (const type of events) window.addEventListener(type, seen, { capture: true });
    },
    /** On each drag move, with the pointer's offset from the press. */
    moved(delta: Point) {
      if (press) to({ x: press.x + delta.x, y: press.y + delta.y });
    },
    /** The pointer is less than one INDENT from the press: a drop moves
     *  nothing. At drag end, once the release has been seen, from the
     *  release. */
    isNear,
    /** At drag end, once the release has been seen: the path from the press
     *  was under one INDENT, from a plain press, with no menu on the way. */
    isClick(): boolean {
      return isShort() && plain && !menu;
    },
    stop,
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
 *  listener. `getFlat` returns the rows on screen, `getDocs` the pages they
 *  show, `getDrop` the drop the drag ended with. The end says what the drop
 *  did, not which row it was over: an un-nest on one's own row moves the
 *  page, and a drop over another row may leave it where it was. */
export function makeAnnouncements(
  getFlat: () => FlatRow[],
  getDocs: () => Doc[],
  getDrop: () => Drop | null,
): Announcements {
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
      if (drop) return describeMove(getFlat(), getDocs(), String(active.id), drop.projection.args);
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
