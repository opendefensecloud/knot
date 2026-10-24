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
  });
  cached = m;
  return m;
}
