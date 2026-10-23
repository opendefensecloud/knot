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

export type RenderDeps = {
  /** Returns SVG markup. */
  renderMermaid: (source: string) => Promise<string>;
  /** Returns SVG markup, or null on any non-200. */
  fetchBoardSvg: (boardId: string) => Promise<string | null>;
  /** Returns an image/png blob. */
  rasterize: (svg: string, scale: number, maxSide: number) => Promise<Blob>;
};

const MAX_SIDE = 2000;
const CONCURRENCY = 3;
const TIMEOUT_MS = 10_000;
const BODY_HEADROOM = 64 * 1024;

let renderSeq = 0;

function rasterizeToPng(svg: string, scale: number, maxSide: number): Promise<Blob> {
  const url = URL.createObjectURL(new Blob([svg], { type: "image/svg+xml;charset=utf-8" }));
  return new Promise<HTMLImageElement>((resolve, reject) => {
    const img = document.createElement("img");
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error("svg image load failed"));
    img.src = url;
  })
    .then(
      (img) =>
        new Promise<Blob>((resolve, reject) => {
          const w = img.naturalWidth || 1;
          const h = img.naturalHeight || 1;
          const k = Math.min(scale, maxSide / Math.max(w, h));
          const canvas = document.createElement("canvas");
          canvas.width = Math.max(1, Math.round(w * k));
          canvas.height = Math.max(1, Math.round(h * k));
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
  const svg = d.kind === "mermaid" ? await deps.renderMermaid(d.source) : await deps.fetchBoardSvg(d.board_id);
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
