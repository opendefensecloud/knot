import { describe, expect, it } from "vitest";

import { HistoryEntry, type HistoryKind, type HistoryPage, parse } from "../../../lib/validators";
import { copy } from "./copy";
import {
  dayLabel,
  dedupeEntries,
  entryLine,
  entryNote,
  formatSpan,
  showsChanges,
  timelineRows,
  type TimelineRow,
} from "./timeline";

const TZ = "Europe/Berlin";
const NOW = new Date("2026-11-08T10:00:00Z");

/** An entry as the list API sends it, parsed by the client's schema:
 *  by default Ann's sealed session 09:12–09:40 Berlin time today. */
function entry(over: Record<string, unknown> = {}): HistoryEntry {
  return parse(HistoryEntry, {
    id: 1,
    kind: "session",
    author: { key: "u-ann", name: "Ann", former: false },
    started_at: "2026-11-08T08:12:00Z",
    ended_at: "2026-11-08T08:40:00Z",
    open: false,
    words_added: null,
    words_removed: null,
    meta: {},
    ...over,
  });
}

function page(entries: HistoryEntry[], over: Partial<HistoryPage> = {}): HistoryPage {
  return {
    history_since: "2026-11-02T08:40:00Z",
    history_complete: true,
    catching_up: false,
    unavailable: false,
    entries,
    next_cursor: null,
    ...over,
  };
}

function line(over: Record<string, unknown>): string {
  return entryLine(entry(over), NOW, TZ);
}

function shape(rows: TimelineRow[]): string[] {
  return rows.map((r) => (r.type === "day" ? `day ${r.label}` : r.type === "entry" ? `entry ${r.entry.id}` : r.type));
}

describe("dayLabel", () => {
  it("labels days as Today, Yesterday, then weekday day month", () => {
    expect(dayLabel(new Date("2026-11-08T07:00:00Z"), NOW, TZ)).toBe("Today");
    expect(dayLabel(new Date("2026-11-07T12:00:00Z"), NOW, TZ)).toBe("Yesterday");
    expect(dayLabel(new Date("2026-11-02T12:00:00Z"), NOW, TZ)).toBe("Mon 2 Nov");
    expect(dayLabel(new Date("2025-11-03T12:00:00Z"), NOW, TZ)).toBe("Mon 3 Nov 2025");
  });

  it("files an entry under the local day it ended", () => {
    const late = new Date("2026-11-07T23:30:00Z");
    expect(dayLabel(late, NOW, TZ)).toBe("Today");
    expect(dayLabel(late, NOW, "UTC")).toBe("Yesterday");

    const rows = timelineRows([entry({ ended_at: "2026-11-07T23:30:00Z", started_at: "2026-11-07T23:10:00Z" })], {
      now: NOW,
      timeZone: TZ,
      historySince: "2026-11-01T00:00:00Z",
      historyComplete: true,
      exhausted: true,
    });
    expect(shape(rows)).toEqual(["day Today", "entry 1"]);
  });

  it("handles the DST change", () => {
    // 2026-10-25 is the night Berlin goes from CEST (UTC+2) to CET (UTC+1).
    expect(dayLabel(new Date("2026-10-25T00:30:00Z"), NOW, TZ)).toBe("Sun 25 Oct");
    expect(dayLabel(new Date("2026-10-25T23:30:00Z"), NOW, TZ)).toBe("Mon 26 Oct");
  });
});

describe("timelineRows", () => {
  const opts = {
    now: NOW,
    timeZone: TZ,
    historySince: "2026-11-02T08:40:00Z",
    historyComplete: true,
    exhausted: true,
  };

  it("puts one day heading before each run of entries of the same day", () => {
    const rows = timelineRows(
      [
        entry({ id: 3, ended_at: "2026-11-08T09:00:00Z" }),
        entry({ id: 2, ended_at: "2026-11-08T08:00:00Z" }),
        entry({ id: 1, ended_at: "2026-11-07T08:00:00Z" }),
      ],
      opts,
    );
    expect(shape(rows)).toEqual(["day Today", "entry 3", "entry 2", "day Yesterday", "entry 1"]);
  });

  it("repeats a day heading when the order interleaves days", () => {
    const rows = timelineRows(
      [
        entry({ id: 3, ended_at: "2026-11-08T09:00:00Z" }),
        entry({ id: 2, ended_at: "2026-11-07T09:00:00Z" }),
        entry({ id: 1, ended_at: "2026-11-08T08:00:00Z" }),
      ],
      opts,
    );
    expect(shape(rows)).toEqual(["day Today", "entry 3", "day Yesterday", "entry 2", "day Today", "entry 1"]);
    const keys = rows.filter((r) => r.type === "day").map((r) => (r.type === "day" ? r.key : ""));
    expect(new Set(keys).size).toBe(keys.length);
  });

  it("puts earlier versions after the log and the since line last", () => {
    const entries = [
      entry({ id: 2 }),
      entry({
        id: 1,
        kind: "baseline",
        author: null,
        started_at: "2026-10-01T08:00:00Z",
        ended_at: "2026-10-01T08:00:00Z",
        meta: { rollout: false },
      }),
    ];
    const rows = timelineRows(entries, { ...opts, historyComplete: false, exhausted: true });
    expect(shape(rows)).toEqual(["day Today", "entry 2", "earlier", "entry 1", "since"]);
    expect(rows[4]).toEqual({ type: "since", label: "Complete history starts 2 Nov 2026, 09:40" });

    expect(shape(timelineRows(entries, { ...opts, historyComplete: true, exhausted: true }))).not.toContain("since");
    expect(shape(timelineRows(entries, { ...opts, historyComplete: false, exhausted: false }))).not.toContain("since");
  });

  it("puts a single earlier row before several baselines", () => {
    const baseline = (id: number) =>
      entry({ id, kind: "baseline", author: null, ended_at: "2026-10-0" + id + "T08:00:00Z", meta: { rollout: false } });
    const rows = timelineRows([baseline(3), baseline(2), baseline(1)], { ...opts, historyComplete: false, exhausted: false });
    expect(shape(rows)).toEqual(["earlier", "entry 3", "entry 2", "entry 1"]);
  });

  it("is empty for no entries, apart from the since line", () => {
    expect(timelineRows([], opts)).toEqual([]);
    expect(shape(timelineRows([], { ...opts, historyComplete: false }))).toEqual(["since"]);
  });
});

describe("entryLine", () => {
  it("words every kind as the design says", () => {
    // Sessions.
    expect(line({ words_added: 120, words_removed: 15 })).toBe("Ann · 09:12–09:40 · +120 / −15");
    expect(line({})).toBe("Ann · 09:12–09:40");
    expect(line({ open: true, words_added: 3, words_removed: 0 })).toBe("Ann · 09:12–09:40 · in progress");

    // Restore.
    expect(line({ kind: "restore", meta: { target_ended_at: "2026-11-02T08:40:00Z", target_entry_id: 4 } })).toBe(
      "Restored by Ann, to 2 Nov 2026, 09:40",
    );
    expect(line({ kind: "restore", meta: { target_ended_at: null, target_entry_id: null, fallback: null } })).toBe(
      "Restored by Ann",
    );

    // Copy.
    expect(line({ kind: "copy", meta: { source_doc_id: "d2", source_title: "Plan", version_at: null } })).toBe(
      "Created from Plan by Ann",
    );
    expect(
      line({ kind: "copy", meta: { source_doc_id: "d2", source_title: "Plan", version_at: "2026-11-02T08:40:00Z" } }),
    ).toBe("Created from Plan, version of 2 Nov 2026, 09:40, by Ann");
    expect(line({ kind: "copy", meta: { source_hidden: true } })).toBe("Created from a page you can't open by Ann");
    expect(line({ kind: "copy", meta: { source_doc_id: "d2", source_title: null, version_at: null } })).toBe(
      "Created from another page by Ann",
    );
    expect(line({ kind: "copy", meta: { moved: true, moved_from: "d2", source_title: "Plan" } })).toBe(
      "Moved here from Plan by Ann",
    );
    expect(line({ kind: "copy", meta: { moved: true, moved_from: "d2", source_title: null } })).toBe(
      "Moved here from another page by Ann",
    );
    expect(line({ kind: "copy", meta: { moved: true, source_hidden: true } })).toBe(
      "Moved here from a page you can't open by Ann",
    );

    // Import.
    expect(line({ kind: "import", meta: { mode: "replace" } })).toBe("Ann replaced the content (import)");
    expect(line({ kind: "import", meta: { mode: "append" } })).toBe("Ann added imported content");
    expect(line({ kind: "import", meta: { mode: null } })).toBe("Ann imported content");

    // Template.
    expect(line({ kind: "template", meta: { template_doc_id: "t1", template_title: "Weekly" } })).toBe(
      "Created from template Weekly",
    );
    expect(line({ kind: "template", meta: { template_hidden: true } })).toBe("Created from a template");
    expect(line({ kind: "template", meta: { template_doc_id: "t1", template_title: null } })).toBe(
      "Created from a template",
    );

    // Erase and gap.
    expect(line({ kind: "erase" })).toBe("Ann removed content from history");
    expect(line({ kind: "gap", author: null })).toBe("Changes saved without an author");

    // Baseline.
    const at = { started_at: "2026-11-02T08:40:00Z", ended_at: "2026-11-02T08:40:00Z", author: null };
    expect(line({ kind: "baseline", ...at, meta: { rollout: false } })).toBe("Earlier version · 2 Nov 2026, 09:40");
    expect(line({ kind: "baseline", ...at, meta: { rollout: true } })).toBe(
      "When complete history began · 2 Nov 2026, 09:40",
    );

    // Names: no "Former member" anywhere, the `former` field is ignored.
    expect(line({ author: null })).toBe("Unknown author · 09:12–09:40");
    expect(line({ author: { key: null, name: null, former: true } })).toBe("Unknown author · 09:12–09:40");
    expect(line({ author: { key: "u-ann", name: "Ann", former: true } })).toBe("Ann · 09:12–09:40");
    expect(line({ kind: "restore", author: null, meta: {} })).toBe("Restored by Unknown author");
    expect(Object.values(copy).join(" ")).not.toContain("Former member");
  });

  it("prefixes a span that starts on another day", () => {
    expect(formatSpan("2026-11-02T22:50:00Z", "2026-11-02T23:10:00Z", NOW, TZ)).toBe("Mon 2 Nov 23:50–00:10");
    expect(formatSpan("2026-11-07T22:50:00Z", "2026-11-07T23:10:00Z", NOW, TZ)).toBe("Yesterday 23:50–00:10");
    expect(formatSpan("2026-11-08T08:12:00Z", "2026-11-08T08:40:00Z", NOW, TZ)).toBe("09:12–09:40");
  });
});

describe("entryNote", () => {
  it("names who accepted suggestions from the list's author objects", () => {
    const accepted = [{ key: "u-bea", name: "Bea", former: false }, { key: null, name: null, former: true }, null];
    expect(entryNote(entry({ meta: { accepted_by: accepted } }))).toBe("accepted by Bea, Unknown author, Unknown author");
    expect(entryNote(entry({ meta: { accepted_by: [{ name: 7 }] } }))).toBe("accepted by Unknown author");
    expect(entryNote(entry({ meta: {} }))).toBeNull();
    expect(entryNote(entry({ meta: { accepted_by: [] } }))).toBeNull();
  });

  it("a fallback restore entry carries a note", () => {
    expect(entryNote(entry({ kind: "restore", meta: { fallback: true } }))).toBe(copy.restoreFallbackNote);
    expect(entryNote(entry({ kind: "restore", meta: { fallback: null } }))).toBeNull();
    expect(entryNote(entry({ kind: "restore", meta: {} }))).toBeNull();
    // Only a restore's meta says how it was made.
    expect(entryNote(entry({ meta: { fallback: true } }))).toBeNull();
  });
});

describe("showsChanges", () => {
  it("shows changes for every kind but baseline and erase", () => {
    const kinds: Record<HistoryKind, boolean> = {
      session: true,
      restore: true,
      copy: true,
      import: true,
      template: true,
      gap: true,
      erase: false,
      baseline: false,
    };
    for (const [kind, shows] of Object.entries(kinds)) {
      expect(showsChanges(kind as HistoryKind), kind).toBe(shows);
    }
  });
});

describe("dedupeEntries", () => {
  it("de-duplicates entries across pages", () => {
    const first = page([entry({ id: 5, ended_at: "2026-11-08T09:00:00Z" }), entry({ id: 4 })]);
    // The open session 5 moved above the first page's cursor and shows up
    // again on the second page, with a later end.
    const second = page([entry({ id: 5, ended_at: "2026-11-08T09:30:00Z" }), entry({ id: 3 })]);
    const out = dedupeEntries([first, second]);
    expect(out.map((e) => e.id)).toEqual([5, 4, 3]);
    expect(out[0]?.ended_at).toBe("2026-11-08T09:00:00Z");
  });
});
