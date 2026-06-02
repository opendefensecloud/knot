import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate, useParams } from "react-router-dom";

import { useUi } from "../../stores/ui";

import { docsApi } from "./docs.api";
import { buildTree, type TreeNode } from "./tree";

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
      <ul style={{ listStyle: "none", padding: 0, margin: 0 }}>
        {tree.map((n) => (
          <TreeRow key={n.id} node={n} depth={0} activeId={activeId} />
        ))}
      </ul>
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
  const isActive = activeId === node.id;
  return (
    <li>
      <Link
        data-testid={`doc-row-${node.id}`}
        to={`/doc/${node.id}`}
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
