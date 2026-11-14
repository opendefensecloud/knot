import { describe, it, expect, vi, beforeEach } from "vitest";
import { ApiRequestError, apiFetch, fetchBlob, readApiError } from "./api";

beforeEach(() => {
  vi.restoreAllMocks();
  document.cookie = "csrf=test-token; Path=/";
});

describe("apiFetch", () => {
  it("returns ok for 200 JSON", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ id: "x" }), {
        status: 200,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const r = await apiFetch<{ id: string }>("/api/foo");
    if ("ok" in r) expect(r.ok.id).toBe("x");
    else throw new Error("expected ok");
  });

  it("returns error envelope for 4xx", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ error: { code: "auth.csrf", message: "no", details: {} } }), {
        status: 403,
        headers: { "Content-Type": "application/json" },
      }),
    );
    const r = await apiFetch("/api/foo", { method: "POST", body: {} });
    if ("error" in r) {
      expect(r.error.code).toBe("auth.csrf");
      expect(r.error.status).toBe(403);
    } else throw new Error("expected error");
  });

  it("sends X-CSRF-Token on POST", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(null, { status: 200 }),
    );
    await apiFetch("/api/foo", { method: "POST", body: { a: 1 } });
    const init = spy.mock.calls[0]?.[1];
    const headers = init?.headers as Record<string, string> | undefined;
    expect(headers?.["X-CSRF-Token"]).toBe("test-token");
    expect(headers?.["Content-Type"]).toBe("application/json");
  });

  it("does NOT send X-CSRF-Token on GET", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } }),
    );
    await apiFetch("/api/foo");
    const init = spy.mock.calls[0]?.[1];
    const headers = init?.headers as Record<string, string> | undefined;
    expect(headers?.["X-CSRF-Token"]).toBeUndefined();
  });
});

describe("a 429 rate.limited answer", () => {
  function tooFast() {
    return new Response(
      JSON.stringify({ error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 7 } } }),
      { status: 429, headers: { "Content-Type": "application/json", "Retry-After": "7" } },
    );
  }

  it("keeps its code, status and retry_after_secs through apiFetch", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(tooFast());
    const r = await apiFetch("/api/docs", { method: "POST", body: {} });
    if (!("error" in r)) throw new Error("expected error");
    expect(r.error).toEqual({
      code: "rate.limited",
      message: "too many requests",
      details: { retry_after_secs: 7 },
      status: 429,
    });
  });

  it("is read from a raw response by readApiError", async () => {
    expect(await readApiError(tooFast())).toEqual({
      code: "rate.limited",
      message: "too many requests",
      details: { retry_after_secs: 7 },
      status: 429,
    });
  });
});

describe("readApiError", () => {
  it("falls back to http_error for a body that isn't the envelope", async () => {
    const error = await readApiError(new Response("<html>bad gateway</html>", { status: 502 }));
    expect(error).toEqual({ code: "http_error", message: "HTTP 502", details: {}, status: 502 });
  });
});

describe("fetchBlob", () => {
  it("returns the body as a blob, with the session cookie", async () => {
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("zip-bytes", { status: 200, headers: { "Content-Type": "application/zip" } }),
    );
    const r = await fetchBlob("/api/workspace/export");
    if (!("ok" in r)) throw new Error("expected ok");
    expect(await r.ok.text()).toBe("zip-bytes");
    expect(spy.mock.calls[0]?.[0]).toBe("/api/workspace/export");
    expect(spy.mock.calls[0]?.[1]).toMatchObject({ credentials: "include" });
  });

  it("returns the error envelope of a refused download", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(
        JSON.stringify({ error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 4 } } }),
        { status: 429, headers: { "Content-Type": "application/json" } },
      ),
    );
    const r = await fetchBlob("/api/workspace/export");
    if (!("error" in r)) throw new Error("expected error");
    expect(r.error.code).toBe("rate.limited");
    expect(r.error.details).toEqual({ retry_after_secs: 4 });
  });

  it("reports a failed request as a network error", async () => {
    vi.spyOn(globalThis, "fetch").mockRejectedValue(new TypeError("offline"));
    const r = await fetchBlob("/api/workspace/export");
    if (!("error" in r)) throw new Error("expected error");
    expect(r.error).toMatchObject({ code: "network", status: 0 });
  });
});

describe("ApiRequestError", () => {
  it("carries the error and names its code as the message", () => {
    const apiError = { code: "rate.limited", message: "too many requests", details: {}, status: 429 };
    const thrown = new ApiRequestError(apiError);
    expect(thrown).toBeInstanceOf(Error);
    expect(thrown.message).toBe("rate.limited");
    expect(thrown.apiError).toBe(apiError);
  });
});
