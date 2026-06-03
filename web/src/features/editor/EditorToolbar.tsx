import { useState, type ReactNode } from "react";

import type { Editor } from "@tiptap/react";
import {
  Bold,
  Code,
  Code2,
  Heading1,
  Heading2,
  Heading3,
  Italic,
  Link as LinkIcon,
  List,
  ListOrdered,
  Network,
  Quote,
  Strikethrough,
} from "lucide-react";

type ToolbarBtnProps = {
  testId: string;
  label: string;
  active?: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
};

function Btn({ testId, label, active, disabled, onClick, children }: ToolbarBtnProps) {
  return (
    <button
      type="button"
      data-testid={testId}
      title={label}
      aria-label={label}
      aria-pressed={active}
      disabled={disabled}
      onClick={onClick}
      className={`inline-flex items-center justify-center h-8 min-w-8 px-2 rounded text-fg-muted hover:text-fg hover:bg-muted transition-colors ease-swift duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:opacity-40 disabled:cursor-not-allowed ${active ? "bg-muted text-fg" : ""}`}
    >
      {children}
    </button>
  );
}

function Sep() {
  return <span className="mx-1 h-5 w-px bg-border" aria-hidden />;
}

export function EditorToolbar({ editor }: { editor: Editor | null }) {
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkUrl, setLinkUrl] = useState("");

  if (!editor) return null;
  const c = () => editor.chain().focus();

  return (
    <div
      data-testid="editor-toolbar"
      className="sticky top-0 z-10 flex items-center gap-0.5 flex-wrap py-2 mb-4 bg-bg/80 backdrop-blur border-b border-border relative"
    >
      <Btn testId="toolbar-bold" label="Bold" active={editor.isActive("bold")}
        onClick={() => c().toggleBold().run()}>
        <Bold size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-italic" label="Italic" active={editor.isActive("italic")}
        onClick={() => c().toggleItalic().run()}>
        <Italic size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-strike" label="Strikethrough" active={editor.isActive("strike")}
        onClick={() => c().toggleStrike().run()}>
        <Strikethrough size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-code" label="Inline code" active={editor.isActive("code")}
        onClick={() => c().toggleCode().run()}>
        <Code size={15} aria-hidden />
      </Btn>
      <Sep />
      <Btn testId="toolbar-h1" label="Heading 1" active={editor.isActive("heading", { level: 1 })}
        onClick={() => c().toggleHeading({ level: 1 }).run()}>
        <Heading1 size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-h2" label="Heading 2" active={editor.isActive("heading", { level: 2 })}
        onClick={() => c().toggleHeading({ level: 2 }).run()}>
        <Heading2 size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-h3" label="Heading 3" active={editor.isActive("heading", { level: 3 })}
        onClick={() => c().toggleHeading({ level: 3 }).run()}>
        <Heading3 size={15} aria-hidden />
      </Btn>
      <Sep />
      <Btn testId="toolbar-bullet-list" label="Bullet list" active={editor.isActive("bulletList")}
        onClick={() => c().toggleBulletList().run()}>
        <List size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-ordered-list" label="Ordered list" active={editor.isActive("orderedList")}
        onClick={() => c().toggleOrderedList().run()}>
        <ListOrdered size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-blockquote" label="Quote" active={editor.isActive("blockquote")}
        onClick={() => c().toggleBlockquote().run()}>
        <Quote size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-code-block" label="Code block" active={editor.isActive("code_block")}
        onClick={() => c().toggleCodeBlock().run()}>
        <Code2 size={15} aria-hidden />
      </Btn>
      <Btn
        testId="toolbar-mermaid"
        label="Insert diagram"
        onClick={() =>
          c()
            .insertContent({
              type: "code_block",
              attrs: { language: "mermaid" },
              content: [
                {
                  type: "text",
                  text: "graph TD\n  A[Start] --> B{Decision}\n  B -->|Yes| C[OK]\n  B -->|No| D[End]",
                },
              ],
            })
            .run()
        }
      >
        <Network size={15} aria-hidden />
      </Btn>
      <Sep />
      <Btn
        testId="toolbar-link"
        label="Link"
        active={editor.isActive("link")}
        onClick={() => {
          const current = editor.getAttributes("link").href as string | undefined;
          setLinkUrl(current ?? "");
          setLinkOpen(true);
        }}
      >
        <LinkIcon size={15} aria-hidden />
      </Btn>
      {linkOpen && (
        <div
          data-testid="link-popover"
          className="absolute top-full left-0 mt-1 flex items-center gap-1 p-2 rounded-md bg-surface border border-border shadow-lg z-20"
          onKeyDown={(e) => { if (e.key === "Escape") setLinkOpen(false); }}
        >
          <input
            data-testid="link-input"
            type="url"
            value={linkUrl}
            onChange={(e) => setLinkUrl(e.target.value)}
            placeholder="https://"
            className="h-8 px-2 min-w-[240px] rounded border border-border bg-bg text-fg text-sm focus:outline-none focus:ring-2 focus:ring-accent"
            autoFocus
          />
          <button
            data-testid="link-apply"
            type="button"
            className="h-8 px-2.5 rounded bg-accent text-accent-fg text-[13px] font-medium hover:opacity-90 transition-opacity"
            onClick={() => {
              if (linkUrl) c().extendMarkRange("link").setLink({ href: linkUrl }).run();
              else c().unsetLink().run();
              setLinkOpen(false);
            }}
          >
            Apply
          </button>
          <button
            data-testid="link-remove"
            type="button"
            className="h-8 px-2.5 rounded text-fg-muted hover:text-fg hover:bg-muted text-[13px] transition-colors"
            onClick={() => {
              c().unsetLink().run();
              setLinkOpen(false);
            }}
          >
            Remove
          </button>
        </div>
      )}
    </div>
  );
}
