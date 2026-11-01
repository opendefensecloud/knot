/**
 * /about — the running build and the release notes, newest first. The
 * notes are CHANGELOG.md as compiled into the server, so they always match
 * the version running. Releases the user hadn't seen are opened and marked.
 */

import { useQuery } from "@tanstack/react-query";
import { ChevronRight } from "lucide-react";
import { useEffect, useRef, useState } from "react";

import { sanitizeReleaseNotes } from "../../lib/sanitize";
import type { ReleaseNotes } from "../../lib/validators";

import { formatReleaseDate, shortCommit } from "./about";
import { aboutApi } from "./about.api";
import { useMarkSeen, useWhatsNew } from "./useWhatsNew";

type Release = ReleaseNotes["releases"][number];

/** Server-rendered Markdown; preflight leaves it unstyled. Long code spans
 *  and URLs wrap anywhere, as in prose.css, or a phone scrolls sideways. */
const NOTES_CLASS =
  "text-sm text-fg leading-relaxed [overflow-wrap:anywhere] [&_p]:my-2 [&_ul]:list-disc [&_ul]:pl-5 [&_ol]:list-decimal [&_ol]:pl-5 " +
  "[&_li]:my-1 [&_a]:text-accent [&_a]:underline [&_code]:font-mono [&_code]:text-[12px] " +
  "[&_code]:bg-muted [&_code]:px-1 [&_code]:rounded-sm [&_pre]:bg-muted [&_pre]:p-3 [&_pre]:rounded " +
  "[&_pre]:overflow-x-auto [&_pre_code]:p-0 [&_pre_code]:bg-transparent [&_blockquote]:border-l-2 [&_blockquote]:border-border [&_blockquote]:pl-3 " +
  "[&_table]:my-2 [&_th]:text-left [&_th]:font-semibold [&_th]:pr-4 [&_td]:pr-4 [&_td]:align-top";

export default function AboutPage() {
  const whatsNew = useWhatsNew();
  const info = whatsNew.data && "ok" in whatsNew.data ? whatsNew.data.ok : null;
  const notes = useQuery({
    // Compiled into the server binary: they change only with the version,
    // so keyed by it a tab left open across an upgrade fetches the new ones.
    queryKey: ["release-notes", info?.version],
    queryFn: () => aboutApi.releaseNotes(),
    enabled: info !== null,
    staleTime: Infinity,
  });
  const markSeen = useMarkSeen();

  // What was new when this visit began. Marking it seen empties `unseen`,
  // and the markers should stay until the user leaves the page. Taken from
  // a fresh answer only: the one cached in a tab left open across an
  // upgrade predates it, and this page's own refetch brings the real one.
  const [newThisVisit, setNewThisVisit] = useState<ReadonlySet<string> | null>(null);
  const fresh = whatsNew.isFetchedAfterMount || !whatsNew.isStale;
  if (newThisVisit === null && info && fresh) setNewThisVisit(new Set(info.unseen));

  // Once per visit: StrictMode runs this twice when the query was already
  // cached, and a failed mark brings `unseen` back on the refetch.
  const marked = useRef(false);
  useEffect(() => {
    if (marked.current || !newThisVisit || newThisVisit.size === 0) return;
    marked.current = true;
    markSeen();
  }, [newThisVisit, markSeen]);

  const released = info?.dev
    ? "Development build"
    : info?.released_on && formatReleaseDate(info.released_on);
  const releases = notes.data && "ok" in notes.data ? notes.data.ok.releases : [];
  // The notes wait on the version, so they fail with it too.
  const failed = [whatsNew.data, notes.data].some((d) => d && "error" in d);

  return (
    <section className="mx-auto max-w-[760px] px-6 py-8" data-testid="about-page">
      <header className="mb-6">
        <h1 className="text-2xl font-bold text-fg">About knot</h1>
      </header>
      {info && (
        <dl className="bg-surface border border-border rounded-lg px-5 py-4 m-0 grid grid-cols-[auto_1fr] gap-x-6 gap-y-1 text-sm">
          <dt className="text-fg-muted">Version</dt>
          <dd data-testid="about-version" className="m-0 text-fg">{info.version}</dd>
          <dt className="text-fg-muted">Build</dt>
          <dd data-testid="about-commit" className="m-0 text-fg font-mono text-[13px]">
            {shortCommit(info.commit)}
          </dd>
          {released && (
            <>
              <dt className="text-fg-muted">Released</dt>
              <dd data-testid="about-released" className="m-0 text-fg">{released}</dd>
            </>
          )}
        </dl>
      )}
      <h2 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mt-8 mb-3">
        Release notes
      </h2>
      {notes.isPending && !failed && <p className="text-sm text-fg-muted">Loading…</p>}
      {failed && (
        <p className="text-sm text-destructive">Couldn&apos;t load the release notes.</p>
      )}
      <ul className="list-none m-0 p-0 space-y-3">
        {releases.map((r, i) => (
          <li key={r.version}>
            <ReleaseEntry
              release={r}
              isNew={newThisVisit?.has(r.version) ?? false}
              open={i === 0 || (newThisVisit?.has(r.version) ?? false)}
            />
          </li>
        ))}
      </ul>
    </section>
  );
}

function ReleaseEntry({ release, isNew, open }: { release: Release; isNew: boolean; open: boolean }) {
  return (
    <details
      open={open}
      data-testid={`release-${release.version}`}
      className="group bg-surface border border-border rounded-lg"
    >
      <summary className="flex flex-wrap items-center gap-x-2 gap-y-1 px-5 py-3 cursor-pointer list-none [&::-webkit-details-marker]:hidden rounded-lg focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent">
        <ChevronRight
          size={14}
          aria-hidden
          className="text-fg-muted transition-transform duration-150 group-open:rotate-90"
        />
        <h3 className="text-base font-semibold text-fg m-0">{release.version}</h3>
        {release.date && (
          <time dateTime={release.date} className="text-[13px] text-fg-muted">
            {formatReleaseDate(release.date)}
          </time>
        )}
        {isNew && (
          <span
            data-testid="release-new"
            className="ml-auto inline-flex items-center px-2 h-5 rounded-full text-[11px] font-medium bg-accent text-accent-fg"
          >
            New since your last visit
          </span>
        )}
      </summary>
      <div className="px-5 pb-4 pt-1 border-t border-border">
        {release.sections.map((s, i) => (
          <div key={i}>
            {s.title && (
              <h4 className="text-[13px] font-semibold uppercase tracking-wider text-fg-muted mt-3 mb-1">
                {s.title}
              </h4>
            )}
            <div
              className={NOTES_CLASS}
              // Sanitized by the server too; see sanitizeReleaseNotes.
              dangerouslySetInnerHTML={{ __html: sanitizeReleaseNotes(s.html) }}
            />
          </div>
        ))}
      </div>
    </details>
  );
}
