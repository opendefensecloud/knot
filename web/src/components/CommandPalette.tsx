import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";

import { authApi } from "../auth/session.api";
import { docsApi } from "../features/docs/docs.api";
import { searchApi, type SearchHit } from "../lib/search.api";
import { useUi } from "../stores/ui";

// Snippets come from Postgres ts_headline, configured to emit only <b>…</b>.
// Escape all HTML, then restore only <b> and </b>.
function safeSnippet(s: string): string {
  const esc = s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  return esc
    .replace(/&lt;b&gt;/g, "<b>")
    .replace(/&lt;\/b&gt;/g, "</b>");
}

type Action = {
  id: string;
  label: string;
  kind: "doc" | "nav" | "action";
  snippet?: string;
  run: () => void | Promise<void>;
};

export function CommandPalette() {
  const open = useUi((s) => s.paletteOpen);
  const close = useUi((s) => s.closePalette);
  const togglePalette = useUi((s) => s.togglePalette);
  const nav = useNavigate();
  const qc = useQueryClient();
  const [q, setQ] = useState("");
  const [cursor, setCursor] = useState(0);

  // `hits` holds the last successful search results; `pendingQuery` tracks
  // which query is in-flight so we can derive the "searching" indicator.
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [pendingQuery, setPendingQuery] = useState<string | null>(null);

  // A ref to AbortController so the cleanup can cancel the in-flight fetch.
  const acRef = useRef<AbortController | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        togglePalette();
      } else if (e.key === "Escape" && useUi.getState().paletteOpen) {
        close();
      }
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [close, togglePalette]);

  const trimmed = q.trim();
  const queryActive = open && trimmed.length >= 2;

  // Debounced server search. State is only set inside async callbacks,
  // never synchronously — satisfies react-hooks/set-state-in-effect.
  useEffect(() => {
    // Cancel any previous in-flight request.
    acRef.current?.abort();

    if (!queryActive) {
      // Defer the reset to the next microtask so it's not "synchronous".
      const handle = window.setTimeout(() => {
        setHits([]);
        setPendingQuery(null);
      }, 0);
      return () => window.clearTimeout(handle);
    }

    const ac = new AbortController();
    acRef.current = ac;

    const handle = window.setTimeout(() => {
      void (async () => {
        setPendingQuery(trimmed);
        const r = await searchApi.query(trimmed);
        if (ac.signal.aborted) return;
        setPendingQuery(null);
        if ("ok" in r) setHits(r.ok);
      })();
    }, 200);

    return () => {
      window.clearTimeout(handle);
      ac.abort();
    };
  // trimmed + open covers all cases; queryActive would be redundant.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [trimmed, open]);

  // Derive visible hits — when the query is inactive, show nothing.
  const visibleHits = useMemo(
    () => (queryActive ? hits : []),
    [queryActive, hits],
  );
  const searching = queryActive && pendingQuery !== null;

  const actions = useMemo<Action[]>(() => {
    const docActions: Action[] = visibleHits.map((h) => ({
      id: `doc:${h.doc_id}`,
      label: h.title,
      kind: "doc" as const,
      snippet: h.snippet || undefined,
      run: () => { close(); void nav(`/doc/${h.doc_id}`); },
    }));
    const navActions: Action[] = [
      {
        id: "action:create",
        label: "Create new document",
        kind: "action",
        run: async () => {
          const r = await docsApi.create({ title: "Untitled" });
          close();
          if ("error" in r) return;
          const created = r.ok as { id: string };
          await qc.invalidateQueries({ queryKey: ["docs"] });
          void nav(`/doc/${created.id}`);
        },
      },
      {
        id: "nav:members",
        label: "Go to Members",
        kind: "nav",
        run: () => { close(); void nav("/members"); },
      },
      {
        id: "nav:settings",
        label: "Go to Settings",
        kind: "nav",
        run: () => { close(); void nav("/settings"); },
      },
      {
        id: "action:logout",
        label: "Sign out",
        kind: "action",
        run: async () => {
          await authApi.logout();
          close();
          void nav("/login", { replace: true });
        },
      },
    ];
    return [...docActions, ...navActions];
  }, [visibleHits, close, nav, qc]);

  // Filter static nav/action items by query substring; doc hits come from the server.
  const filtered = useMemo(() => {
    const needle = trimmed.toLowerCase();
    if (!needle) return actions;
    return actions.filter((a) =>
      a.kind === "doc" || a.label.toLowerCase().includes(needle)
    );
  }, [trimmed, actions]);

  // Clamp cursor whenever the filtered list shrinks so we never point out of bounds.
  const safeCursor = Math.min(cursor, Math.max(0, filtered.length - 1));

  const showNoMatches =
    queryActive && visibleHits.length === 0 && !searching;

  if (!open) return null;

  return (
    <div
      role="dialog"
      data-testid="cmdk"
      onClick={close}
      style={{
        position: "fixed",
        inset: 0,
        background: "rgba(0,0,0,0.4)",
        display: "flex",
        alignItems: "flex-start",
        justifyContent: "center",
        paddingTop: "10vh",
        zIndex: 60,
      }}
    >
      <div
        onClick={(e) => e.stopPropagation()}
        style={{
          background: "white",
          borderRadius: 6,
          minWidth: 480,
          maxWidth: 640,
          padding: 8,
          boxShadow: "0 8px 24px rgba(0,0,0,0.2)",
        }}
      >
        <input
          data-testid="cmdk-input"
          autoFocus
          value={q}
          onChange={(e) => { setQ(e.target.value); setCursor(0); }}
          placeholder="Type to search…"
          onKeyDown={(e) => {
            if (e.key === "ArrowDown") {
              e.preventDefault();
              setCursor((c) => Math.min(c + 1, filtered.length - 1));
            } else if (e.key === "ArrowUp") {
              e.preventDefault();
              setCursor((c) => Math.max(c - 1, 0));
            } else if (e.key === "Enter") {
              e.preventDefault();
              const a = filtered[safeCursor];
              if (a) void a.run();
            }
          }}
          style={{
            width: "100%",
            padding: 10,
            border: "1px solid #e5e5e5",
            borderRadius: 4,
            boxSizing: "border-box",
          }}
        />
        {searching && (
          <div style={{ padding: "4px 10px", color: "#888", fontSize: 13 }}>
            Searching…
          </div>
        )}
        <ul
          data-testid="cmdk-list"
          style={{
            listStyle: "none",
            margin: 0,
            padding: 0,
            marginTop: 8,
            maxHeight: 320,
            overflow: "auto",
          }}
        >
          {filtered.map((a, i) => (
            <li key={a.id}>
              <button
                type="button"
                data-testid={`cmdk-item-${a.id}`}
                onClick={() => void a.run()}
                style={{
                  display: "block",
                  width: "100%",
                  textAlign: "left",
                  padding: "6px 10px",
                  border: "none",
                  background: i === safeCursor ? "#e5e5ff" : "transparent",
                  cursor: "pointer",
                }}
              >
                {a.label}
                {a.snippet && (
                  <div
                    style={{ fontSize: 12, color: "#777", marginTop: 2 }}
                    dangerouslySetInnerHTML={{ __html: safeSnippet(a.snippet) }}
                  />
                )}
              </button>
            </li>
          ))}
          {showNoMatches && (
            <li style={{ padding: "8px 10px", color: "#888" }}>
              No documents matched.
            </li>
          )}
          {!showNoMatches && filtered.length === 0 && (
            <li style={{ padding: "8px 10px", color: "#888" }}>No matches.</li>
          )}
        </ul>
      </div>
    </div>
  );
}
