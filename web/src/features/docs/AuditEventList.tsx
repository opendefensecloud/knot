/**
 * The events of the page audit log, newest first, one sentence each, with
 * filter chips and "Load more" for older ones.
 */
import { Clock, ShieldCheck } from "lucide-react";
import { Link } from "react-router-dom";

import { Avatar } from "../../components/ui/Avatar";
import { Button } from "../../components/ui/Button";
import type { DocAuditEvent } from "../../lib/validators";

import type { AuditCategory } from "./audit.api";
import { AUDIT_SECTION_HEADING_CLASS, RelativeTime } from "./AuditStats";
import { describeAuditEvent } from "./auditText";

const FILTERS: Array<{ category: AuditCategory; label: string; emptyText: string }> = [
  { category: "all", label: "All", emptyText: "Nothing recorded yet." },
  { category: "exports", label: "Exports", emptyText: "No exports recorded." },
  { category: "access", label: "Access", emptyText: "No access changes recorded." },
  { category: "changes", label: "Changes", emptyText: "No changes recorded." },
];

function ActorMark({ event, actorName }: { event: DocAuditEvent; actorName: string }) {
  if (event.actor_kind === "user") {
    return <Avatar name={actorName} seed={event.actor?.key ?? "former"} size="md" />;
  }
  const Icon = event.actor_kind === "admin_cli" ? ShieldCheck : Clock;
  return (
    <span
      aria-hidden
      className="inline-flex items-center justify-center h-6 w-6 rounded-full bg-muted text-fg-muted shrink-0"
    >
      <Icon size={14} />
    </span>
  );
}

function AuditEventRow({ event }: { event: DocAuditEvent }) {
  const sentence = describeAuditEvent(event);
  return (
    <li
      data-testid="audit-event"
      data-action={event.action}
      className="flex items-start gap-3 py-2 border-t border-border"
    >
      <ActorMark event={event} actorName={sentence.actor} />
      <div className="min-w-0 flex-1">
        <p className="text-sm text-fg m-0 break-words">
          <span className="font-medium">{sentence.actor}</span> {sentence.text}
          {sentence.link && (
            <>
              {" "}
              <Link to={sentence.link.to} className="text-accent hover:underline">
                {sentence.link.title}
              </Link>
            </>
          )}
          {sentence.textAfterLink && ` ${sentence.textAfterLink}`}
        </p>
        <div className="text-[12px] text-fg-muted mt-0.5">
          <RelativeTime iso={event.at} />
        </div>
      </div>
    </li>
  );
}

type Props = {
  events: DocAuditEvent[];
  category: AuditCategory;
  onCategoryChange: (category: AuditCategory) => void;
  /** The list on screen is the previous filter's while the new one loads. */
  isSwitching: boolean;
  hasNextPage: boolean;
  isFetchingNextPage: boolean;
  /** The last "Load more" failed; the button stays to try again. */
  loadMoreFailed: boolean;
  onLoadMore: () => void;
  /**
   * This filter's events didn't load (switching to it failed): the list
   * says so and offers to try again; the filters stay.
   */
  loadFailed?: boolean;
  isRetrying?: boolean;
  onRetry?: () => void;
};

export function AuditEventList({
  events,
  category,
  onCategoryChange,
  isSwitching,
  hasNextPage,
  isFetchingNextPage,
  loadMoreFailed,
  onLoadMore,
  loadFailed = false,
  isRetrying = false,
  onRetry,
}: Props) {
  const emptyText = FILTERS.find((filter) => filter.category === category)?.emptyText ?? "Nothing recorded yet.";
  return (
    <section data-testid="audit-events" aria-labelledby="audit-events-heading" className="mt-8">
      <h2 id="audit-events-heading" className={AUDIT_SECTION_HEADING_CLASS}>
        Events
      </h2>
      <div role="group" aria-label="Show events" className="flex flex-wrap gap-1 mt-3 mb-2">
        {FILTERS.map((filter) => {
          const pressed = filter.category === category;
          return (
            <button
              key={filter.category}
              type="button"
              data-testid={`audit-filter-${filter.category}`}
              aria-pressed={pressed}
              onClick={() => onCategoryChange(filter.category)}
              className={`h-7 px-2.5 rounded-full text-[13px] border transition-colors ease-swift duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
                pressed ? "bg-muted text-fg border-border" : "border-transparent text-fg-muted hover:text-fg hover:bg-muted"
              }`}
            >
              {filter.label}
            </button>
          );
        })}
      </div>
      <div aria-busy={isSwitching || isRetrying} className={isSwitching ? "opacity-60" : undefined}>
        {loadFailed ? (
          <div data-testid="audit-events-failed" role="alert" className="flex flex-wrap items-center gap-3 py-2">
            <p className="text-sm text-destructive m-0">Couldn&apos;t load these events.</p>
            <Button size="sm" data-testid="audit-events-retry" disabled={isRetrying} onClick={onRetry}>
              {isRetrying ? "Loading…" : "Try again"}
            </Button>
          </div>
        ) : events.length === 0 && !isSwitching ? (
          <p className="text-sm text-fg-muted m-0 py-2">{emptyText}</p>
        ) : (
          <ul className="list-none m-0 p-0">
            {events.map((event) => (
              <AuditEventRow key={event.id} event={event} />
            ))}
          </ul>
        )}
      </div>
      {hasNextPage && !isSwitching && !loadFailed && (
        <Button
          variant="secondary"
          size="sm"
          className="mt-3"
          data-testid="audit-load-more"
          disabled={isFetchingNextPage}
          onClick={onLoadMore}
        >
          {isFetchingNextPage ? "Loading…" : "Load more"}
        </Button>
      )}
      {loadMoreFailed && !isFetchingNextPage && (
        <p className="text-sm text-destructive m-0 mt-2">Couldn&apos;t load older events.</p>
      )}
    </section>
  );
}
