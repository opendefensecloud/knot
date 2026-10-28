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

/** The server's DocResponse: a Doc without the archive fields. */
const COPY = {
  id: "d2",
  workspace_id: "w1",
  parent_id: null,
  title: "Plan (copy)",
  sort_key: "n",
  icon: null,
  created_by: "u-alice",
  archived: false,
  is_template: false,
};

describe("docsApi.duplicate", () => {
  it("POSTs include_subpages to the doc's duplicate sub-resource and returns the parsed copy", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse(COPY, 201));

    const r = await docsApi.duplicate("d/1", { include_subpages: true });

    expect(spy.mock.calls[0]?.[0]).toBe("/api/docs/d%2F1/duplicate");
    expect(spy.mock.calls[0]?.[1]?.method).toBe("POST");
    expect(JSON.parse(spy.mock.calls[0]?.[1]?.body as string)).toEqual({ include_subpages: true });
    if (!("ok" in r)) throw new Error("expected ok");
    expect(r.ok).toEqual({ ...COPY, archived_at: null, archive_root_id: null });
  });

  it("passes the error envelope through on 413", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      jsonResponse(
        { error: { code: "doc.duplicate_too_large", message: "over 200 pages or 200 MiB of files", details: {} } },
        413,
      ),
    );

    const r = await docsApi.duplicate("d1", { include_subpages: false });

    if (!("error" in r)) throw new Error("expected error");
    expect(r.error.status).toBe(413);
    expect(r.error.code).toBe("doc.duplicate_too_large");
  });

  it("rejects a body that does not match the contract", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(jsonResponse({ ...COPY, id: 7 }, 201));

    await expect(docsApi.duplicate("d1", { include_subpages: false })).rejects.toThrow();
  });
});
