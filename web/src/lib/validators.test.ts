import { describe, expect, it } from "vitest";

import {
  AuditAuthor,
  Doc,
  DocAccessInfo,
  DocWithRole,
  HistoryAuthor,
  HistoryEntry,
  HistoryPage,
  HistoryView,
  Member,
  parse,
} from "./validators";

/** A sidebar row as servers before restricted documents sent it. */
const LEGACY_DOC = {
  id: "d1",
  workspace_id: "w1",
  parent_id: null,
  title: "Plan",
  sort_key: "n",
  icon: null,
  created_by: "u1",
  archived: false,
};

describe("Doc", () => {
  it("parses a row without the restriction fields, with defaults", () => {
    const doc = parse(Doc, LEGACY_DOC);

    expect(doc.restricted).toBe(false);
    expect(doc.within_restricted).toBe(false);
    expect(doc.parent_hidden).toBe(false);
    expect(doc.effective_role).toBeNull();
  });

  it("keeps the restriction fields the server sends", () => {
    const doc = parse(Doc, {
      ...LEGACY_DOC,
      restricted: true,
      within_restricted: true,
      parent_hidden: true,
      effective_role: "editor",
    });

    expect(doc).toMatchObject({
      restricted: true,
      within_restricted: true,
      parent_hidden: true,
      effective_role: "editor",
    });
  });
});

describe("Member", () => {
  const LEGACY_MEMBER = { user_id: "u1", email: "a@example.org", display_name: "A", role: "editor" };

  // Catches the kind being made required: older servers send none.
  it("treats a member without a kind as human", () => {
    expect(parse(Member, LEGACY_MEMBER).kind).toBe("human");
  });

  it("keeps a bot's kind", () => {
    expect(parse(Member, { ...LEGACY_MEMBER, kind: "bot" }).kind).toBe("bot");
  });

  // Catches v.fallback coming back: it turned an unknown kind into human, and
  // the members page then offered Owner on that row.
  it("rejects a kind it doesn't know instead of calling it human", () => {
    expect(() => parse(Member, { ...LEGACY_MEMBER, kind: "robot" })).toThrow('received "robot"');
  });
});

describe("DocWithRole", () => {
  it("defaults the restriction fields", () => {
    const doc = parse(DocWithRole, { ...LEGACY_DOC, effective_role: "owner" });

    expect(doc.restricted).toBe(false);
    expect(doc.within_restricted).toBe(false);
    expect(doc.restriction_root_id).toBeNull();
  });
});

describe("DocAccessInfo", () => {
  it("parses the /access answer, hidden sources included", () => {
    const body = {
      restricted: false,
      restriction_root_id: null,
      restriction_root_title: null,
      people: [
        { principal: "user:u1", role: "owner", inherit: true, source_doc_id: null, source_title: null },
        { principal: "user:u2", role: "viewer", inherit: false, source_doc_id: "d1", source_title: "Plan" },
      ],
    };

    expect(parse(DocAccessInfo, body)).toEqual(body);
  });
});

describe("AuditAuthor", () => {
  it("reads a person without the bot mark as no bot, and keeps a bot's", () => {
    expect(parse(AuditAuthor, { key: "u1", name: "Ada", former: false })).toEqual({
      key: "u1",
      name: "Ada",
      former: false,
      bot: false,
    });
    expect(parse(AuditAuthor, { key: "u2", name: "CI bot", former: false, bot: true }).bot).toBe(true);
    expect(parse(AuditAuthor, { key: "u3", name: "Odd", former: false, bot: "yes" }).bot).toBe(false);
  });
});

/** One page of GET /api/docs/{id}/history/entries, as M1.3's list sends it. */
const HISTORY_PAGE = {
  history_since: "2026-11-02T08:40:00Z",
  history_complete: false,
  catching_up: false,
  unavailable: false,
  entries: [
    {
      id: 7,
      kind: "session",
      author: { key: "u-ann", name: "Ann", former: false },
      started_at: "2026-11-08T08:12:00Z",
      ended_at: "2026-11-08T08:40:00Z",
      open: false,
      words_added: 120,
      words_removed: 15,
      meta: { accepted_by: [{ key: "u-bea", name: "Bea", former: false }, null] },
    },
    {
      id: 6,
      kind: "gap",
      author: null,
      started_at: "2026-11-08T08:00:00Z",
      ended_at: "2026-11-08T08:00:00Z",
      open: false,
      words_added: null,
      words_removed: null,
      meta: {},
    },
  ],
  next_cursor: "TC4zLjQ",
};

describe("HistoryPage", () => {
  it("parses a page of the entries list", () => {
    const parsed = parse(HistoryPage, HISTORY_PAGE);

    expect(parsed.next_cursor).toBe("TC4zLjQ");
    expect(parsed.entries).toHaveLength(2);
    expect(parsed.entries[0]?.author).toEqual({ key: "u-ann", name: "Ann" });
    expect(parsed.entries[0]?.meta).toEqual(HISTORY_PAGE.entries[0]?.meta);
    expect(parsed.entries[1]?.author).toBeNull();
  });

  it("rejects an entry of a kind it doesn't know", () => {
    const bad = { ...HISTORY_PAGE.entries[1], kind: "snapshot" };

    expect(() => parse(HistoryEntry, bad)).toThrow('received "snapshot"');
    expect(() => parse(HistoryPage, { ...HISTORY_PAGE, entries: [bad] })).toThrow();
  });
});

describe("HistoryAuthor", () => {
  // M4 is dropped: the API still sends `former`, and the client ignores it.
  it("accepts the former field or its absence and drops it", () => {
    for (const author of [
      { key: "u1", name: "Ada", former: false },
      { key: null, name: null, former: true },
      { key: "u1", name: "Ada" },
    ]) {
      const parsed = parse(HistoryAuthor, author);

      expect(parsed).toEqual({ key: author.key, name: author.name });
      expect(parsed).not.toHaveProperty("former");
    }
  });
});

describe("HistoryView", () => {
  it("parses a view and keeps the doc as sent", () => {
    const body = {
      entry_id: 7,
      kind: "session",
      changes: true,
      can_restore: true,
      can_erase: false,
      doc: {
        type: "doc",
        content: [{ type: "paragraph", content: [{ type: "text", text: "hi", marks: [{ type: "historyInsert" }] }] }],
      },
    };

    expect(parse(HistoryView, body)).toEqual(body);
    expect(parse(HistoryView, { ...body, doc: { type: "doc" } }).doc).toEqual({ type: "doc" });
  });

  it("rejects a wrong kind or a doc that is not a doc", () => {
    const body = { entry_id: 7, kind: "session", changes: false, can_restore: true, can_erase: false, doc: { type: "doc" } };

    expect(() => parse(HistoryView, { ...body, kind: "snapshot" })).toThrow();
    expect(() => parse(HistoryView, { ...body, doc: { type: "paragraph" } })).toThrow();
  });
});
