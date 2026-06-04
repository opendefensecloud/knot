import { useMemo, useRef, useState, type ReactNode } from "react";

import type { Editor } from "@tiptap/react";
import { useQuery } from "@tanstack/react-query";

import { docsApi } from "../docs/docs.api";
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
  ListChecks,
  ListOrdered,
  Paperclip,
  Table as TableIcon,
  Network,
  PenSquare,
  Quote,
  Strikethrough,
} from "lucide-react";

import { boardsApi } from "../../lib/boards.api";
import { useUi } from "../../stores/ui";

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

export function EditorToolbar({
  editor,
  docId,
  onUploadFiles,
}: {
  editor: Editor | null;
  docId: string;
  onUploadFiles?: (files: File[]) => void;
}) {
  const fileInputRef = useRef<HTMLInputElement | null>(null);
  const [linkOpen, setLinkOpen] = useState(false);
  const [linkUrl, setLinkUrl] = useState("");
  const [linkMode, setLinkMode] = useState<"url" | "doc">("url");
  const [docQuery, setDocQuery] = useState("");
  const docs = useQuery({
    queryKey: ["docs"],
    queryFn: () => docsApi.list(),
    staleTime: 30_000,
    enabled: linkOpen && linkMode === "doc",
  });
  const docMatches = useMemo(() => {
    const data = docs.data && "ok" in docs.data ? docs.data.ok : [];
    const q = docQuery.trim().toLowerCase();
    const filtered = q ? data.filter((d) => d.title.toLowerCase().includes(q)) : data;
    return filtered.slice(0, 8);
  }, [docs.data, docQuery]);
  const notify = useUi((s) => s.notify);

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
      <Btn testId="toolbar-bullet-list" label="Bullet list" active={editor.isActive("bullet_list")}
        onClick={() => c().toggleBulletList().run()}>
        <List size={15} aria-hidden />
      </Btn>
      <Btn testId="toolbar-ordered-list" label="Ordered list" active={editor.isActive("ordered_list")}
        onClick={() => c().toggleOrderedList().run()}>
        <ListOrdered size={15} aria-hidden />
      </Btn>
      <Btn
        testId="toolbar-attachment"
        label="Attach file"
        disabled={!onUploadFiles}
        onClick={() => fileInputRef.current?.click()}
      >
        <Paperclip size={15} aria-hidden />
      </Btn>
      <input
        ref={fileInputRef}
        type="file"
        multiple
        className="hidden"
        data-testid="toolbar-attachment-input"
        onChange={(e) => {
          const files = Array.from(e.target.files ?? []);
          if (files.length > 0) onUploadFiles?.(files);
          // Reset so the same file can be re-uploaded.
          e.target.value = "";
        }}
      />
      <Btn
        testId="toolbar-table"
        label="Insert table"
        onClick={() =>
          c()
            .insertTable({ rows: 3, cols: 3, withHeaderRow: true })
            .run()
        }
      >
        <TableIcon size={15} aria-hidden />
      </Btn>
      <Btn
        testId="toolbar-task-list"
        label="Task list"
        active={editor.isActive("list_item", { checked: false }) || editor.isActive("list_item", { checked: true })}
        onClick={() =>
          c().toggleList("bullet_list", "list_item").updateAttributes("list_item", { checked: false }).run()
        }
      >
        <ListChecks size={15} aria-hidden />
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
      <Btn
        testId="toolbar-excalidraw"
        label="Insert Excalidraw diagram"
        onClick={() => {
          void (async () => {
            const res = await boardsApi.create(docId, null);
            if ("error" in res) {
              notify("error", "Failed to create diagram.");
              return;
            }
            c()
              .insertContent({
                type: "excalidraw_board",
                attrs: { board_id: res.ok.id, label: null },
              })
              .run();
          })();
        }}
      >
        <PenSquare size={15} aria-hidden />
      </Btn>
      <Sep />
      <Btn
        testId="toolbar-link"
        label="Link"
        active={editor.isActive("link")}
        onClick={() => {
          const current = editor.getAttributes("link").href as string | undefined;
          setLinkUrl(current ?? "");
          setLinkMode(current?.startsWith("knot://doc/") ? "doc" : "url");
          setDocQuery("");
          setLinkOpen(true);
        }}
      >
        <LinkIcon size={15} aria-hidden />
      </Btn>
      {linkOpen && (
        <div
          data-testid="link-popover"
          className="absolute top-full left-0 mt-1 p-2 rounded-md bg-surface border border-border shadow-lg z-20 min-w-[300px]"
          onKeyDown={(e) => { if (e.key === "Escape") setLinkOpen(false); }}
        >
          <div className="flex items-center gap-1 mb-2 p-0.5 rounded bg-muted/40 text-[11px] font-medium">
            <button
              data-testid="link-mode-url"
              type="button"
              className={`flex-1 h-6 rounded transition-colors ${linkMode === "url" ? "bg-surface text-fg shadow-sm" : "text-fg-muted hover:text-fg"}`}
              onClick={() => setLinkMode("url")}
            >
              URL
            </button>
            <button
              data-testid="link-mode-doc"
              type="button"
              className={`flex-1 h-6 rounded transition-colors ${linkMode === "doc" ? "bg-surface text-fg shadow-sm" : "text-fg-muted hover:text-fg"}`}
              onClick={() => setLinkMode("doc")}
            >
              Document
            </button>
          </div>
          {linkMode === "url" ? (
            <div className="flex items-center gap-1">
              <input
                data-testid="link-input"
                type="url"
                value={linkUrl}
                onChange={(e) => setLinkUrl(e.target.value)}
                placeholder="https://"
                className="h-8 px-2 flex-1 min-w-[200px] rounded border border-border bg-bg text-fg text-sm focus:outline-none focus:ring-2 focus:ring-accent"
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
          ) : (
            <div>
              <input
                data-testid="link-doc-search"
                type="text"
                value={docQuery}
                onChange={(e) => setDocQuery(e.target.value)}
                placeholder="Search documents…"
                className="h-8 px-2 w-full rounded border border-border bg-bg text-fg text-sm focus:outline-none focus:ring-2 focus:ring-accent"
                autoFocus
              />
              <ul className="mt-1 max-h-56 overflow-auto" data-testid="link-doc-results">
                {docMatches.length === 0 ? (
                  <li className="px-2 py-1.5 text-xs text-fg-muted">
                    {docs.isLoading ? "Loading…" : "No matching documents"}
                  </li>
                ) : (
                  docMatches.map((d) => (
                    <li key={d.id}>
                      <button
                        type="button"
                        className="block w-full text-left px-2 py-1.5 rounded text-sm text-fg hover:bg-muted focus:bg-muted focus:outline-none"
                        onClick={() => {
                          const href = `knot://doc/${d.id}`;
                          // If no text is selected, insert the doc title as the
                          // link's text. Otherwise wrap the current selection.
                          const { from, to } = editor.state.selection;
                          if (from === to) {
                            c()
                              .insertContent({
                                type: "text",
                                text: d.title || "Untitled",
                                marks: [{ type: "link", attrs: { href } }],
                              })
                              .run();
                          } else {
                            c().extendMarkRange("link").setLink({ href }).run();
                          }
                          setLinkOpen(false);
                        }}
                      >
                        {d.title || <span className="text-fg-muted">Untitled</span>}
                      </button>
                    </li>
                  ))
                )}
              </ul>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
