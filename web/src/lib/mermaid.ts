export type MermaidApi = {
  initialize: (opts: Record<string, unknown>) => void;
  render: (id: string, src: string) => Promise<{ svg: string }>;
};

let cached: MermaidApi | null = null;
export async function getMermaid(): Promise<MermaidApi> {
  if (cached) return cached;
  const mod = await import("mermaid");
  const m = (mod.default ?? mod) as MermaidApi;
  m.initialize({
    startOnLoad: false,
    securityLevel: "strict",
    theme: "default",
    fontFamily: "'Inter Variable', Inter, system-ui, sans-serif",
    // Draw labels as SVG text. HTML labels live in <foreignObject>, which
    // sanitizeSvg strips (it can smuggle HTML), so the editor preview would
    // show empty boxes; Word and the export canvas can't draw them either.
    // Swimlane clusters and the triangle shape still read
    // flowchart.htmlLabels directly, so set both.
    htmlLabels: false,
    flowchart: { htmlLabels: false },
  });
  cached = m;
  return m;
}
