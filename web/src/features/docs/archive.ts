/**
 * Wording and date arithmetic for the document archive (#37). Deleting a
 * doc moves it and its subpages to the archive, where editors can restore
 * it until the retention period ends and the server purges it.
 */

function plural(n: number, one: string, many: string): string {
  return `${n} ${n === 1 ? one : many}`;
}

/** The question the sidebar asks before moving a doc to the archive. */
export function archiveConfirmText(title: string, subpageCount: number, retentionDays: number): string {
  const subpages =
    subpageCount > 0
      ? `Its ${plural(subpageCount, "subpage goes", "subpages go")} with it. `
      : "";
  return (
    `Move "${title}" to the archive?\n\n` +
    subpages +
    `It will be permanently deleted in ${plural(retentionDays, "day", "days")} unless restored.`
  );
}

/** Why moving to the archive failed, from the server's error code. */
export function archiveErrorText(code: string): string {
  if (code.startsWith("acl.")) return "You don't have permission to do that.";
  if (code === "doc.archived") return "It is already in the archive.";
  return "Couldn't move it to the archive.";
}

const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * Whole days left until `iso`, rounded up: a doc due in an hour still shows
 * "1 day". Never negative — the hourly purge job may not have reached a
 * doc that is already due.
 */
export function daysUntil(iso: string, now: Date = new Date()): number {
  return Math.max(0, Math.ceil((new Date(iso).getTime() - now.getTime()) / DAY_MS));
}

/** The question the archive asks before deleting a doc for good. */
export function purgeConfirmText(title: string, subpageCount: number): string {
  const what =
    subpageCount > 0
      ? `"${title}" and its ${plural(subpageCount, "subpage", "subpages")}`
      : `"${title}"`;
  return `Permanently delete ${what}?\n\nThis can't be undone.`;
}
