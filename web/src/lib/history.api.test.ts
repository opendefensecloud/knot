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
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(null, { status: 204 }));

    expect(await historyApi.restoreEntry("d1", 7, "2026-11-08T08:40:00.123+00:00")).toEqual({ ok: undefined });

    const [url, init] = spy.mock.calls[0] ?? [];
    expect(url).toBe("/api/docs/d1/history/entries/7/restore?ended_at=2026-11-08T08%3A40%3A00.123%2B00%3A00");
    expect(init?.method).toBe("POST");
    expect((init?.headers as Record<string, string>)["X-CSRF-Token"]).toBe("test-token");
  });

  it("returns a refused restore's error", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      json({ error: { code: "history.moved", message: "moved", details: {} } }, 409),
    );

    expect(await historyApi.restoreEntry("d1", 7, "2026-11-08T08:40:00Z")).toEqual({
      error: { code: "history.moved", message: "moved", details: {}, status: 409 },
    });
  });
});
