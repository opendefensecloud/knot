import { afterEach, describe, expect, it, vi } from "vitest";

import {
  renderDiagrams,
  sizeSvg,
  type DiagramLimits,
  type DiagramRef,
  type RenderDeps,
  type SizedSvg,
} from "./renderDiagrams";

const limits: DiagramLimits = { max_body_bytes: 10_485_760, max_part_bytes: 2_097_152, max_parts: 100, scale: 2 };
const png = (n: number) => new Blob([new Uint8Array(n)], { type: "image/png" });
const mer = (key: string): DiagramRef => ({ key, kind: "mermaid", source: `graph TD; ${key}` });
const board = (key: string): DiagramRef => ({ key, kind: "board", board_id: `b-${key}`, label: key });
const SVG_NS = "http://www.w3.org/2000/svg";
/** An SVG shaped like Mermaid's default output: relative width, a max-width, a viewBox, no height. */
const mermaidSvg = (w: number, h: number, body = "") =>
  `<svg xmlns="${SVG_NS}" width="100%" style="max-width: ${w}px; background-color: white;" viewBox="0 0 ${w} ${h}">${body}</svg>`;

function deps(over: Partial<RenderDeps> = {}): RenderDeps {
  return {
    renderMermaid: (s) => Promise.resolve(mermaidSvg(100, 50, `<text>${s}</text>`)),
    fetchBoardSvg: (id) => Promise.resolve(`<svg xmlns="${SVG_NS}" width="80" height="40"><text>${id}</text></svg>`),
    rasterize: () => Promise.resolve(png(10)),
    ...over,
  };
}

function root(sized: SizedSvg | null): Element {
  if (!sized) throw new Error("not sized");
  return new DOMParser().parseFromString(sized.markup, "image/svg+xml").documentElement;
}

afterEach(() => {
  vi.useRealTimers();
});

describe("sizeSvg", () => {
  it("sizes Mermaid's width=100% output from its viewBox and drops the max-width", () => {
    const sized = sizeSvg(mermaidSvg(1492, 82, "<g><text>A</text></g>"));
    expect(sized).toMatchObject({ width: 1492, height: 82 });
    const svg = root(sized);
    expect(svg.getAttribute("width")).toBe("1492");
    expect(svg.getAttribute("height")).toBe("82");
    expect(svg.getAttribute("viewBox")).toBe("0 0 1492 82");
    expect(svg.getAttribute("style")).toBe("background-color: white");
    expect(svg.querySelector("text")?.textContent).toBe("A");
  });

  it("keeps a tall diagram's height", () => {
    const sized = sizeSvg(
      `<svg xmlns="${SVG_NS}" width="100%" style="max-width:468px" viewBox="-50, -10, 468, 1321"></svg>`,
    );
    expect(sized).toMatchObject({ width: 468, height: 1321 });
    expect(root(sized).hasAttribute("style")).toBe(false);
  });

  it("keeps absolute width and height, as a board preview has", () => {
    expect(sizeSvg(`<svg xmlns="${SVG_NS}" width="400" height="300px" viewBox="0 0 800 600"/>`)).toMatchObject({
      width: 400,
      height: 300,
    });
  });

  it("takes the missing side from the viewBox's aspect ratio", () => {
    expect(sizeSvg(`<svg xmlns="${SVG_NS}" width="200" viewBox="0 0 100 50"/>`)).toMatchObject({
      width: 200,
      height: 100,
    });
    expect(sizeSvg(`<svg xmlns="${SVG_NS}" height="10" width="auto" viewBox="0 0 100 50"/>`)).toMatchObject({
      width: 20,
      height: 10,
    });
  });

  it("gives up on an SVG without a usable size, or that isn't one", () => {
    expect(sizeSvg(`<svg xmlns="${SVG_NS}" width="100%" style="max-width: 300px"/>`)).toBeNull();
    expect(sizeSvg(`<svg xmlns="${SVG_NS}" width="100%" viewBox="0 0 0 50"/>`)).toBeNull();
    expect(sizeSvg(`<svg width="10" height="10"/>`)).toBeNull();
    expect(sizeSvg(`<svg xmlns="${SVG_NS}" width="10" height="10"><g></svg>`)).toBeNull();
    expect(sizeSvg("not svg")).toBeNull();
  });
});

describe("renderDiagrams", () => {
  it("rasterizes each diagram at the size its SVG declares", async () => {
    const rasterize = vi.fn((_svg: SizedSvg, _scale: number, _max: number) => Promise.resolve(png(10)));
    const d = deps({ renderMermaid: () => Promise.resolve(mermaidSvg(1492, 82)), rasterize });
    await renderDiagrams([mer("m1"), board("b1")], limits, d);
    expect(rasterize.mock.calls.map(([svg, scale]) => [svg.width, svg.height, scale])).toEqual([
      [1492, 82, 2],
      [80, 40, 2],
    ]);
    expect(root(rasterize.mock.calls[0]![0]).getAttribute("width")).toBe("1492");
  });

  it("treats an SVG without a size as no picture", async () => {
    const rasterize = vi.fn(() => Promise.resolve(png(10)));
    const d = deps({ renderMermaid: () => Promise.resolve(`<svg xmlns="${SVG_NS}" width="100%"/>`), rasterize });
    const out = await renderDiagrams([mer("m1"), board("b1")], limits, d);
    expect([...out.keys()]).toEqual(["b1"]);
    expect(rasterize).toHaveBeenCalledTimes(1);
  });

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
        return Promise.resolve(mermaidSvg(10, 10));
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
    const rasterize = vi.fn((_svg: SizedSvg, _scale: number, _max: number) => Promise.resolve(png(100)));
    const out = await renderDiagrams([mer("m1")], { ...limits, max_part_bytes: 50 }, deps({ rasterize }));
    expect(out.size).toBe(0);
    expect(rasterize.mock.calls.map((c) => c[2])).toEqual([2000, 1000]);
  });

  it("keeps the half-size retry when it fits", async () => {
    const rasterize = (_s: SizedSvg, _k: number, max: number) => Promise.resolve(png(max === 2000 ? 100 : 10));
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
      return mermaidSvg(10, 10);
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
