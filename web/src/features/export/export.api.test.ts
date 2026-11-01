import { afterEach, describe, expect, it, vi } from "vitest";

import { exportApi } from "./export.api";

afterEach(() => {
  vi.restoreAllMocks();
  document.cookie = "csrf=; expires=Thu, 01 Jan 1970 00:00:00 GMT";
});

describe("exportApi.file", () => {
  it("posts multipart with CSRF and returns blob + fallbacks", async () => {
    document.cookie = "csrf=tok123";
    const spy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("DOCX", {
        status: 200,
        headers: { "X-Knot-Export-Fallbacks": "diagrams=2, images=1, unknown=0" },
      }),
    );
    const r = await exportApi.file("d 1", "docx", new Map([["m1", new Blob(["png"])]]));
    expect(spy).toHaveBeenCalledTimes(1);
    const [url, init] = spy.mock.calls[0]!;
    expect(url).toBe("/api/docs/d%201/export/docx");
    expect(init?.method).toBe("POST");
    expect(init?.credentials).toBe("include");
    expect((init?.headers as Record<string, string>)["X-CSRF-Token"]).toBe("tok123");
    const fd = init?.body as FormData;
    expect((fd.get("m1") as File).name).toBe("m1.png");
    if (!("ok" in r)) throw new Error("expected ok");
    expect(r.ok.fallbacks).toEqual({ diagrams: 2, images: 1 });
    expect(await r.ok.blob.text()).toBe("DOCX");
  });

  it("maps the error envelope", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ error: { code: "export.busy", message: "Busy" } }), {
        status: 503,
      }),
    );
    const r = await exportApi.file("d", "odt", new Map());
    expect(r).toMatchObject({ error: { code: "export.busy", message: "Busy", status: 503 } });
  });

  it("maps network errors", async () => {
    vi.spyOn(globalThis, "fetch").mockRejectedValue(new TypeError("down"));
    const r = await exportApi.file("d", "odt", new Map());
    expect(r).toMatchObject({ error: { code: "network", status: 0 } });
  });
});

describe("exportApi.diagrams", () => {
  it("parses the list", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(
        JSON.stringify({
          limits: { max_body_bytes: 1, max_part_bytes: 2, max_parts: 3, scale: 2 },
          diagrams: [{ kind: "mermaid", key: "m1", source: "graph TD" }],
        }),
        { status: 200, headers: { "Content-Type": "application/json" } },
      ),
    );
    const r = await exportApi.diagrams("d");
    if (!("ok" in r)) throw new Error("expected ok");
    expect(r.ok.diagrams).toHaveLength(1);
  });
});
