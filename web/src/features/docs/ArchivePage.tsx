/**
 * /archive — documents moved to the archive (#37). Each row is a doc
 * someone deleted, with the subpages that went with it. Editors can open a
 * read-only preview and restore; owners can also delete permanently ahead
 * of the retention period, after which the server purges it anyway.
 */

import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Archive } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";

import { useEffectiveRole } from "../../auth/useEffectiveRole";
import { Button } from "../../components/ui/Button";
import type { ArchiveEntry } from "../../lib/validators";
import { useUi } from "../../stores/ui";

import { archiveErrorText, daysUntil, purgeConfirmText, purgeErrorText } from "./archive";
import { docsApi } from "./docs.api";

function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
}

/** Where the page was: its parent, or the top level. A parent the user
 *  can't see is not named. */
function parentText(entry: ArchiveEntry): string {
  if (entry.parent_title) return `in ${entry.parent_title}`;
  if (entry.parent_hidden || entry.parent_id !== null) return "in a page you can't see";
  return "top level";
}

export default function ArchivePage() {
  const qc = useQueryClient();
  const notify = useUi((s) => s.notify);
  const { workspace: role } = useEffectiveRole();
  /** Permanent deletion takes an owner of the page: the role on it when the
   *  archive says, else the workspace role. */
  const canPurge = (entry: ArchiveEntry) => (entry.effective_role ?? role) === "owner";
  // One action at a time per row; the id of the row whose action is running.
  const [busyId, setBusyId] = useState<string | null>(null);

  const archive = useQuery({
    queryKey: ["archive"],
    queryFn: () => docsApi.listArchive(),
    refetchOnMount: "always",
    staleTime: 0,
  });
  const entries = archive.data && "ok" in archive.data ? archive.data.ok : [];

  async function afterChange() {
    await qc.invalidateQueries({ queryKey: ["archive"] });
    await qc.invalidateQueries({ queryKey: ["docs"] });
    await qc.invalidateQueries({ queryKey: ["doc"] });
  }

  async function restore(entry: ArchiveEntry) {
    setBusyId(entry.id);
    const r = await docsApi.restore(entry.id);
    setBusyId(null);
    if ("error" in r) {
      notify(
        "error",
        r.error.code === "doc.restore_restricted_parent_archived"
          ? archiveErrorText(r.error.code)
          : `Couldn't restore "${entry.title}"`,
      );
      return;
    }
    notify("info", `Restored "${entry.title}"`);
    await afterChange();
  }

  async function purge(entry: ArchiveEntry) {
    if (!window.confirm(purgeConfirmText(entry.title, entry.subpage_count))) return;
    setBusyId(entry.id);
    const r = await docsApi.purge(entry.id);
    setBusyId(null);
    if ("error" in r) {
      notify("error", purgeErrorText(r.error.code, `Couldn't delete "${entry.title}"`));
      return;
    }
    notify("info", `Deleted "${entry.title}" permanently`);
    await afterChange();
  }

  return (
    <section className="mx-auto max-w-[760px] px-6 py-8" data-testid="archive-page">
      <header className="mb-6">
        <h1 className="text-2xl font-bold text-fg">Archive</h1>
        <p className="text-sm text-fg-muted mt-1">
          Deleted documents stay here, with their subpages, until they are deleted permanently.
          Open one to look at it, or restore it to put it back where it was.
        </p>
      </header>

      {archive.isLoading && <p className="text-fg-muted text-sm">Loading…</p>}

      {archive.data && "error" in archive.data && (
        <p className="text-sm text-destructive">Couldn't load the archive.</p>
      )}

      {archive.data && "ok" in archive.data && entries.length === 0 && (
        <div className="bg-surface border border-border rounded-lg px-6 py-10 text-center">
          <Archive size={28} className="mx-auto text-fg-muted mb-3" aria-hidden />
          <p className="text-sm text-fg-muted m-0">Nothing in the archive.</p>
        </div>
      )}

      {entries.length > 0 && (
        <ul className="list-none m-0 p-0 flex flex-col gap-2" data-testid="archive-list">
          {entries.map((entry) => {
            const days = daysUntil(entry.purge_at);
            const busy = busyId === entry.id;
            return (
              <li
                key={entry.id}
                data-testid={`archive-row-${entry.id}`}
                className="bg-surface border border-border rounded-lg p-4 flex flex-wrap items-center gap-3"
              >
                <div className="min-w-0 flex-1">
                  <div className="text-sm font-medium text-fg truncate">{entry.title}</div>
                  <div className="text-xs text-fg-muted mt-1">
                    {parentText(entry)}
                    {entry.subpage_count > 0 &&
                      ` · +${entry.subpage_count} ${entry.subpage_count === 1 ? "subpage" : "subpages"}`}
                  </div>
                  <div className="text-xs text-fg-muted mt-0.5">
                    {/* No deleter for pages deleted before the archive
                        existed, or by someone since removed from knot. */}
                    {entry.archived_by
                      ? `Deleted by ${entry.archived_by.display_name} on ${formatDate(entry.archived_at)}`
                      : `Deleted on ${formatDate(entry.archived_at)}`}
                    {" · "}
                    {entry.purging
                      ? "Being deleted permanently"
                      : `Deleted permanently in ${days} ${days === 1 ? "day" : "days"}`}
                  </div>
                </div>
                <div className="flex items-center gap-2">
                  <Link
                    to={`/doc/${entry.id}`}
                    className="inline-flex items-center h-7 px-2 rounded text-[13px] font-medium text-fg hover:bg-muted no-underline"
                  >
                    Open
                  </Link>
                  {/* Its attachments may already be partly gone. */}
                  {!entry.purging && (
                    <Button size="sm" disabled={busy} onClick={() => void restore(entry)}>
                      Restore
                    </Button>
                  )}
                  {canPurge(entry) && (
                    <Button
                      size="sm"
                      variant="destructive"
                      disabled={busy}
                      onClick={() => void purge(entry)}
                    >
                      Delete permanently
                    </Button>
                  )}
                </div>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
