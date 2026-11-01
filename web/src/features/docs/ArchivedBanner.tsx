import { Archive } from "lucide-react";
import { Link } from "react-router-dom";

import { Button } from "../../components/ui/Button";
import type { DocWithRole } from "../../lib/validators";

function formatDate(iso: string): string {
  return new Date(iso).toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
}

/**
 * Shown above an archived doc (#37). On the doc someone archived it says
 * who and when, when it will be purged, and offers Restore (and, for
 * owners, Delete permanently). A subpage that went into the archive with
 * its folder points at that folder instead: it comes back with it. When
 * the user can't read that folder, it is not named.
 */
export function ArchivedBanner({
  doc,
  canPurge,
  busy,
  onRestore,
  onPurge,
}: {
  doc: DocWithRole;
  canPurge: boolean;
  busy: boolean;
  onRestore: () => void;
  onPurge: () => void;
}) {
  const isRoot = doc.archive_root_id === doc.id;
  return (
    <div
      data-testid="archived-banner"
      role="status"
      className="measure mt-3 flex flex-wrap items-center gap-3 rounded-md border border-border bg-muted px-4 py-3 text-sm text-fg"
    >
      <Archive size={16} className="text-fg-muted shrink-0" aria-hidden />
      {isRoot ? (
        <>
          <p className="m-0 flex-1 min-w-0">
            <span className="font-medium">In the archive.</span>{" "}
            {doc.archived_by ? `Deleted by ${doc.archived_by.display_name}` : "Deleted"}
            {doc.archived_at && ` on ${formatDate(doc.archived_at)}`}.
            {doc.purge_at && ` Permanently deleted on ${formatDate(doc.purge_at)}.`}
          </p>
          <div className="flex items-center gap-2">
            <Button size="sm" disabled={busy} onClick={onRestore}>
              Restore
            </Button>
            {canPurge && (
              <Button size="sm" variant="destructive" disabled={busy} onClick={onPurge}>
                Delete permanently
              </Button>
            )}
          </div>
        </>
      ) : doc.archive_root_id === null ? (
        // The server names no root the user can't read: no link, no title.
        <p className="m-0 flex-1 min-w-0">
          In the archive with a page above it. It comes back when that page is restored.
        </p>
      ) : (
        <p className="m-0 flex-1 min-w-0">
          Part of “
          <Link to={`/doc/${doc.archive_root_id}`} className="font-medium text-fg underline">
            {doc.archive_root_title ?? "a folder"}
          </Link>
          ”, which is in the archive. Restore it from there to bring this page back.
        </p>
      )}
    </div>
  );
}
