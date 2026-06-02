import { EditorContent, useEditor } from "@tiptap/react";
import { useEffect, useMemo, useState } from "react";
import * as Y from "yjs";

import { useSession } from "../../auth/SessionContext";

import { createExtensions } from "./extensions";
import { KnotProvider, type ProviderStatus } from "./KnotProvider";

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
  return <EditorBody pair={pair} role={role} />;
}

function EditorBody({ pair, role }: { pair: Pair; role: "owner" | "editor" | "viewer" }) {
  const session = useSession();
  const sessionUser = session.data && "ok" in session.data ? session.data.ok : null;
  const userColor = useMemo(() => colorFor(sessionUser?.user_id ?? "anon"), [sessionUser]);

  const [presence, setPresence] = useState<Array<{ name: string; color: string }>>([]);

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

  const editor = useEditor(
    {
      extensions: createExtensions({
        doc: pair.doc,
        awareness: pair.provider.awareness,
        user: { name: sessionUser?.display_name ?? "Anonymous", color: userColor },
      }),
      editable: role !== "viewer",
    },
    [pair, sessionUser?.user_id, role, userColor],
  );

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
      <div data-testid="editor-host" style={{ border: "1px solid #e5e5e5", padding: 16, minHeight: 240 }}>
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
