import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";
import { useState, useMemo } from "react";

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

import { useUi } from "../../stores/ui";
import { ContextMenu, type ContextMenuItem } from "../../components/ContextMenu";
import { type Doc } from "../../lib/validators";
import { type ApiError } from "../../lib/api";

import { docsApi } from "./docs.api";
import { buildTree, reorderInto, type TreeNode } from "./tree";

export function DocTree() {
  const qc = useQueryClient();
  const nav = useNavigate();
  const notify = useUi((s) => s.notify);
  const { id: activeId } = useParams();

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

  function doMove(id: string, body: { parent_id?: string | null; before_id?: string; after_id?: string }) {
    move.mutate({ id, body });
  }

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
    doMove(movedId, { parent_id: targetId });
  }

  if (list.isLoading) return <div style={{ padding: 12 }}>Loading…</div>;
  if (!list.data || "error" in list.data) return <div style={{ padding: 12 }}>Failed.</div>;

  const tree = buildTree(list.data.ok);
  return (
    <div data-testid="doc-tree" style={{ padding: 12 }}>
      <header
        style={{
          display: "flex",
          justifyContent: "space-between",
          alignItems: "center",
          marginBottom: 8,
        }}
      >
        <strong>Docs</strong>
        <button
          data-testid="new-doc"
          onClick={() => create.mutate(undefined)}
          style={{ padding: "2px 8px" }}
        >
          + New
        </button>
      </header>
      {tree.length === 0 && <p style={{ color: "#888" }}>No documents yet.</p>}
      <DndContext sensors={sensors} collisionDetection={closestCenter} onDragEnd={onDragEnd}>
        <SortableContext items={flatIds} strategy={verticalListSortingStrategy}>
          <ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
            {tree.map((n) => (
              <TreeRow key={n.id} node={n} depth={0} activeId={activeId} />
            ))}
          </ul>
        </SortableContext>
      </DndContext>
      <nav style={{ marginTop: 24, borderTop: "1px solid #e5e5e5", paddingTop: 12 }}>
        <Link to="/members" style={{ display: "block", padding: 4 }}>Members</Link>
        <Link to="/settings" style={{ display: "block", padding: 4 }}>Settings</Link>
      </nav>
    </div>
  );
}

function TreeRow({
  node,
  depth,
  activeId,
}: {
  node: TreeNode;
  depth: number;
  activeId?: string;
}) {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const isActive = activeId === node.id;
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);

  const { attributes, listeners, setNodeRef, transform, isDragging } = useSortable({ id: node.id });
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

  const items: ContextMenuItem[] = [
    { label: "Rename", testId: "ctx-rename", onSelect: () => void onRename() },
    { label: "Delete", testId: "ctx-delete", destructive: true, onSelect: () => void onArchive() },
  ];

  return (
    <li ref={setNodeRef} style={sortableStyle} {...attributes} {...listeners}>
      <Link
        data-testid={`doc-row-${node.id}`}
        to={`/doc/${node.id}`}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        style={{
          display: "block",
          padding: "4px 0",
          paddingLeft: depth * 12,
          background: isActive ? "#e5e5ff" : "transparent",
          textDecoration: "none",
          color: "inherit",
        }}
      >
        {node.icon ?? "📄"} {node.title}
      </Link>
      {menu && (
        <ContextMenu x={menu.x} y={menu.y} items={items} onClose={() => setMenu(null)} />
      )}
      {node.children.length > 0 && (
        <ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
          {node.children.map((c) => (
            <TreeRow key={c.id} node={c} depth={depth + 1} activeId={activeId} />
          ))}
        </ul>
      )}
    </li>
  );
}
