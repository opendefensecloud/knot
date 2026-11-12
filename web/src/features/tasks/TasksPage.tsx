/**
 * /tasks — open checklist items across the workspace, scoped to the current
 * user (assignee via @-mention).
 *
 * Grouped by urgency bucket (Overdue / Today / This week / Later / No date)
 * via due_at — see knot_markdown::tasks for how due dates are lifted from
 * inline `by [chip](knot://time/...)` patterns.
 *
 * The index updates automatically: the server-side reindex worker watches
 * the room actors and re-extracts tasks within a couple seconds of any
 * doc persist.
 */

import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Link } from "react-router-dom";
import { Calendar, CheckSquare, RefreshCw, Square } from "lucide-react";

import { ApiRequestError } from "../../lib/api";
import { notifyFailure } from "../../lib/limitErrors";
import { tasksApi, type Task } from "../../lib/tasks.api";
import { useUi } from "../../stores/ui";

type Bucket = "overdue" | "today" | "week" | "later" | "none";

const BUCKET_LABEL: Record<Bucket, string> = {
  overdue: "Overdue",
  today: "Today",
  week: "This week",
  later: "Later",
  none: "No date",
};

/** Choose the bucket for a task at the moment of render. */
function bucketFor(t: Task, now: Date): Bucket {
  if (!t.due_at) return "none";
  const due = new Date(t.due_at);
  if (Number.isNaN(due.getTime())) return "none";
  // Compare against the start of the next day so a task due at 23:59
  // today counts as Today and not Overdue.
  const todayStart = new Date(now);
  todayStart.setHours(0, 0, 0, 0);
  const tomorrowStart = new Date(todayStart);
  tomorrowStart.setDate(tomorrowStart.getDate() + 1);
  const weekEnd = new Date(todayStart);
  weekEnd.setDate(weekEnd.getDate() + 7);
  if (due < todayStart) return "overdue";
  if (due < tomorrowStart) return "today";
  if (due < weekEnd) return "week";
  return "later";
}

function formatDue(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleString(undefined, {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  });
}

export default function TasksPage() {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const [includeCompleted, setIncludeCompleted] = useState(false);
  const [refreshing, setRefreshing] = useState(false);

  const list = useQuery({
    queryKey: ["tasks", { includeCompleted }],
    queryFn: () => tasksApi.list(includeCompleted),
    refetchOnMount: "always",
    staleTime: 0,
  });

  const toggle = useMutation({
    // A refusal throws, so `onError` puts the checkbox back.
    mutationFn: async (t: Task) => {
      const result = await tasksApi.setChecked(t.doc_id, t.item_index, !t.checked);
      if ("error" in result) throw new ApiRequestError(result.error);
      return result;
    },
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
    onError: (error, _t, ctx) => {
      if (ctx?.prev) qc.setQueryData(["tasks", { includeCompleted }], ctx.prev);
      void qc.invalidateQueries({ queryKey: ["tasks"] });
      notifyFailure(notify, error instanceof ApiRequestError ? error.apiError : null, "Couldn't update the task.");
    },
  });

  async function onRefresh() {
    setRefreshing(true);
    try {
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
  const now = new Date();
  const buckets: Record<Bucket, Task[]> = {
    overdue: [],
    today: [],
    week: [],
    later: [],
    none: [],
  };
  for (const t of tasks) buckets[bucketFor(t, now)].push(t);
  const ORDER: Bucket[] = ["overdue", "today", "week", "later", "none"];

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
            title="Re-fetch the tasks index"
          >
            <RefreshCw size={14} className={refreshing ? "animate-spin" : ""} aria-hidden />
            Refresh
          </button>
        </div>
      </header>

      {tasks.length === 0 ? (
        <p className="text-fg-muted text-sm">
          No tasks assigned to you yet. Type <code className="px-1 rounded bg-muted">@</code> in
          a task item inside any doc to assign it. Add a due date with <code className="px-1 rounded bg-muted">by //</code>.
        </p>
      ) : (
        <ul className="space-y-8" data-testid="tasks-list">
          {ORDER.flatMap((b) => {
            const rows = buckets[b];
            if (rows.length === 0) return [];
            return [
              <li key={b} data-testid={`bucket-${b}`}>
                <h2 className={`text-sm font-semibold mb-2 ${b === "overdue" ? "text-destructive" : b === "today" ? "text-amber-500" : "text-fg-muted"}`}>
                  {BUCKET_LABEL[b]}{" "}
                  <span className="font-normal text-fg-muted">({rows.length})</span>
                </h2>
                <ul className="space-y-1.5">
                  {rows.map((t) => (
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
                      <div className="flex-1 min-w-0">
                        <Link
                          to={`/doc/${t.doc_id}`}
                          className={`text-fg hover:underline ${t.checked ? "line-through text-fg-muted" : ""}`}
                        >
                          {t.text || "(no text)"}
                        </Link>
                        <span className="ml-2 text-xs text-fg-muted">
                          in <Link to={`/doc/${t.doc_id}`} className="hover:text-fg">{t.doc_title}</Link>
                        </span>
                      </div>
                      {t.due_at && (
                        <span
                          data-testid="task-due"
                          className={`shrink-0 inline-flex items-center gap-1 text-xs px-1.5 py-0.5 rounded ${
                            b === "overdue"
                              ? "bg-destructive/15 text-destructive"
                              : b === "today"
                                ? "bg-amber-500/15 text-amber-600"
                                : "bg-muted text-fg-muted"
                          }`}
                          title={new Date(t.due_at).toLocaleString()}
                        >
                          <Calendar size={11} aria-hidden />
                          {formatDue(t.due_at)}
                        </span>
                      )}
                    </li>
                  ))}
                </ul>
              </li>,
            ];
          })}
        </ul>
      )}
    </section>
  );
}
