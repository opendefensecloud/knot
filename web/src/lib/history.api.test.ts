import { beforeEach, describe, expect, it, vi } from "vitest";

import { historyApi } from "./history.api";

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

const PAGE = {
  history_since: "2026-11-02T08:40:00Z",
  history_complete: true,
  catching_up: false,
  unavailable: false,
  entries: [],
  next_cursor: null,
};

/** A doc as the server answers a copy (`doc_answer`), shaped as
 *  `docs.api.test.ts`'s duplicate fixture. */
const DOC = {
  id: "n1",
  workspace_id: "w1",
  parent_id: null,
  title: "Plan (version of 2 Nov 2026, 09:40)",
  sort_key: "n",
  icon: null,
  created_by: "u-alice",
  archived: false,
  is_template: false,
};

const VIEW = { entry_id: 7, kind: "session", changes: true, can_restore: true, can_erase: false, doc: { type: "doc" } };

beforeEach(() => {
  vi.restoreAllMocks();
  document.cookie = "csrf=test-token; Path=/";
});

describe("historyApi", () => {
  it("asks for the first page without a cursor and later pages with it", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockImplementation(() => Promise.resolve(json(PAGE)));

    expect(await historyApi.entries("d 1", null)).toEqual({ ok: PAGE });
    await historyApi.entries("d 1", "TC4z+/=");

    expect(spy.mock.calls.map((c) => c[0])).toEqual([
      "/api/docs/d%201/history/entries",
      "/api/docs/d%201/history/entries?cursor=TC4z%2B%2F%3D",
    ]);
  });

  it("asks for a view with or without changes and passes the signal", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockImplementation(() => Promise.resolve(json(VIEW)));
    const controller = new AbortController();

    expect(await historyApi.view("d1", 7, true, controller.signal)).toEqual({ ok: VIEW });
    await historyApi.view("d1", 7, false);

    expect(spy.mock.calls[0]?.[0]).toBe("/api/docs/d1/history/entries/7/view?changes=1");
    expect(spy.mock.calls[0]?.[1]?.signal).toBe(controller.signal);
    expect(spy.mock.calls[1]?.[0]).toBe("/api/docs/d1/history/entries/7/view?changes=0");
  });

  it("rejects a view whose body is not a view", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(json({ ...VIEW, kind: "snapshot" }));

    await expect(historyApi.view("d1", 7, true)).rejects.toThrow();
  });

  it("restores an entry with the version's end and the CSRF token", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(json({ outcome: "restored" }));

    expect(await historyApi.restoreEntry("d1", 7, "2026-11-08T08:40:00.123+00:00")).toEqual({
      ok: { outcome: "restored" },
    });

    const [url, init] = spy.mock.calls[0] ?? [];
    expect(url).toBe("/api/docs/d1/history/entries/7/restore?ended_at=2026-11-08T08%3A40%3A00.123%2B00%3A00");
    expect(init?.method).toBe("POST");
    expect((init?.headers as Record<string, string>)["X-CSRF-Token"]).toBe("test-token");
  });

  it("rejects a restore answer with an unknown outcome", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(json({ outcome: "reverted" }));

    await expect(historyApi.restoreEntry("d1", 7, "2026-11-08T08:40:00Z")).rejects.toThrow();
  });

  it("returns a refused restore's error", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      json({ error: { code: "history.moved", message: "moved", details: {} } }, 409),
    );

    expect(await historyApi.restoreEntry("d1", 7, "2026-11-08T08:40:00Z")).toEqual({
      error: { code: "history.moved", message: "moved", details: {}, status: 409 },
    });
  });

  it("restores an entry as a new page with its label and end", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(json(DOC, 201));
    const body = { version_label: "2 Nov 2026, 09:40", ended_at: "2026-11-08T08:40:00Z" };

    const r = await historyApi.restoreAsNew("d/1", 7, body);

    expect(r).toEqual({
      ok: {
        ...DOC,
        archived_at: null,
        archive_root_id: null,
        restricted: false,
        within_restricted: false,
        parent_hidden: false,
        effective_role: null,
      },
    });
    const [url, init] = spy.mock.calls[0] ?? [];
    expect(url).toBe("/api/docs/d%2F1/history/entries/7/restore-as-new");
    expect(init?.method).toBe("POST");
    expect(JSON.parse(init?.body as string)).toEqual(body);
    const headers = init?.headers as Record<string, string>;
    expect(headers["X-CSRF-Token"]).toBe("test-token");
    expect(headers["Content-Type"]).toBe("application/json");
  });

  it("returns a refused restore-as-new's error and rejects a body that is not a doc", async () => {
    const body = { version_label: "2 Nov 2026, 09:40", ended_at: "2026-11-08T08:40:00Z" };
    vi.spyOn(globalThis, "fetch").mockResolvedValueOnce(
      json({ error: { code: "acl.editor_required", message: "editor required", details: {} } }, 403),
    );

    expect(await historyApi.restoreAsNew("d1", 7, body)).toEqual({
      error: { code: "acl.editor_required", message: "editor required", details: {}, status: 403 },
    });

    vi.spyOn(globalThis, "fetch").mockResolvedValueOnce(json({ id: 1 }, 201));
    await expect(historyApi.restoreAsNew("d1", 7, body)).rejects.toThrow();
  });
});
