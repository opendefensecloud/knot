/**
 * Every user-facing string of the page history drawer. There is no
 * translation layer in `web/src`; a later i18n pass touches this file only.
 * `·` is U+00B7, `–` U+2013, `−` U+2212, `—` U+2014, `…` U+2026.
 */
export const copy = {
  title: "History",
  close: "Close",
  loadMore: "Load more",
  loading: "Loading…",
  back: "Back to history",
  empty: "No history yet.",
  loadError: "History couldn't be loaded.",
  retry: "Retry",
  refreshError: "History couldn't be refreshed.",
  catchingUp: "History is still being built for this page. Recent changes may be missing.",
  unavailable: "History for this page can't be updated right now. The entries below stay available.",
  earlierSection: "Earlier versions (author unknown)",
  selectEntry: "Select an entry to see the page as it was.",
  untitled: "Untitled",
  loadingVersion: "Loading version…",
  busyRetrying: "History is busy, retrying…",
  showChanges: "Show changes",
  changesUnavailable: "Changes can't be highlighted for this entry.",
  tooLarge: "This version is too large to show.",
  busy: "History is busy. Try again in a moment.",
  notFound: "This entry is no longer available.",
  cantShow: "This version can't be shown.",
  restoreHere: "Restore here",
  restoring: "Restoring…",
  restoreConfirm: "Replace the current content with this version? Formatting outside the canonical schema is not kept.",
  restored: "Restored — your editor will refresh shortly.",
  restoreFailed: "Restore failed",
  restoreMoved: "This entry changed while you were reading it. Check the new version and restore again.",
  restoreUnknown: "The restore may not have completed. Check the page before trying again.",
  rebuilding: "History for this page is being rebuilt. Versions can be opened again when it's done.",
  gap: "Changes saved without an author",
  unknownAuthor: "Unknown author",
  today: "Today",
  yesterday: "Yesterday",
  templateUnknown: "Created from a template",
} as const;

/** The line under the earlier versions: when complete history began. */
export function sinceLine(datetime: string): string {
  return `Complete history starts ${datetime}`;
}

/** A session; `counts` is null until the builder counted its words. */
export function sessionLine(
  name: string,
  span: string,
  counts: { added: number; removed: number } | null,
  open: boolean,
): string {
  if (open) return `${name} · ${span} · in progress`;
  if (counts === null) return `${name} · ${span}`;
  return `${name} · ${span} · +${counts.added} / −${counts.removed}`;
}

/** The note of a session whose author's suggestions others accepted. */
export function acceptedBy(names: string[]): string {
  return `accepted by ${names.join(", ")}`;
}

/** A restore; `target` is the restored version's time, when known. */
export function restoreLine(name: string, target: string | null): string {
  return target === null ? `Restored by ${name}` : `Restored by ${name}, to ${target}`;
}

/** The first entry of a page made by Duplicate or Move to subpage, from
 *  what its `meta` says about the source: `version` is the copied
 *  version's time, when a past version was copied; `moved` marks a page
 *  made by Move to subpage. */
export function copyLine(
  source: { sourceTitle: string | null; sourceHidden: boolean; version: string | null; moved: boolean },
  name: string,
): string {
  const { sourceTitle, sourceHidden, version, moved } = source;
  const verb = moved ? "Moved here from" : "Created from";
  if (sourceHidden) return `${verb} a page you can't open by ${name}`;
  if (sourceTitle === null) return `${verb} another page by ${name}`;
  if (!moved && version !== null) return `Created from ${sourceTitle}, version of ${version}, by ${name}`;
  return `${verb} ${sourceTitle} by ${name}`;
}

/** A Markdown import; `mode` is null when the server did not record it. */
export function importLine(mode: "replace" | "append" | null, name: string): string {
  if (mode === "replace") return `${name} replaced the content (import)`;
  if (mode === "append") return `${name} added imported content`;
  return `${name} imported content`;
}

/** A page created from a template; `title` is null when it can't be shown. */
export function templateLine(title: string | null): string {
  return title === null ? copy.templateUnknown : `Created from template ${title}`;
}

/** Content someone erased from the page's history. */
export function eraseLine(name: string): string {
  return `${name} removed content from history`;
}

/** A version from before complete history; `rollout` marks the state the
 *  page had when complete history began. */
export function baselineLine(datetime: string, rollout: boolean): string {
  return rollout ? `When complete history began · ${datetime}` : `Earlier version · ${datetime}`;
}
