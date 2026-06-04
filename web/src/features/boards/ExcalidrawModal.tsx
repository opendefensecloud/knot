/**
 * ExcalidrawModal — full-screen overlay that mounts an Excalidraw canvas
 * bound to a `/collab/board/:id` Y.Doc.
 *
 * Lifecycle:
 *   mount  → new Y.Doc + BoardProvider; lazy-import Excalidraw; bind on api.
 *   close  → exportToSvg → PUT /api/boards/:id/svg (fire-and-forget),
 *            then provider.destroy() + doc.destroy().
 *
 * Awareness: our own pointer is written to `awareness.local.pointer` on
 * each `onPointerUpdate`; remote peers are rebuilt from the awareness map
 * on every change and piped into Excalidraw via `updateScene({ collaborators })`.
 *
 * Excalidraw's own collaboration UI is suppressed (`isCollaborating=false`
 * and CanvasActions trimmed) since we own the lifecycle.
 */

import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState } from "react";

// Vite injects `import.meta.env.DEV` at build time; the package ships its own
// `vite/client` ambient types but we don't reference them from tsconfig.json,
// so narrow `import.meta` locally for the two `DEV` checks below.
const importMetaEnv = (import.meta as unknown as { env?: { DEV?: boolean } }).env;
const IS_DEV = !!importMetaEnv?.DEV;
import { useQueryClient } from "@tanstack/react-query";
import * as Y from "yjs";
import type {
  Collaborator,
  ExcalidrawImperativeAPI,
  SocketId,
} from "@excalidraw/excalidraw/types";
import type { ExcalidrawElement } from "@excalidraw/excalidraw/element/types";

import { BoardProvider } from "./BoardProvider";
import { bindExcalidraw, type ExcalidrawBinding } from "./yBinding";
import { boardsApi } from "../../lib/boards.api";
import { useSession } from "../../auth/SessionContext";
import { colorFor } from "../../components/ui/Avatar";

// Lazy-load Excalidraw (and its CSS) so it ships as its own chunk.
const Excalidraw = lazy(async () => {
  const mod = await import("@excalidraw/excalidraw");
  // Side-effect import for the package's stylesheet. The shim in
  // src/excalidraw-css.d.ts gives this path a type declaration.
  await import("@excalidraw/excalidraw/index.css");
  // Test hook: expose the `convertToExcalidrawElements` skeleton-to-element
  // helper on window in dev so e2e specs can build valid elements without
  // hand-rolling every required field.
  if (IS_DEV) {
    (window as unknown as {
      __excalidrawConvert?: typeof mod.convertToExcalidrawElements;
    }).__excalidrawConvert = mod.convertToExcalidrawElements;
  }
  return { default: mod.Excalidraw };
});

async function saveSvgSnapshot(
  api: ExcalidrawImperativeAPI,
  boardId: string,
): Promise<void> {
  try {
    // Typed alias around the dynamic import: the package's `.d.ts` includes
    // scss side-effect imports that confuse ESLint's type checker on the
    // raw `import(...)` Promise, so we narrow to a hand-written shape.
    type ExportToSvg = (opts: {
      elements: ReturnType<ExcalidrawImperativeAPI["getSceneElements"]>;
      appState: Record<string, unknown>;
      files: ReturnType<ExcalidrawImperativeAPI["getFiles"]>;
    }) => Promise<SVGSVGElement>;
    const mod = (await import("@excalidraw/excalidraw")) as unknown as {
      exportToSvg: ExportToSvg;
    };
    const elements = api.getSceneElements();
    // Skip the PUT if there's nothing meaningful to render. `getSceneElements`
    // already filters out elements with `isDeleted: true`, so empty here
    // means the canvas is genuinely blank — saving would overwrite a real
    // cached preview with a 256-byte white placeholder.
    if (elements.length === 0) return;
    const appState = api.getAppState();
    const files = api.getFiles();
    const svg = await mod.exportToSvg({
      elements,
      appState: { ...appState, exportBackground: true },
      files,
    });
    const text = new XMLSerializer().serializeToString(svg);
    await boardsApi.putSvg(boardId, text);
  } catch (err) {
    // best-effort — the SVG cache is recoverable on next render
    console.warn("board svg snapshot failed", err);
  }
}

type AwarenessPointerState = {
  user?: { name?: string; color?: string };
  pointer?: { x: number; y: number };
};

export function ExcalidrawModal({
  boardId,
  label,
  onLabelChange,
  onClose,
}: {
  boardId: string;
  label: string | null;
  onLabelChange: (next: string) => void;
  onClose: () => void;
}) {
  const qc = useQueryClient();
  const session = useSession();
  const sessionUser = session.data && "ok" in session.data ? session.data.ok : null;
  const userName = sessionUser?.display_name ?? "Anonymous";
  const userColor = useMemo(
    () => colorFor(sessionUser?.user_id ?? "anon"),
    [sessionUser?.user_id],
  );
  const [ready, setReady] = useState(false);
  const [synced, setSynced] = useState(false);
  const docRef = useRef<Y.Doc | null>(null);
  const providerRef = useRef<BoardProvider | null>(null);
  const apiRef = useRef<ExcalidrawImperativeAPI | null>(null);
  const bindingRef = useRef<ExcalidrawBinding | null>(null);
  const saveTimerRef = useRef<number | null>(null);

  // Debounced periodic SVG snapshot during editing. The save-on-close in the
  // mount effect remains the final commitment; this just keeps the cached
  // SVG fresh for other open NodeViews while the modal is open.
  const scheduleSave = useCallback(() => {
    if (saveTimerRef.current !== null) {
      window.clearTimeout(saveTimerRef.current);
    }
    saveTimerRef.current = window.setTimeout(() => {
      saveTimerRef.current = null;
      const api = apiRef.current;
      if (!api) return;
      void saveSvgSnapshot(api, boardId)
        .then(() => {
          void qc.invalidateQueries({ queryKey: ["board-svg", boardId] });
        })
        .catch((err: unknown) => {
          console.warn("board svg snapshot failed", err);
        });
    }, 300);
  }, [boardId, qc]);

  // Build Y.Doc + provider once. Save SVG on unmount.
  useEffect(() => {
    const doc = new Y.Doc();
    const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
    const provider = new BoardProvider({
      url: `${proto}//${window.location.host}/collab/board/${boardId}`,
      doc,
    });
    docRef.current = doc;
    providerRef.current = provider;
    // Effect runs once on mount; setReady here is intentional so the
    // lazy Excalidraw can render after refs are populated.
    // eslint-disable-next-line react-hooks/set-state-in-effect
    setReady(true);
    // If the provider already has remote state by the time we attach (e.g.
    // reopening a modal in the same session), treat as synced immediately.
    if (provider.synced) {
      setSynced(true);
    } else {
      provider.on("synced", () => setSynced(true));
    }

    return () => {
      // Cancel any pending debounced save; the close-save below is the final
      // commitment so we don't need to flush the timer.
      if (saveTimerRef.current !== null) {
        window.clearTimeout(saveTimerRef.current);
        saveTimerRef.current = null;
      }
      // Fire-and-forget SVG snapshot. Don't block close.
      const api = apiRef.current;
      if (api) {
        void saveSvgSnapshot(api, boardId);
      }

      bindingRef.current?.destroy();
      bindingRef.current = null;
      provider.destroy();
      doc.destroy();
      docRef.current = null;
      providerRef.current = null;
      apiRef.current = null;
    };
  }, [boardId]);

  // Identify ourselves in awareness so peers see a real name + color instead
  // of "anon". Re-runs if the session user updates while the modal is open.
  useEffect(() => {
    const provider = providerRef.current;
    if (!provider || !ready) return;
    provider.awareness.setLocalStateField("user", { name: userName, color: userColor });
  }, [ready, userName, userColor]);

  // ESC closes.
  useEffect(() => {
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Push remote awareness → Excalidraw collaborators map.
  useEffect(() => {
    const provider = providerRef.current;
    if (!provider || !ready) return;
    const awareness = provider.awareness;
    const localId = awareness.clientID;

    function syncCollaborators() {
      const api = apiRef.current;
      if (!api) return;
      const collaborators = new Map<SocketId, Collaborator>();
      for (const [clientId, raw] of awareness.getStates()) {
        if (clientId === localId) continue;
        const state = raw as AwarenessPointerState;
        if (!state.pointer) continue;
        const color = state.user?.color ?? "#888888";
        const entry: Collaborator = {
          username: state.user?.name ?? "anon",
          pointer: { x: state.pointer.x, y: state.pointer.y, tool: "pointer" },
          color: { background: color, stroke: color },
        };
        collaborators.set(String(clientId) as SocketId, entry);
      }
      api.updateScene({ collaborators });
    }

    awareness.on("change", syncCollaborators);
    syncCollaborators();
    return () => {
      awareness.off("change", syncCollaborators);
    };
  }, [ready]);

  const [apiReady, setApiReady] = useState(false);
  function handleApi(api: ExcalidrawImperativeAPI) {
    apiRef.current = api;
    setApiReady(true);
    // Test hook: expose the Excalidraw API on window in dev so e2e specs
    // can drive shapes deterministically without simulating canvas events.
    if (IS_DEV) {
      (window as unknown as { __excalidrawAPI?: ExcalidrawImperativeAPI }).__excalidrawAPI = api;
    }
  }

  // Bind only once BOTH the Excalidraw API is mounted AND the provider has
  // received its first SYNC_STEP_2. Binding earlier risks the mount-time
  // `onChange([])` running the delete-missing branch against remote state,
  // which would wipe the board for every peer.
  useEffect(() => {
    if (!apiReady || !synced) return;
    const api = apiRef.current;
    const doc = docRef.current;
    if (!api || !doc) return;
    bindingRef.current = bindExcalidraw(api, doc);
    return () => {
      bindingRef.current?.destroy();
      bindingRef.current = null;
    };
  }, [apiReady, synced]);

  const handleChange = useCallback(
    (next: readonly ExcalidrawElement[]) => {
      bindingRef.current?.onChange(next);
      scheduleSave();
    },
    [scheduleSave],
  );

  // Seed Excalidraw with the doc's current elements at mount time, so it
  // never has an "empty initial scene" phase whose mount-onChange could
  // race ahead of our bind effect and wipe Y. Snapshotted once when sync
  // flips true. Later remote updates flow through observeDeep →
  // pushToExcalidraw inside yBinding; initialData is read-once at mount.
  const [initialData, setInitialData] = useState<{
    elements: ExcalidrawElement[];
    scrollToContent: boolean;
  } | null>(null);
  useEffect(() => {
    if (!synced) return;
    const doc = docRef.current;
    if (!doc) return;
    const elementsMap = doc.getMap<ExcalidrawElement>("elements");
    setInitialData({
      elements: Array.from(elementsMap.values()),
      scrollToContent: true,
    });
  }, [synced]);

  function handlePointerUpdate(payload: {
    pointer: { x: number; y: number; tool: "pointer" | "laser" };
  }) {
    const provider = providerRef.current;
    if (!provider) return;
    provider.awareness.setLocalStateField("pointer", {
      x: payload.pointer.x,
      y: payload.pointer.y,
    });
  }

  return (
    <div
      data-testid="excalidraw-modal"
      className="fixed inset-0 z-50 bg-black/70 flex flex-col"
      onClick={(e) => {
        // Backdrop click closes only when clicking the overlay itself.
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex items-center justify-between gap-3 px-4 py-2 bg-surface border-b border-border">
        <input
          type="text"
          value={label ?? ""}
          placeholder="Untitled diagram"
          onChange={(e) => onLabelChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" || e.key === "Escape") e.currentTarget.blur();
          }}
          className="flex-1 min-w-0 max-w-md bg-transparent text-sm font-medium text-fg placeholder:text-fg-muted focus:outline-none"
          data-testid="excalidraw-modal-label"
          aria-label="Diagram title"
        />
        <button
          type="button"
          className="text-sm text-fg-muted hover:text-fg"
          onClick={onClose}
          data-testid="excalidraw-modal-close"
        >
          Close
        </button>
      </div>
      <div className="flex-1 bg-white">
        {ready && synced && initialData ? (
          <Suspense
            fallback={
              <div className="h-full grid place-items-center text-fg-muted text-sm">
                Loading…
              </div>
            }
          >
            <Excalidraw
              excalidrawAPI={handleApi}
              initialData={initialData}
              onChange={handleChange}
              onPointerUpdate={handlePointerUpdate}
              isCollaborating={false}
              UIOptions={{
                canvasActions: {
                  toggleTheme: false,
                  loadScene: false,
                  saveToActiveFile: false,
                  export: false,
                  saveAsImage: false,
                },
              }}
            />
          </Suspense>
        ) : (
          <div className="h-full grid place-items-center text-fg-muted text-sm">
            {ready ? "Syncing…" : "Connecting…"}
          </div>
        )}
      </div>
    </div>
  );
}
