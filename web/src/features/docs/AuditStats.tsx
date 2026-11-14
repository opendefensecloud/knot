/**
 * The stats at the top of the page audit log: how often the page was
 * exported, by whom, when last and in which formats, then one line for the
 * rest of its activity. Every list is stacked, so a phone never scrolls
 * sideways.
 */
import { Download } from "lucide-react";
import type { ReactNode } from "react";

import { Avatar } from "../../components/ui/Avatar";
import { relTime } from "../../lib/relTime";
import type { DocAuditStats } from "../../lib/validators";

import {
  actorLabel,
  eventCountText,
  exportCountText,
  exportFormatShortLabel,
  formatBreakdown,
  otherActivityBreakdown,
  personLabel,
  scopeSummary,
} from "./auditText";

export const AUDIT_SECTION_HEADING_CLASS = "text-sm font-semibold uppercase tracking-wider text-fg-muted m-0";

/** "2h ago", with the exact time on hover and for assistive tech. */
export function RelativeTime({ iso }: { iso: string }) {
  return (
    <time dateTime={iso} title={new Date(iso).toLocaleString()}>
      {relTime(iso)}
    </time>
  );
}

function StatTile({ label, testId, children }: { label: string; testId: string; children: ReactNode }) {
  return (
    <div className="bg-surface border border-border rounded-lg p-3 min-w-0">
      <dt className="text-[12px] text-fg-muted">{label}</dt>
      <dd data-testid={testId} className="m-0 mt-1 text-lg font-semibold text-fg break-words">
        {children}
      </dd>
    </div>
  );
}

export function AuditStats({ stats }: { stats: DocAuditStats }) {
  const exportStats = stats.exports;
  const lastExport = exportStats.last;
  const formats = formatBreakdown(exportStats.by_format);
  const scopeLine = scopeSummary(exportStats.by_scope);

  return (
    <section data-testid="audit-stats" aria-labelledby="audit-exports-heading" className="mt-8">
      <h2 id="audit-exports-heading" className={AUDIT_SECTION_HEADING_CLASS}>
        Exports
      </h2>
      <dl className="grid grid-cols-2 sm:grid-cols-4 gap-2 mt-3 mb-0">
        <StatTile label="Exports" testId="audit-stat-exports">
          {exportStats.total}
        </StatTile>
        <StatTile label="People" testId="audit-stat-people">
          {exportStats.people}
        </StatTile>
        <StatTile label="Last 30 days" testId="audit-stat-30d">
          {exportStats.last_30_days}
        </StatTile>
        <StatTile label="Last export" testId="audit-stat-last">
          {lastExport ? (
            <>
              <RelativeTime iso={lastExport.at} />
              <span className="block text-[12px] font-normal text-fg-muted">
                {exportFormatShortLabel(lastExport.format)} · {actorLabel(lastExport.actor, lastExport.actor_kind)}
              </span>
            </>
          ) : (
            "Never"
          )}
        </StatTile>
      </dl>

      {exportStats.total === 0 ? (
        <div
          data-testid="audit-exports-empty"
          className="mt-3 bg-surface border border-border rounded-lg px-6 py-8 text-center"
        >
          <Download size={24} className="mx-auto text-fg-muted mb-2" aria-hidden />
          <p className="text-sm text-fg-muted m-0">
            Nobody has exported this page since exports started being recorded.
          </p>
        </div>
      ) : (
        <>
          {formats.length > 0 && (
            <ul
              data-testid="audit-formats"
              aria-label="Exports by format"
              className="list-none m-0 mt-3 p-0 flex flex-wrap gap-1.5"
            >
              {formats.map((entry) => (
                <li
                  key={entry.format}
                  className="inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-muted text-fg-muted"
                >
                  {entry.label} {entry.count}
                </li>
              ))}
            </ul>
          )}
          {scopeLine && <p className="text-[12px] text-fg-muted m-0 mt-2">{scopeLine}</p>}

          <h3 className="text-[13px] font-medium text-fg mt-5 mb-1">Exported by</h3>
          <ul data-testid="audit-exporters" className="list-none m-0 p-0">
            {exportStats.by_person.map((exporter, index) => {
              const name = personLabel(exporter.actor);
              return (
                <li
                  key={exporter.actor.key ?? `former-${index}`}
                  className="flex items-center gap-3 py-2 border-t border-border"
                >
                  <Avatar name={name} seed={exporter.actor.key ?? "former"} size="md" />
                  <div className="min-w-0 flex-1">
                    <div className="text-sm font-medium text-fg truncate">{name}</div>
                    <div className="text-[12px] text-fg-muted">
                      {exportCountText(exporter.count)} · last <RelativeTime iso={exporter.last_at} />
                    </div>
                  </div>
                </li>
              );
            })}
          </ul>
          {exportStats.by_person_truncated && (
            <p className="text-[12px] text-fg-muted m-0 mt-1">Showing the 50 most recent exporters.</p>
          )}
        </>
      )}
    </section>
  );
}

/** One line for everything that isn't an export: "Access changes 3 · Changes 30". */
export function AuditOtherActivity({ stats }: { stats: DocAuditStats }) {
  const breakdown = otherActivityBreakdown(stats.by_action);
  return (
    <section data-testid="audit-other-activity" aria-labelledby="audit-other-heading" className="mt-8">
      <h2 id="audit-other-heading" className={AUDIT_SECTION_HEADING_CLASS}>
        Other activity
      </h2>
      <p className="text-sm text-fg m-0 mt-2">
        {breakdown.length > 0
          ? breakdown.map((entry) => `${entry.label} ${entry.count}`).join(" · ")
          : "No access or page changes recorded."}
      </p>
      <p className="text-[12px] text-fg-muted m-0 mt-1">{eventCountText(stats.total)} in total</p>
    </section>
  );
}

