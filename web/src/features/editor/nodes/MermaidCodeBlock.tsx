import CodeBlock from "@tiptap/extension-code-block";
import {
  NodeViewContent,
  NodeViewWrapper,
  ReactNodeViewRenderer,
  type ReactNodeViewProps,
} from "@tiptap/react";
import { Eye, Pencil } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";

import { sanitizeSvg } from "../../../lib/sanitize";

/**
 * MermaidCodeBlock — replaces StarterKit's `codeBlock` with one that adds
 * a React NodeView. When the block's language is "mermaid" the NodeView
 * renders the SVG output (mermaid is dynamically imported). When the
 * language is anything else (including null) the NodeView falls through
 * to a plain <pre><code> with editable content.
 *
 * Storage-wise this is still a fenced code block, so markdown round-trip
 * is unchanged.
 */
/**
 * Note on the node name: the canonical schema (`crates/knot-markdown`) uses
 * snake_case "code_block". Tiptap's CodeBlock defaults to "codeBlock". We
 * override the name so the y-prosemirror sync writes `code_block` into the
 * Yjs XML element, which is what the markdown serializer expects.
 */
export const MermaidCodeBlock = CodeBlock.extend({
  name: "code_block",
  addNodeView() {
    return ReactNodeViewRenderer(MermaidNodeView);
  },
  addKeyboardShortcuts() {
    return {
      ...(this.parent?.() ?? {}),
      // Tab inside a code block inserts two spaces instead of moving focus.
      // Shift-Tab dedents (best-effort: removes up to two leading spaces).
      Tab: ({ editor }) => {
        if (!editor.isActive(this.name)) return false;
        editor.commands.insertContent("  ");
        return true;
      },
      "Shift-Tab": ({ editor }) => {
        if (!editor.isActive(this.name)) return false;
        const { from } = editor.state.selection;
        const $from = editor.state.doc.resolve(from);
        const lineStart = from - $from.parentOffset;
        const textBefore = editor.state.doc.textBetween(lineStart, from);
        const lineStartPos = from - textBefore.length;
        const lineText = editor.state.doc.textBetween(lineStartPos, lineStartPos + 2);
        if (lineText === "  ") {
          editor.commands.deleteRange({ from: lineStartPos, to: lineStartPos + 2 });
          return true;
        }
        if (lineText.startsWith(" ")) {
          editor.commands.deleteRange({ from: lineStartPos, to: lineStartPos + 1 });
          return true;
        }
        return true;
      },
    };
  },
});

type MermaidApi = {
  initialize: (opts: Record<string, unknown>) => void;
  render: (id: string, src: string) => Promise<{ svg: string }>;
};

let cached: MermaidApi | null = null;
async function getMermaid(): Promise<MermaidApi> {
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

function MermaidNodeView(props: ReactNodeViewProps) {
  const { node } = props;
  const lang = (node.attrs.language as string | null) ?? "";
  const isMermaid = lang === "mermaid";

  // Non-mermaid: render the standard editable <pre><code> via NodeViewContent.
  if (!isMermaid) {
    return (
      <NodeViewWrapper as="div" className="my-3">
        <pre className="bg-muted text-fg rounded-md p-3 overflow-x-auto">
          <NodeViewContent as="code" />
        </pre>
      </NodeViewWrapper>
    );
  }

  return <MermaidPreview {...props} />;
}

function MermaidPreview(props: ReactNodeViewProps) {
  const { node } = props;
  const src = node.textContent;
  const [mode, setMode] = useState<"preview" | "source">("preview");
  const [svg, setSvg] = useState<string>("");
  const [error, setError] = useState<string | null>(null);
  const renderId = useId().replace(/:/g, "");
  // Mermaid requires a unique id per render() call. A monotonically bumped
  // counter sidesteps Math.random (impure) while still guaranteeing uniqueness.
  const callCounter = useRef(0);

  useEffect(() => {
    if (mode !== "preview") return;
    let cancelled = false;
    const trimmed = src.trim();
    callCounter.current += 1;
    const callId = `mermaid-${renderId}-${callCounter.current}`;
    void (async () => {
      if (!trimmed) {
        if (cancelled) return;
        setSvg("");
        setError(null);
        return;
      }
      try {
        const m = await getMermaid();
        const out = await m.render(callId, trimmed);
        if (cancelled) return;
        setSvg(out.svg);
        setError(null);
      } catch (e) {
        if (cancelled) return;
        setError(e instanceof Error ? e.message : String(e));
        setSvg("");
      }
    })();
    return () => { cancelled = true; };
  }, [src, mode, renderId]);

  return (
    <NodeViewWrapper
      as="div"
      data-testid="mermaid-node"
      className="my-3 rounded-md border border-border bg-surface overflow-hidden"
    >
      <div className="flex items-center gap-2 px-3 py-1.5 border-b border-border bg-muted/40">
        <span className="text-[11px] font-semibold uppercase tracking-wider text-fg-muted">Mermaid</span>
        {error && (
          <span className="text-[11px] text-destructive">syntax error</span>
        )}
        <div className="ml-auto inline-flex items-center rounded border border-border bg-bg p-0.5 text-fg-muted">
          <button
            type="button"
            data-testid="mermaid-mode-preview"
            aria-pressed={mode === "preview"}
            onClick={() => setMode("preview")}
            className={`inline-flex items-center gap-1 h-6 px-2 rounded text-[11px] transition-colors ${
              mode === "preview" ? "bg-muted text-fg" : "hover:text-fg"
            }`}
          >
            <Eye size={12} aria-hidden /> Preview
          </button>
          <button
            type="button"
            data-testid="mermaid-mode-source"
            aria-pressed={mode === "source"}
            onClick={() => setMode("source")}
            className={`inline-flex items-center gap-1 h-6 px-2 rounded text-[11px] transition-colors ${
              mode === "source" ? "bg-muted text-fg" : "hover:text-fg"
            }`}
          >
            <Pencil size={12} aria-hidden /> Source
          </button>
        </div>
      </div>
      {mode === "preview" ? (
        <div className="p-3">
          {error ? (
            <>
              <div
                data-testid="mermaid-error"
                className="text-[12px] text-destructive font-mono whitespace-pre-wrap mb-2"
              >
                {error}
              </div>
              <pre className="bg-muted text-fg rounded p-2 overflow-x-auto text-[12px] font-mono whitespace-pre-wrap m-0">
                {src}
              </pre>
            </>
          ) : svg ? (
            <div
              data-testid="mermaid-svg"
              className="flex justify-center [&_svg]:max-w-full [&_svg]:h-auto"
              dangerouslySetInnerHTML={{ __html: sanitizeSvg(svg) }}
            />
          ) : (
            <div className="text-fg-muted text-[12px] italic">Empty diagram.</div>
          )}
          {/* Source still exists in the document; hide it from view but keep it
              editable via the keyboard navigation by toggling to source mode. */}
          <div className="hidden" aria-hidden>
            <NodeViewContent as="code" />
          </div>
        </div>
      ) : (
        <pre className="overflow-x-auto">
          <NodeViewContent as="code" spellCheck={false} />
        </pre>
      )}
    </NodeViewWrapper>
  );
}
