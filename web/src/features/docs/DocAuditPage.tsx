/**
 * /doc/:id/audit — the page's audit log, for its editors and owners (by
 * their role on this page), also while it is archived: export stats first,
 * then every audited event in plain sentences, then what is and isn't
 * recorded. The server is the guard; a viewer is told without asking it.
 */
import { useQuery } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import { Link, useParams } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { Button } from "../../components/ui/Button";
import type { DocAuditStats } from "../../lib/validators";

import type { AuditCategory } from "./audit.api";
import { AuditEventList } from "./AuditEventList";
import { AuditOtherActivity, AuditStats } from "./AuditStats";
import { Breadcrumb } from "./Breadcrumb";
import { docsApi } from "./docs.api";
import { useDocAudit } from "./useDocAudit";

const NO_ACCESS_CODES = new Set(["acl.no_grant", "doc.not_found"]);

function errorCode(error: unknown): string | null {
  if (typeof error !== "object" || error === null || !("code" in error)) return null;
  return typeof error.code === "string" ? error.code : null;
}

function NoticeCard({ testId, children }: { testId: string; children: ReactNode }) {
  return (
    <div data-testid={testId} className="mt-8 bg-surface border border-border rounded-lg px-6 py-8 text-center">
      {children}
    </div>
  );
}

export default function DocAuditPage() {
  const { id = "" } = useParams<{ id: string }>();
  const [category, setCategory] = useState<AuditCategory>("all");

  // The same query DocPage and useEffectiveRole use, so it is usually cached.
  const docQuery = useQuery({
    queryKey: ["doc", id],
    queryFn: () => docsApi.get(id),
    enabled: Boolean(id),
  });
  const { doc: effectiveRole } = useEffectiveRole(id);
  const docMeta = docQuery.data && "ok" in docQuery.data ? docQuery.data.ok : null;
  // Wait for the page's role so a viewer's browser never asks; a page the
  // user can't load still asks, and the server answers for it.
  const isViewer = effectiveRole === "viewer";
  const audit = useDocAudit(id, category, Boolean(id) && !docQuery.isPending && !isViewer);

  const failureCode = audit.isError ? errorCode(audit.error) : null;
  const forbidden = isViewer || failureCode === "acl.editor_required";
  const noAccess = failureCode !== null && NO_ACCESS_CODES.has(failureCode);
  const firstPage = audit.data?.pages[0];
  const loadedStats = firstPage?.stats ?? null;
  // The stats last shown for this page, which don't depend on the filter:
  // when switching the filter fails, they and the filters stay on screen
  // (the failed list says so and offers to try again), so the user can go
  // back to a filter that loaded.
  const [lastStats, setLastStats] = useState<{ docId: string; stats: DocAuditStats } | null>(null);
  if (loadedStats !== null && (lastStats?.docId !== id || lastStats.stats !== loadedStats)) {
    setLastStats({ docId: id, stats: loadedStats });
  }
  const earlierStats = lastStats?.docId === id ? lastStats.stats : null;
  const switchFailed = audit.isError && !audit.data && earlierStats !== null;
  const stats = loadedStats ?? (switchFailed ? earlierStats : null);
  const events = audit.data?.pages.flatMap((auditPage) => auditPage.events) ?? [];
  const isArchived = Boolean(docMeta?.archived_at ?? docMeta?.archived);

  let body: ReactNode;
  if (forbidden) {
    body = (
      <NoticeCard testId="audit-forbidden">
        <p className="text-sm text-fg m-0">Only editors and owners of this page can see its audit log.</p>
        <Link to={`/doc/${id}`} className="inline-block mt-3 text-sm text-accent hover:underline">
          Back to page
        </Link>
      </NoticeCard>
    );
  } else if (noAccess) {
    body = (
      <NoticeCard testId="audit-no-access">
        <p className="text-sm text-fg m-0">This page doesn&apos;t exist or you don&apos;t have access to it.</p>
      </NoticeCard>
    );
  } else if (audit.isError && !audit.data && !switchFailed) {
    body = (
      <div className="mt-8 flex flex-wrap items-center gap-3">
        <p className="text-sm text-destructive m-0">Couldn&apos;t load the audit log.</p>
        <Button size="sm" data-testid="audit-retry" onClick={() => void audit.refetch()}>
          Try again
        </Button>
      </div>
    );
  } else if (!audit.data && !switchFailed) {
    body = <p className="text-sm text-fg-muted mt-8">Loading…</p>;
  } else {
    body = (
      <>
        {stats && <AuditStats stats={stats} />}
        {stats && <AuditOtherActivity stats={stats} />}
        <AuditEventList
          events={events}
          category={category}
          onCategoryChange={setCategory}
          isSwitching={audit.isPlaceholderData}
          hasNextPage={audit.hasNextPage}
          isFetchingNextPage={audit.isFetchingNextPage}
          loadMoreFailed={audit.isFetchNextPageError}
          onLoadMore={() => void audit.fetchNextPage()}
          loadFailed={switchFailed}
          isRetrying={switchFailed && audit.isFetching}
          onRetry={() => void audit.refetch()}
        />
        <p data-testid="audit-scope-note" className="text-[12px] text-fg-muted border-t border-border pt-4 mt-8 mb-0">
          Recorded: downloads as Word, OpenDocument and Markdown (zip), also as part of a subpage or
          workspace export, and access and page changes. Not recorded: opening or reading the page, the
          Markdown view and its Copy button, attachments, and public link views. Exports made before this
          version of knot were not recorded.
        </p>
      </>
    );
  }

  return (
    <section className="mx-auto max-w-[760px] px-4 sm:px-6 py-8" data-testid="doc-audit-page">
      <Breadcrumb
        items={
          docMeta
            ? [{ title: "Documents" }, { id, title: docMeta.title }, { title: "Audit log" }]
            : [{ title: "Documents" }, { title: "Audit log" }]
        }
      />
      <header className="mt-3">
        <div className="flex flex-wrap items-center gap-2">
          <h1 className="text-2xl font-bold text-fg m-0">Audit log</h1>
          {isArchived && (
            <span
              data-testid="audit-archived"
              className="inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-muted text-fg-muted"
            >
              Archived
            </span>
          )}
        </div>
        <p className="text-sm text-fg-muted mt-1 mb-0">
          Who exported this page, changed its access or changed it. Only its editors and owners see this.
        </p>
      </header>
      {body}
    </section>
  );
}
