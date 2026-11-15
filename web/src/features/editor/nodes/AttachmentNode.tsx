import { Node, mergeAttributes } from "@tiptap/core";
import { NodeViewWrapper, ReactNodeViewRenderer, type ReactNodeViewProps } from "@tiptap/react";

type AttachmentAttrs = {
  url: string;
  name: string;
  size: number;
  contentType: string;
};

function Renderer({ node }: ReactNodeViewProps) {
  const attrs = node.attrs as AttachmentAttrs;
  const kb = Math.max(1, Math.round(attrs.size / 1024));
  return (
    <NodeViewWrapper
      as="div"
      data-testid="attachment-node"
      data-history-change={(node.attrs.historyChange as string | null | undefined) ?? undefined}
      style={{
        display: "inline-flex",
        gap: 8,
        padding: 8,
        margin: "4px 0",
        border: "1px solid #e5e5e5",
        borderRadius: 6,
        background: "#fafafa",
        alignItems: "center",
      }}
    >
      <span aria-hidden>📎</span>
      <a
        href={attrs.url}
        target="_blank"
        rel="noopener noreferrer"
        download={attrs.name}
        style={{ color: "#0050ff", textDecoration: "none" }}
      >
        {attrs.name}
      </a>
      <span style={{ color: "#888", fontSize: 12 }}>({kb} KB)</span>
    </NodeViewWrapper>
  );
}

export const Attachment = Node.create({
  name: "attachment",
  group: "block",
  atom: true,
  addAttributes() {
    return {
      url:         { default: "" },
      name:        { default: "file" },
      size:        { default: 0 },
      contentType: { default: "application/octet-stream" },
    };
  },
  parseHTML() {
    return [{ tag: "div[data-attachment]" }];
  },
  renderHTML({ HTMLAttributes }) {
    return ["div", mergeAttributes(HTMLAttributes, { "data-attachment": "true" })];
  },
  addNodeView() {
    return ReactNodeViewRenderer(Renderer);
  },
});
