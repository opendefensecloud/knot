import { type Editor } from "@tiptap/core";
import { EditorContent, useEditor } from "@tiptap/react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as Y from "yjs";

import { useSession } from "../../auth/SessionContext";
import { blobsApi } from "../../lib/blobs.api";
import { useUi } from "../../stores/ui";

import { encodeAnchor } from "../comments/anchor";
import { createExtensions } from "./extensions";
import { EditorToolbar } from "./EditorToolbar";
import { KnotProvider, type MentionMsg, type ProviderStatus } from "./KnotProvider";

type Pair = { doc: Y.Doc; provider: KnotProvider };

export function KnotEditor({
  docId,
  onStatus,
  role,
}: {
  docId: string;
  onStatus: (s: ProviderStatus) => void;
  role: "owner" | "editor" | "viewer";
}) {
  const [pair, setPair] = useState<Pair | null>(null);

  // Own the Y.Doc + KnotProvider lifecycle inside an effect so React 18
  // StrictMode's double-mount in dev cannot leak a duplicate WebSocket.
  useEffect(() => {
    const doc = new Y.Doc();
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const provider = new KnotProvider({
      url: `${proto}//${window.location.host}/collab/${docId}`,
      doc,
    });
    setPair({ doc, provider });
    onStatus(provider.status);
    const fn = (s: ProviderStatus) => onStatus(s);
    provider.on("status", fn);
    return () => {
      provider.off("status", fn);
      provider.destroy();
      doc.destroy();
      setPair(null);
    };
  }, [docId, onStatus]);

  if (!pair) {
    return (
      <div data-testid="editor-host" style={{ border: "1px solid #e5e5e5", padding: 16, minHeight: 240 }}>
        Connecting…
      </div>
    );
  }
  return <EditorBody pair={pair} role={role} docId={docId} />;
}

const IMAGE_RE = /^image\/(png|jpe?g|gif|webp)$/;
function isImageType(t: string): boolean { return IMAGE_RE.test(t); }

function EditorBody({ pair, role, docId }: { pair: Pair; role: "owner" | "editor" | "viewer"; docId: string }) {
  const session = useSession();
  const sessionUser = session.data && "ok" in session.data ? session.data.ok : null;
  const userColor = useMemo(() => colorFor(sessionUser?.user_id ?? "anon"), [sessionUser]);
  const notify = useUi((s) => s.notify);
  const openCommentSidebar = useUi((s) => s.openCommentSidebar);
  const setPendingAnchor = useUi((s) => s.setPendingAnchor);
  const editorRef = useRef<Editor | null>(null);

  const [presence, setPresence] = useState<Array<{ name: string; color: string }>>([]);

  // Floating "Add comment" button state
  const [addCommentPos, setAddCommentPos] = useState<{ top: number; left: number } | null>(null);
  const [selectionRange, setSelectionRange] = useState<{ from: number; to: number } | null>(null);

  useEffect(() => {
    const { provider } = pair;
    const update = () => {
      const states = Array.from(provider.awareness.getStates().values()) as Array<
        { user?: { name?: string; color?: string } }
      >;
      setPresence(
        states
          .filter((s) => s.user?.name)
          .map((s) => ({ name: s.user!.name!, color: s.user!.color ?? "#666" })),
      );
    };
    provider.awareness.on("change", update);
    update();
    return () => { provider.awareness.off("change", update); };
  }, [pair]);

  // Subscribe to mention push events from the collab WS.
  // NOTE: The server-side pg_notify → WS pipeline is not yet wired (T16 concern).
  // This handler fires when the server eventually sends MSG_MENTION frames.
  useEffect(() => {
    const { provider } = pair;
    const userId = sessionUser?.user_id;
    const onMention = (msg: MentionMsg) => {
      if (!userId || !msg.user_ids.includes(userId)) return;
      notify("info", "You were mentioned in a comment.");
      openCommentSidebar();
    };
    provider.on("mention", onMention);
    return () => { provider.off("mention", onMention); };
  }, [pair, sessionUser?.user_id, notify, openCommentSidebar]);

  const uploadAndInsert = useCallback(async (files: File[]) => {
    for (const f of files) {
      const r = await blobsApi.upload(docId, f);
      if ("error" in r) {
        notify(
          "error",
          r.error.code === "blob.too_large" ? "File too large (10 MB cap)."
            : r.error.code === "blob.blocked_type" ? "File type not allowed."
            : r.error.code === "acl.no_grant" ? "You don't have permission to upload here."
            : "Upload failed.",
        );
        continue;
      }
      const blob = r.ok;
      if (isImageType(blob.content_type)) {
        editorRef.current?.chain().focus().setImage({ src: blob.url }).run();
      } else {
        editorRef.current?.chain().focus().insertContent({
          type: "attachment",
          attrs: {
            url: blob.url,
            name: blob.original_name ?? f.name,
            size: blob.byte_size,
            contentType: blob.content_type,
          },
        }).run();
      }
    }
  }, [docId, notify]);

  const canComment = role === "owner" || role === "editor";

  const editor = useEditor(
    {
      extensions: createExtensions({
        doc: pair.doc,
        awareness: pair.provider.awareness,
        user: { name: sessionUser?.display_name ?? "Anonymous", color: userColor },
      }),
      editable: role !== "viewer",
      editorProps: {
        handleDrop(_view, event, _slice, _moved) {
          const files = Array.from((event as DragEvent).dataTransfer?.files ?? []);
          if (files.length === 0) return false;
          event.preventDefault();
          void uploadAndInsert(files);
          return true;
        },
        handlePaste(_view, event) {
          const files = Array.from((event as ClipboardEvent).clipboardData?.files ?? []);
          if (files.length === 0) return false;
          event.preventDefault();
          void uploadAndInsert(files);
          return true;
        },
      },
      onSelectionUpdate: ({ editor: ed }) => {
        if (!canComment) return;
        const { from, to } = ed.state.selection;
        if (from === to) {
          setAddCommentPos(null);
          setSelectionRange(null);
          return;
        }
        const coords = ed.view.coordsAtPos(from);
        const editorDom = ed.view.dom.getBoundingClientRect();
        setAddCommentPos({
          top: coords.top - editorDom.top - 32,
          left: Math.max(0, coords.left - editorDom.left),
        });
        setSelectionRange({ from, to });
      },
    },
    [pair, sessionUser?.user_id, role, userColor, uploadAndInsert, canComment],
  );

  // Keep ref in sync so uploadAndInsert (stable callback) can reach the latest editor instance.
  editorRef.current = editor ?? null;

  function handleAddComment() {
    if (!editor || !selectionRange) return;
    const { from, to } = selectionRange;
    const anchorText = editor.state.doc.textBetween(from, to, " ").slice(0, 120);
    const positionY = encodeAnchor(editor, pair.doc, from);
    setPendingAnchor({
      positionY: positionY ?? "",
      anchorText,
    });
    openCommentSidebar();
    setAddCommentPos(null);
  }

  return (
    <>
      <div data-testid="presence-bar" style={{ marginBottom: 8 }}>
        {presence.map((p, i) => (
          <span
            key={i}
            style={{
              display: "inline-block",
              padding: "2px 6px",
              borderRadius: 4,
              background: p.color,
              color: "white",
              marginRight: 4,
              fontSize: 12,
            }}
          >
            {p.name}
          </span>
        ))}
      </div>
      {role !== "viewer" && <EditorToolbar editor={editor} />}
      <div
        data-testid="editor-host"
        style={{ border: "1px solid #e5e5e5", padding: 16, minHeight: 240, position: "relative" }}
      >
        {/* Floating "Add comment" button */}
        {canComment && addCommentPos && (
          <button
            type="button"
            data-testid="add-comment-float"
            onClick={handleAddComment}
            style={{
              position: "absolute",
              top: addCommentPos.top,
              left: addCommentPos.left,
              zIndex: 20,
              background: "#0050ff",
              color: "white",
              border: "none",
              borderRadius: 4,
              padding: "3px 8px",
              fontSize: 12,
              cursor: "pointer",
              boxShadow: "0 2px 8px rgba(0,0,0,0.2)",
            }}
          >
            Add comment
          </button>
        )}
        <EditorContent editor={editor} />
      </div>
    </>
  );
}

function colorFor(id: string): string {
  let hash = 0;
  for (let i = 0; i < id.length; i += 1) hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  return `hsl(${hash % 360}, 70%, 45%)`;
}
