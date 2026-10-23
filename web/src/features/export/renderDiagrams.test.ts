import { afterEach, describe, expect, it, vi } from "vitest";

import { renderDiagrams, type DiagramLimits, type DiagramRef, type RenderDeps } from "./renderDiagrams";

const limits: DiagramLimits = { max_body_bytes: 10_485_760, max_part_bytes: 2_097_152, max_parts: 100, scale: 2 };
const png = (n: number) => new Blob([new Uint8Array(n)], { type: "image/png" });
const mer = (key: string): DiagramRef => ({ key, kind: "mermaid", source: `graph TD; ${key}` });
const board = (key: string): DiagramRef => ({ key, kind: "board", board_id: `b-${key}`, label: key });

function deps(over: Partial<RenderDeps> = {}): RenderDeps {
  return {
    renderMermaid: (s) => Promise.resolve(`<svg>${s}</svg>`),
    fetchBoardSvg: (id) => Promise.resolve(`<svg>${id}</svg>`),
    rasterize: () => Promise.resolve(png(10)),
    ...over,
  };
}

afterEach(() => {
  vi.useRealTimers();
});

describe("renderDiagrams", () => {
  it("renders mermaid and boards into keyed PNG blobs", async () => {
    const out = await renderDiagrams([mer("m1"), board("b1")], limits, deps());
    expect([...out.keys()]).toEqual(["m1", "b1"]);
    expect(out.get("m1")?.type).toBe("image/png");
  });

  it("skips a diagram whose render throws or times out", async () => {
    vi.useFakeTimers();
    const d = deps({
      renderMermaid: (s) => {
        if (s.includes("bad")) return Promise.reject(new Error("boom"));
        if (s.includes("hang")) return new Promise<string>(() => {});
        return Promise.resolve("<svg/>");
      },
    });
    const p = renderDiagrams([mer("bad"), mer("hang"), mer("ok")], limits, d);
    await vi.advanceTimersByTimeAsync(10_001);
    const out = await p;
    expect([...out.keys()]).toEqual(["ok"]);
  });

  it("treats a missing board preview as no picture", async () => {
    const out = await renderDiagrams([board("b1"), mer("m1")], limits, deps({ fetchBoardSvg: () => Promise.resolve(null) }));
    expect([...out.keys()]).toEqual(["m1"]);
  });

  it("re-rasterizes once at half size when a part is too big, then drops it", async () => {
    const rasterize = vi.fn((_svg: string, _scale: number, _max: number) => Promise.resolve(png(100)));
    const out = await renderDiagrams([mer("m1")], { ...limits, max_part_bytes: 50 }, deps({ rasterize }));
    expect(out.size).toBe(0);
    expect(rasterize.mock.calls.map((c) => c[2])).toEqual([2000, 1000]);
  });

  it("keeps the half-size retry when it fits", async () => {
    const rasterize = (_s: string, _k: number, max: number) => Promise.resolve(png(max === 2000 ? 100 : 10));
    const out = await renderDiagrams([mer("m1")], { ...limits, max_part_bytes: 50 }, deps({ rasterize }));
    expect([...out.keys()]).toEqual(["m1"]);
  });

  it("stops adding parts at the count limit", async () => {
    const out = await renderDiagrams([mer("a"), mer("b"), mer("c")], { ...limits, max_parts: 2 }, deps());
    expect([...out.keys()]).toEqual(["a", "b"]);
  });

  it("stops adding parts at the body limit", async () => {
    const out = await renderDiagrams(
      [mer("a"), mer("b"), mer("c")],
      { ...limits, max_body_bytes: 64 * 1024 + 25 },
      deps({ rasterize: () => Promise.resolve(png(10)) }),
    );
    expect([...out.keys()]).toEqual(["a", "b"]);
  });

  it("never runs more than three renders at once", async () => {
    let active = 0;
    let peak = 0;
    const renderMermaid = async () => {
      active++;
      peak = Math.max(peak, active);
      await new Promise((r) => setTimeout(r, 5));
      active--;
      return "<svg/>";
    };
    const out = await renderDiagrams(
      Array.from({ length: 8 }, (_, i) => mer(`m${i}`)),
      limits,
      deps({ renderMermaid }),
    );
    expect(out.size).toBe(8);
    expect(peak).toBe(3);
  });
});
