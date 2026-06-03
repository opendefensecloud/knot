import { useQuery } from "@tanstack/react-query";
import { Copy, RefreshCw } from "lucide-react";

import { IconButton } from "../../components/ui/IconButton";
import { historyApi } from "../../lib/history.api";
import { useUi } from "../../stores/ui";

export function MarkdownView({ docId }: { docId: string }) {
  const notify = useUi((s) => s.notify);

  const q = useQuery({
    queryKey: ["doc-markdown", docId],
    queryFn: () => historyApi.exportMarkdown(docId),
    staleTime: 0,
    gcTime: 0,
  });

  const text = q.data && "ok" in q.data ? q.data.ok : "";

  return (
    <div data-testid="markdown-view" className="rounded-md border border-border bg-surface overflow-hidden">
      <div className="flex items-center gap-1 px-3 py-2 border-b border-border bg-muted/40">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-fg-muted">Markdown</span>
        <span className="text-[11px] text-fg-muted/80">read-only export</span>
        <div className="ml-auto flex items-center gap-0.5">
          <IconButton
            label="Refresh"
            size="sm"
            onClick={() => void q.refetch()}
            disabled={q.isFetching}
          >
            <RefreshCw size={14} aria-hidden className={q.isFetching ? "animate-spin" : ""} />
          </IconButton>
          <IconButton
            label="Copy"
            size="sm"
            disabled={!text}
            onClick={() => {
              void navigator.clipboard.writeText(text).then(() => notify("info", "Copied markdown"));
            }}
          >
            <Copy size={14} aria-hidden />
          </IconButton>
        </div>
      </div>
      {q.isLoading && (
        <div className="px-4 py-6 text-sm text-fg-muted">Loading markdown…</div>
      )}
      {q.data && "error" in q.data && (
        <div className="px-4 py-6 text-sm text-destructive">Couldn't load markdown.</div>
      )}
      {!q.isLoading && q.data && "ok" in q.data && (
        <pre
          data-testid="markdown-view-content"
          className="m-0 px-4 py-4 overflow-auto font-mono text-[13px] leading-relaxed text-fg whitespace-pre-wrap break-words max-h-[70vh]"
        >
          {text || <span className="text-fg-muted italic">Empty document.</span>}
        </pre>
      )}
    </div>
  );
}
