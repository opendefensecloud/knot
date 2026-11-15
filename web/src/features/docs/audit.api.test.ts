import { afterEach, describe, expect, it, vi } from "vitest";

import { auditApi } from "./audit.api";

afterEach(() => vi.restoreAllMocks());

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const FIRST_PAGE = {
  events: [
    {
      id: "5c1e9a07d3b4f268",
      at: "2026-11-13T09:12:44Z",
      action: "doc.export",
      category: "exports",
      actor_kind: "user",
      actor: { key: "u-ada", name: "Ada", former: false },
      details: { format: "docx", scope: "page", diagram_pictures: 2 },
    },
  ],
  next_cursor: "5c1e9a07d3b4f268",
  stats: {
    total: 1,
    by_action: [{ action: "doc.export", count: 1, last_at: "2026-11-13T09:12:44Z" }],
    exports: {
      total: 1,
      people: 1,
      last_30_days: 1,
      last: {
        at: "2026-11-13T09:12:44Z",
        actor_kind: "user",
        actor: { key: "u-ada", name: "Ada", former: false },
        format: "docx",
        scope: "page",
      },
      by_format: { docx: 1 },
      by_scope: { page: 1 },
      by_person: [
        { actor: { key: "u-ada", name: "Ada", former: false }, count: 1, last_at: "2026-11-13T09:12:44Z" },
      ],
      by_person_truncated: false,
    },
  },
};

describe("auditApi.list", () => {
  it("GETs the page's audit log with no query when nothing is set", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse(FIRST_PAGE));

    const result = await auditApi.list("d1");

    expect(fetchSpy.mock.calls[0]?.[0]).toBe("/api/docs/d1/audit");
    expect(fetchSpy.mock.calls[0]?.[1]?.method).toBe("GET");
    if (!("ok" in result)) throw new Error("expected ok");
    expect(result.ok).toMatchObject(FIRST_PAGE);
    // A person the server didn't mark is no bot.
    expect(result.ok.events[0]?.actor?.bot).toBe(false);
    expect(result.ok.stats?.exports.by_person[0]?.actor.bot).toBe(false);
  });

  it("keeps a bot's mark wherever a person is named", async () => {
    const ciBot = { key: "u-ci", name: "CI bot", former: false, bot: true };
    const exports = FIRST_PAGE.stats.exports;
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse({
        ...FIRST_PAGE,
        events: [{ ...FIRST_PAGE.events[0], actor: ciBot }],
        stats: {
          ...FIRST_PAGE.stats,
          exports: {
            ...exports,
            last: { ...exports.last, actor: ciBot },
            by_person: [{ ...exports.by_person[0], actor: ciBot }],
          },
        },
      }),
    );

    const result = await auditApi.list("d1");

    if (!("ok" in result)) throw new Error("expected ok");
    expect(result.ok.events[0]?.actor?.bot).toBe(true);
    expect(result.ok.stats?.exports.last?.actor?.bot).toBe(true);
    expect(result.ok.stats?.exports.by_person[0]?.actor.bot).toBe(true);
  });

  it("encodes the id and sends only the parameters that are set", async () => {
    const fetchSpy = vi
      .spyOn(globalThis, "fetch")
      .mockImplementation(() => Promise.resolve(jsonResponse({ events: [], next_cursor: null, stats: null })));

    await auditApi.list("d/1", { category: "exports", cursor: "e07b42c9a1d65f30" });
    await auditApi.list("d1", { limit: 20 });
    await auditApi.list("d1", { category: undefined, cursor: undefined });

    expect(fetchSpy.mock.calls[0]?.[0]).toBe("/api/docs/d%2F1/audit?category=exports&cursor=e07b42c9a1d65f30");
    expect(fetchSpy.mock.calls[1]?.[0]).toBe("/api/docs/d1/audit?limit=20");
    expect(fetchSpy.mock.calls[2]?.[0]).toBe("/api/docs/d1/audit");
  });

  it("passes the error envelope through", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse(
        { error: { code: "acl.editor_required", message: "editors only", details: {} } },
        403,
      ),
    );

    const result = await auditApi.list("d1");

    if (!("error" in result)) throw new Error("expected error");
    expect(result.error).toEqual({
      code: "acl.editor_required",
      message: "editors only",
      details: {},
      status: 403,
    });
  });

  it("keeps unknown categories and actor kinds and defaults missing details", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse({
        events: [
          { id: "0a4c8e2b6d1f3957", at: "2026-11-13T09:12:44Z", action: "doc.future", category: "other", actor_kind: "robot", actor: null },
        ],
        next_cursor: null,
        stats: null,
      }),
    );

    const result = await auditApi.list("d1", { cursor: "73d9b1e5c08a246f" });

    if (!("ok" in result)) throw new Error("expected ok");
    expect(result.ok.events[0]).toMatchObject({ category: "other", actor_kind: "robot", details: {} });
  });

  it("rejects a body that does not match the contract", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse({ events: [{ id: "x" }] }));

    await expect(auditApi.list("d1")).rejects.toThrow();
  });
});
