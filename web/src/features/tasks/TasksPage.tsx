/**
 * /tasks — open checklist items across the workspace, scoped to the current
 * user (assignee via @-mention).
 *
 * The index is server-side eager and refreshes on markdown export; a
 * "Refresh" button on this page triggers a markdown export for each doc the
 * user can edit, which is what populates the index.
 */

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { CheckSquare, RefreshCw, Square } from "lucide-react";

import { tasksApi, type Task } from "../../lib/tasks.api";
import { docsApi } from "../docs/docs.api";

async function refreshIndex(docs: { id: string }[]) {
  // Tickle each doc's markdown export which triggers the server-side
  // re-extract. Best-effort: ignore failures (likely ACL).
  await Promise.allSettled(
    docs.map((d) =>
      fetch(`/api/docs/${encodeURIComponent(d.id)}/markdown`, {
        credentials: "include",
      }),
    ),
  );
}

export default function TasksPage() {
  const qc = useQueryClient();
  const [includeCompleted, setIncludeCompleted] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const list = useQuery({
    queryKey: ["tasks", { includeCompleted }],
    queryFn: () => tasksApi.list(includeCompleted),
    staleTime: 10_000,
  });

  const allDocs = useQuery({
    queryKey: ["docs"],
    queryFn: () => docsApi.list(),
    staleTime: 60_000,
  });

  // Optimistic check/uncheck: flip locally on click, then PATCH the source
  // doc. On any error, refetch to roll back.
  const toggle = useMutation({
    mutationFn: async (t: Task) => tasksApi.setChecked(t.doc_id, t.item_index, !t.checked),
    onMutate: async (t: Task) => {
      const key = ["tasks", { includeCompleted }] as const;
      await qc.cancelQueries({ queryKey: key });
      const prev = qc.getQueryData<ReturnType<typeof tasksApi.list> extends Promise<infer R> ? R : never>(key);
      qc.setQueryData(key, (curr: unknown) => {
        if (!curr || typeof curr !== "object" || !("ok" in curr)) return curr;
        const ok = (curr as { ok: Task[] }).ok;
        return {
          ok: ok.map((x) => (x.id === t.id ? { ...x, checked: !x.checked } : x)),
        };
      });
      return { prev };
    },
    onError: (_e, _t, ctx) => {
      if (ctx?.prev) qc.setQueryData(["tasks", { includeCompleted }], ctx.prev);
      // Refetch only when the optimistic write fails, so the server can
      // reconcile. On success we trust our local flip — the server-side
      // reindex (which runs after the room actor flushes) eventually
      // catches up via the next manual Refresh or page reload.
      void qc.invalidateQueries({ queryKey: ["tasks"] });
    },
  });

  async function onRefresh() {
    setRefreshing(true);
    try {
      if (allDocs.data && "ok" in allDocs.data) {
        await refreshIndex(allDocs.data.ok.map((d) => ({ id: d.id })));
      }
      await qc.invalidateQueries({ queryKey: ["tasks"] });
    } finally {
      setRefreshing(false);
    }
  }

  if (list.isLoading) {
    return <main className="mx-auto max-w-[760px] px-6 py-8 text-fg-muted">Loading…</main>;
  }
  if (!list.data || "error" in list.data) {
    return <main className="mx-auto max-w-[760px] px-6 py-8 text-fg-muted">Failed to load tasks.</main>;
  }
  const tasks = list.data.ok;
  const byDoc = groupBy(tasks, (t) => t.doc_id);

  return (
    <section className="mx-auto max-w-[760px] px-6 py-8" data-testid="tasks-page">
      <header className="mb-6 flex items-center justify-between gap-3">
        <h1 className="text-2xl font-bold text-fg">My tasks</h1>
        <div className="flex items-center gap-2">
          <label className="inline-flex items-center gap-2 text-sm text-fg-muted cursor-pointer">
            <input
              type="checkbox"
              checked={includeCompleted}
              onChange={(e) => setIncludeCompleted(e.target.checked)}
              className="rounded border-border"
              data-testid="tasks-include-completed"
            />
            Show completed
          </label>
          <button
            type="button"
            data-testid="tasks-refresh"
            disabled={refreshing}
            onClick={() => { void onRefresh(); }}
            className="inline-flex items-center gap-1.5 h-8 px-3 rounded border border-border bg-surface text-fg-muted hover:text-fg hover:bg-muted text-sm transition-colors disabled:opacity-50"
            title="Re-index tasks from every doc you can read"
          >
            <RefreshCw size={14} className={refreshing ? "animate-spin" : ""} aria-hidden />
            Refresh
          </button>
        </div>
      </header>

      {tasks.length === 0 ? (
        <p className="text-fg-muted text-sm">
          No tasks assigned to you yet. Type <code className="px-1 rounded bg-muted">@</code> in
          a task item inside any doc to assign it.
        </p>
      ) : (
        <ul className="space-y-6" data-testid="tasks-list">
          {Object.entries(byDoc).map(([docId, group]) => (
            <li key={docId}>
              <h2 className="text-sm font-semibold text-fg-muted mb-2">
                <Link to={`/doc/${docId}`} className="hover:text-fg">
                  {group[0]?.doc_title ?? "Untitled"}
                </Link>
              </h2>
              <ul className="space-y-1.5">
                {group.map((t) => (
                  <li key={t.id} className="flex items-start gap-2 text-sm" data-testid="task-row">
                    <button
                      type="button"
                      aria-label={t.checked ? "Mark as not done" : "Mark as done"}
                      data-testid="task-checkbox"
                      onClick={() => toggle.mutate(t)}
                      className="mt-0.5 shrink-0"
                    >
                      {t.checked ? (
                        <CheckSquare size={16} className="text-accent" aria-hidden />
                      ) : (
                        <Square size={16} className="text-fg-muted hover:text-fg" aria-hidden />
                      )}
                    </button>
                    <Link
                      to={`/doc/${docId}`}
                      className={`flex-1 text-fg hover:underline ${t.checked ? "line-through text-fg-muted" : ""}`}
                    >
                      {t.text}
                    </Link>
                  </li>
                ))}
              </ul>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function groupBy<T>(arr: T[], key: (x: T) => string): Record<string, T[]> {
  const out: Record<string, T[]> = {};
  for (const x of arr) {
    const k = key(x);
    (out[k] ??= []).push(x);
  }
  return out;
}
