/**
 * The history drawer's timeline model: the rows of the entry list (day
 * headings, entries, the earlier-versions section, the line where complete
 * history starts) and each entry's wording. Times are shown in `timeZone`
 * (the browser's when undefined) with en-GB names and 24-hour clocks; every
 * English word comes from `copy.ts`.
 */
import * as v from "valibot";

import { HistoryAuthor, type HistoryEntry, type HistoryKind, type HistoryPage } from "../../../lib/validators";
import {
  acceptedBy,
  baselineLine,
  copy,
  copyLine,
  eraseLine,
  importLine,
  restoreLine,
  sessionLine,
  sinceLine,
  templateLine,
} from "./copy";

export type TimelineRow =
  /** `key` is unique in the rows (a day's heading repeats when the order
   *  interleaves days); `label` is what the heading reads. */
  | { type: "day"; key: string; label: string }
  | { type: "entry"; entry: HistoryEntry }
  | { type: "earlier" }
  | { type: "since"; label: string };

/** The entries of an infinite query's pages, each id once: the first
 *  occurrence wins (an open session that moved above a used cursor shows up
 *  again on a later page), in page order. */
export function dedupeEntries(pages: HistoryPage[]): HistoryEntry[] {
  const seen = new Set<number>();
  const out: HistoryEntry[] = [];
  for (const page of pages) {
    for (const entry of page.entries) {
      if (seen.has(entry.id)) continue;
      seen.add(entry.id);
      out.push(entry);
    }
  }
  return out;
}

/**
 * The timeline's rows for the loaded entries (log entries newest first, then
 * the baselines): a day heading before the first log entry and whenever the
 * local day of an entry's `ended_at` differs from the previous log entry's;
 * one `earlier` row before the first baseline, which gets no day headings;
 * the `since` line last, once every page is loaded (`exhausted`) of a page
 * whose history is not complete.
 */
export function timelineRows(
  entries: HistoryEntry[],
  o: { now: Date; timeZone?: string; historySince: string; historyComplete: boolean; exhausted: boolean },
): TimelineRow[] {
  const rows: TimelineRow[] = [];
  let previousDay: string | null = null;
  let inEarlier = false;
  for (const entry of entries) {
    if (entry.kind === "baseline") {
      if (!inEarlier) rows.push({ type: "earlier" });
      inEarlier = true;
    } else {
      const ended = new Date(entry.ended_at);
      const day = localDay(ended, o.timeZone).key;
      if (day !== previousDay) {
        rows.push({ type: "day", key: `${day}#${entry.id}`, label: dayLabel(ended, o.now, o.timeZone) });
        previousDay = day;
      }
    }
    rows.push({ type: "entry", entry });
  }
  if (!o.historyComplete && o.exhausted) {
    rows.push({ type: "since", label: sinceLine(formatDateTime(o.historySince, o.timeZone)) });
  }
  return rows;
}

/** `Today`, `Yesterday`, else `Mon 2 Nov`, with the year appended when it is
 *  not the current one (`Mon 3 Nov 2025`), all in `timeZone`. */
export function dayLabel(d: Date, now: Date, timeZone?: string): string {
  const day = localDay(d, timeZone);
  const today = localDay(now, timeZone);
  if (day.key === today.key) return copy.today;
  if (day.key === previousDayKey(today)) return copy.yesterday;
  const p = parts(d, timeZone, { weekday: "short", day: "numeric", month: "short" });
  const label = `${p.weekday} ${p.day} ${p.month}`;
  return day.year === today.year ? label : `${label} ${day.year}`;
}

/** `09:12–09:40`; when the start is on another local day than the end, the
 *  start is prefixed with that day's heading (`Mon 2 Nov 23:50–00:10`). */
export function formatSpan(startedAt: string, endedAt: string, now: Date, timeZone?: string): string {
  const start = new Date(startedAt);
  const end = new Date(endedAt);
  const span = `${clock(start, timeZone)}–${clock(end, timeZone)}`;
  if (localDay(start, timeZone).key === localDay(end, timeZone).key) return span;
  return `${dayLabel(start, now, timeZone)} ${span}`;
}

/** `2 Nov 2026, 09:40`. */
export function formatDateTime(iso: string, timeZone?: string): string {
  const d = new Date(iso);
  const p = parts(d, timeZone, { day: "numeric", month: "short", year: "numeric" });
  return `${p.day} ${p.month} ${p.year}, ${clock(d, timeZone)}`;
}

/** The author's name, or `Unknown author` for no author or no name. */
export function authorName(a: HistoryAuthor | null): string {
  return a?.name ?? copy.unknownAuthor;
}

/** What an entry's row says. */
export function entryLine(e: HistoryEntry, now: Date, timeZone?: string): string {
  const name = authorName(e.author);
  const m = e.meta;
  switch (e.kind) {
    case "session": {
      const counts =
        e.words_added !== null && e.words_removed !== null
          ? { added: e.words_added, removed: e.words_removed }
          : null;
      return sessionLine(name, formatSpan(e.started_at, e.ended_at, now, timeZone), counts, e.open);
    }
    case "restore": {
      const target = text(m, "target_ended_at");
      return restoreLine(name, target === null ? null : formatDateTime(target, timeZone));
    }
    case "copy": {
      const version = text(m, "version_at");
      return copyLine(
        {
          sourceTitle: text(m, "source_title"),
          sourceHidden: m.source_hidden === true,
          version: version === null ? null : formatDateTime(version, timeZone),
          moved: m.moved === true,
        },
        name,
      );
    }
    case "import": {
      const mode = m.mode === "replace" || m.mode === "append" ? m.mode : null;
      return importLine(mode, name);
    }
    case "template":
      return templateLine(m.template_hidden === true ? null : text(m, "template_title"));
    case "erase":
      return eraseLine(name);
    case "gap":
      return copy.gap;
    case "baseline":
      return baselineLine(formatDateTime(e.ended_at, timeZone), m.rollout === true);
  }
}

const acceptedAuthor = v.nullable(HistoryAuthor);

/** The line under an entry's: for a restore that replaced the whole page
 *  (`meta.fallback`), that comments may have lost their place; for a
 *  session whose suggestions others accepted, `accepted by Bea, Unknown
 *  author` (`meta.accepted_by`: the list's author objects, null for a
 *  person it could not load); else null. */
export function entryNote(e: HistoryEntry): string | null {
  if (e.kind === "restore") return e.meta.fallback === true ? copy.restoreFallbackNote : null;
  const accepted = e.meta.accepted_by;
  if (!Array.isArray(accepted) || accepted.length === 0) return null;
  const names = accepted.map((el: unknown) => {
    const parsed = v.safeParse(acceptedAuthor, el);
    return parsed.success ? authorName(parsed.output) : copy.unknownAuthor;
  });
  return acceptedBy(names);
}

/** Whether an entry of `kind` can have its changes highlighted: a baseline
 *  has no previous version to compare with, and an erase's view is the
 *  rewritten page (the client mirror of the server's `view::shows_changes`). */
export function showsChanges(kind: HistoryKind): boolean {
  return kind !== "baseline" && kind !== "erase";
}

/** A non-empty string at `key` of an entry's meta, else null. */
function text(meta: Record<string, unknown>, key: string): string | null {
  const value = meta[key];
  return typeof value === "string" && value !== "" ? value : null;
}

type Parts = Partial<Record<Intl.DateTimeFormatPartTypes, string>>;

function parts(d: Date, timeZone: string | undefined, options: Intl.DateTimeFormatOptions): Parts {
  const out: Parts = {};
  for (const p of new Intl.DateTimeFormat("en-GB", { ...options, timeZone }).formatToParts(d)) {
    out[p.type] = p.value;
  }
  return out;
}

/** `09:40`, 24-hour. */
function clock(d: Date, timeZone: string | undefined): string {
  const p = parts(d, timeZone, { hour: "2-digit", minute: "2-digit", hourCycle: "h23" });
  return `${p.hour}:${p.minute}`;
}

/** The local calendar day of `d`: `key` is `YYYY-MM-DD`. */
function localDay(d: Date, timeZone: string | undefined): { key: string; year: string } {
  const p = parts(d, timeZone, { year: "numeric", month: "2-digit", day: "2-digit" });
  const year = p.year ?? "";
  return { key: `${year}-${p.month}-${p.day}`, year };
}

/** The key of the calendar day before `day`. */
function previousDayKey(day: { key: string }): string {
  const [y, m, d] = day.key.split("-").map(Number);
  return new Date(Date.UTC(y ?? 0, (m ?? 1) - 1, (d ?? 1) - 1)).toISOString().slice(0, 10);
}
