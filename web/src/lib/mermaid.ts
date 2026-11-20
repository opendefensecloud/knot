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

/** Appended after the diagram's own source, so it wins over an init
 *  directive or frontmatter config there that turns HTML labels back on:
 *  Mermaid lets directives beat frontmatter and the last directive win
 *  (preprocessDiagram / detectInit, mermaid 11). Not covered: a directive
 *  with invalid JSON (Mermaid then drops all directives) and the legacy
 *  `config` key (it replaces `flowchart` as a whole). */
const SVG_LABELS = '\n%%{init: {"htmlLabels": false, "flowchart": {"htmlLabels": false}}}%%';

/** Render a diagram for the editor preview or the export, always with SVG
 *  text labels (see the initialize call above). */
export async function renderMermaidSvg(id: string, source: string): Promise<string> {
  const m = await getMermaid();
  return (await m.render(id, source + SVG_LABELS)).svg;
}
