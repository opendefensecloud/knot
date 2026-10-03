import { afterEach, describe, expect, it, vi } from "vitest";

import { docsApi } from "./docs.api";

afterEach(() => vi.restoreAllMocks());

function jsonResponse(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const BODY = {
  created_by: { id: "u-alice", display_name: "Alice" },
  created_at: "2026-03-03T10:00:00Z",
  contributors_since: "2026-10-03T12:00:00Z",
  contributors: [
    {
      user_id: "u-bob",
      display_name: "Bob",
      first_edited_at: "2026-10-03T12:05:00Z",
      last_edited_at: "2026-10-03T13:00:00Z",
    },
  ],
};

describe("docsApi.contributors", () => {
  it("GETs the doc's contributors sub-resource and returns the parsed body", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse(BODY));

    const r = await docsApi.contributors("d/1");

    expect(spy.mock.calls[0]?.[0]).toBe("/api/docs/d%2F1/contributors");
    expect(spy.mock.calls[0]?.[1]?.method).toBe("GET");
    if (!("ok" in r)) throw new Error("expected ok");
    expect(r.ok).toEqual(BODY);
  });

  it("passes the error envelope through on 403", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse({ error: { code: "auth.forbidden", message: "no", details: {} } }, 403),
    );

    const r = await docsApi.contributors("d1");

    if (!("error" in r)) throw new Error("expected error");
    expect(r.error.status).toBe(403);
  });

  it("rejects a body that does not match the contract", async () => {
    // Same stance as docsApi.get: a malformed body is a bug, not data to render.
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse({ ...BODY, contributors: [{ user_id: "u-bob" }] }),
    );

    await expect(docsApi.contributors("d1")).rejects.toThrow();
  });
});
