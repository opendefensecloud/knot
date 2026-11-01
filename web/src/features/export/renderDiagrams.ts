import { boardsApi } from "../../lib/boards.api";
import { getMermaid } from "../../lib/mermaid";

export type DiagramLimits = {
  max_body_bytes: number;
  max_part_bytes: number;
  max_parts: number;
  scale: number;
};

export type DiagramRef =
  | { key: string; kind: "mermaid"; source: string }
  | { key: string; kind: "board"; board_id: string; label: string };

/** An SVG document with an explicit size in CSS px. */
export type SizedSvg = { markup: string; width: number; height: number };

export type RenderDeps = {
  /** Returns SVG markup. */
  renderMermaid: (source: string) => Promise<string>;
  /** Returns SVG markup, or null on any non-200. */
  fetchBoardSvg: (boardId: string) => Promise<string | null>;
  /** Returns an image/png blob of `svg` at `scale` times its size, at most `maxSide` px on the long side. */
  rasterize: (svg: SizedSvg, scale: number, maxSide: number) => Promise<Blob>;
};

const MAX_SIDE = 2000;
const CONCURRENCY = 3;
const TIMEOUT_MS = 10_000;
const BODY_HEADROOM = 64 * 1024;
const SVG_NS = "http://www.w3.org/2000/svg";

let renderSeq = 0;

/** An absolute length in CSS px ("120", "120.5px"); null for a relative or other one ("100%", "auto", "3cm"). */
function pxLength(value: string | null): number | null {
  const m = value === null ? null : /^\s*(\d*\.?\d+)\s*(?:px)?\s*$/i.exec(value);
  const n = m ? Number(m[1]) : NaN;
  return Number.isFinite(n) && n > 0 ? n : null;
}

/** The width and height of a `viewBox`, when it has a positive size. */
function viewBoxSize(value: string | null): { width: number; height: number } | null {
  const parts = (value ?? "").trim().split(/[\s,]+/).map(Number);
  const [, , width = NaN, height = NaN] = parts;
  return parts.length === 4 && width > 0 && height > 0 && Number.isFinite(width + height) ? { width, height } : null;
}

/**
 * `svg` with explicit px `width` and `height` on its root, and those sizes.
 *
 * An `<img>` gives an SVG its natural size only from absolute ones. Mermaid's
 * default output (`width="100%"`, a `max-width` style, a viewBox and no
 * height) has none: Chromium then draws it into a 300×150 box and Firefox and
 * Safari report 0×0, so the diagram comes out unreadably small. Absolute
 * width and height win; a missing or relative side comes from the viewBox.
 * The `max-width` goes, since it would cap the root against the image's box.
 * Null when there is no size to find, or the markup isn't an SVG document.
 */
export function sizeSvg(svg: string): SizedSvg | null {
  const doc = new DOMParser().parseFromString(svg, "image/svg+xml");
  const root = doc.documentElement;
  if (root.namespaceURI !== SVG_NS || root.localName !== "svg") return null;
  if (doc.getElementsByTagName("parsererror").length > 0) return null;

  let width = pxLength(root.getAttribute("width"));
  let height = pxLength(root.getAttribute("height"));
  if (width === null || height === null) {
    const box = viewBoxSize(root.getAttribute("viewBox"));
    if (!box) return null;
    if (width !== null) height = (width * box.height) / box.width;
    else if (height !== null) width = (height * box.width) / box.height;
    else ({ width, height } = box);
  }
  root.setAttribute("width", String(width));
  root.setAttribute("height", String(height));
  const style = root.getAttribute("style");
  if (style !== null) {
    const kept = style
      .split(";")
      .map((d) => d.trim())
      .filter((d) => d && !/^max-width\s*:/i.test(d))
      .join("; ");
    if (kept) root.setAttribute("style", kept);
    else root.removeAttribute("style");
  }
  return { markup: new XMLSerializer().serializeToString(doc), width, height };
}

function rasterizeToPng(svg: SizedSvg, scale: number, maxSide: number): Promise<Blob> {
  const url = URL.createObjectURL(new Blob([svg.markup], { type: "image/svg+xml;charset=utf-8" }));
  return new Promise<HTMLImageElement>((resolve, reject) => {
    const img = document.createElement("img");
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error("svg image load failed"));
    img.src = url;
  })
    .then(
      (img) =>
        new Promise<Blob>((resolve, reject) => {
          // The size sizeSvg found, never the image's natural size (see there).
          const k = Math.min(scale, maxSide / Math.max(svg.width, svg.height));
          const canvas = document.createElement("canvas");
          canvas.width = Math.max(1, Math.round(svg.width * k));
          canvas.height = Math.max(1, Math.round(svg.height * k));
          const ctx = canvas.getContext("2d");
          if (!ctx) {
            reject(new Error("no 2d context"));
            return;
          }
          ctx.fillStyle = "#ffffff";
          ctx.fillRect(0, 0, canvas.width, canvas.height);
          ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
          canvas.toBlob((b) => {
            if (b) resolve(b);
            else reject(new Error("toBlob returned null"));
          }, "image/png");
        }),
    )
    .finally(() => URL.revokeObjectURL(url));
}

export const defaultRenderDeps: RenderDeps = {
  async renderMermaid(source) {
    const m = await getMermaid();
    const id = `knot-export-mermaid-${++renderSeq}`;
    return (await m.render(id, source + '\n%%{init: {"htmlLabels": false}}%%')).svg;
  },
  async fetchBoardSvg(boardId) {
    const res = await boardsApi.getSvg(boardId);
    return "ok" in res ? res.ok : null;
  },
  rasterize: rasterizeToPng,
};

async function renderOne(d: DiagramRef, limits: DiagramLimits, deps: RenderDeps): Promise<Blob | null> {
  const markup = d.kind === "mermaid" ? await deps.renderMermaid(d.source) : await deps.fetchBoardSvg(d.board_id);
  // Without a size there is nothing legible to draw: let it fall back.
  const svg = markup === null ? null : sizeSvg(markup);
  if (svg === null) return null;
  let png = await deps.rasterize(svg, limits.scale, MAX_SIDE);
  if (png.size > limits.max_part_bytes) {
    png = await deps.rasterize(svg, limits.scale, MAX_SIDE / 2);
    if (png.size > limits.max_part_bytes) return null;
  }
  return png;
}

async function renderGuarded(d: DiagramRef, limits: DiagramLimits, deps: RenderDeps): Promise<Blob | null> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<null>((resolve) => {
    timer = setTimeout(() => resolve(null), TIMEOUT_MS);
  });
  try {
    return await Promise.race([renderOne(d, limits, deps), timeout]);
  } catch {
    return null;
  } finally {
    clearTimeout(timer);
  }
}

export async function renderDiagrams(
  diagrams: DiagramRef[],
  limits: DiagramLimits,
  deps: RenderDeps = defaultRenderDeps,
): Promise<Map<string, Blob>> {
  const results: (Blob | null)[] = new Array<Blob | null>(diagrams.length).fill(null);
  let next = 0;
  const worker = async (): Promise<void> => {
    while (next < diagrams.length) {
      const i = next++;
      const d = diagrams[i];
      if (d) results[i] = await renderGuarded(d, limits, deps);
    }
  };
  await Promise.all(Array.from({ length: Math.min(CONCURRENCY, diagrams.length) }, worker));

  const out = new Map<string, Blob>();
  const budget = limits.max_body_bytes - BODY_HEADROOM;
  let total = 0;
  for (let i = 0; i < diagrams.length; i++) {
    const blob = results[i];
    const d = diagrams[i];
    if (!blob || !d) continue;
    if (out.size >= limits.max_parts || total + blob.size > budget) break;
    out.set(d.key, blob);
    total += blob.size;
  }
  return out;
}
