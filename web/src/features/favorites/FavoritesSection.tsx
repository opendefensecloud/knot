import { ChevronRight, FileText, Star } from "lucide-react";
import { useMemo, useRef, useState } from "react";
import { Link, useLocation } from "react-router-dom";

import { IconButton } from "../../components/ui/IconButton";
import type { Doc } from "../../lib/validators";

import { useFavorites } from "./useFavorites";

/** `"1"` while the section is collapsed. */
const COLLAPSED_KEY = "knot.favoritesCollapsed";

function readCollapsed(): boolean {
  try {
    return localStorage.getItem(COLLAPSED_KEY) === "1";
  } catch {
    return false;
  }
}

function writeCollapsed(collapsed: boolean) {
  try {
    if (collapsed) localStorage.setItem(COLLAPSED_KEY, "1");
    else localStorage.removeItem(COLLAPSED_KEY);
  } catch {
    // Storage blocked: the section still collapses, just not across reloads.
  }
}

/** The id of the page open in the editor, from `/doc/{id}…`. */
function openDocId(pathname: string): string | undefined {
  const m = /^\/doc\/([^/]+)/.exec(pathname);
  if (!m) return undefined;
  try {
    return decodeURIComponent(m[1]!);
  } catch {
    return m[1];
  }
}

type Entry = { id: string; title: string; icon: string | null };

/** Where focus goes when the focused unstar button's row is about to go:
 *  the next favorite, else the one before it, else (the section goes too)
 *  the page's own row in the tree, else the tree's first row. */
function focusAfterUnstar(section: HTMLElement | null, entries: Entry[], id: string): HTMLElement | null {
  const i = entries.findIndex((e) => e.id === id);
  const neighbour = entries[i + 1] ?? entries[i - 1];
  const link = (sel: string) => section?.querySelector<HTMLElement>(sel) ?? null;
  if (neighbour) return link(`[data-testid="fav-link-${neighbour.id}"]`);
  const tree = section?.closest<HTMLElement>('[data-testid="doc-tree"]') ?? null;
  return (
    tree?.querySelector<HTMLElement>(`[data-testid="doc-row-${id}"]`) ??
    tree?.querySelector<HTMLElement>('[data-testid^="doc-row-"]') ??
    null
  );
}

/** Classes for a button shown only on hover or keyboard focus. Where there
 *  is no hover (touch), it shows always: an invisible button still takes
 *  taps. */
const REVEAL_ON_HOVER =
  "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100 focus-visible:opacity-100 [@media(hover:none)]:opacity-100";

/**
 * The user's favorite pages, above the sidebar's "Documents" header.
 *
 * Titles come from the tree's docs list, so a rename shows here as soon as
 * the tree has it; a favorite the list doesn't have (no longer readable),
 * or shows as archived or a template, is left out. Until the list has
 * loaded (`docs` undefined), the favorites API's own titles stand in.
 *
 * Rendered outside the tree's `DndContext`, with test ids of its own: its
 * rows are plain links, not tree rows.
 */
export function FavoritesSection({ docs }: { docs: Doc[] | undefined }) {
  const favs = useFavorites();
  const favorites = favs.favorites;
  const { pathname } = useLocation();
  const openId = openDocId(pathname);
  const [collapsed, setCollapsed] = useState(readCollapsed);
  const sectionRef = useRef<HTMLElement | null>(null);

  const entries = useMemo<Entry[]>(() => {
    if (!favorites) return [];
    if (!docs) return favorites.map((f) => ({ id: f.doc_id, title: f.title, icon: f.icon }));
    const byId = new Map(docs.map((d) => [d.id, d]));
    const out: Entry[] = [];
    for (const f of favorites) {
      const d = byId.get(f.doc_id);
      if (!d || d.archived || d.is_template) continue;
      out.push({ id: d.id, title: d.title, icon: d.icon });
    }
    return out;
  }, [favorites, docs]);

  if (entries.length === 0) return null;

  function onToggleCollapsed() {
    const next = !collapsed;
    setCollapsed(next);
    writeCollapsed(next);
  }

  return (
    <section ref={sectionRef} data-testid="favorites" aria-labelledby="favorites-heading">
      <div className="px-3 pt-3 pb-1 flex items-center justify-between">
        <h3 id="favorites-heading" className="m-0 text-[11px] font-semibold uppercase tracking-wider text-fg-muted">
          Favorites
        </h3>
        <IconButton
          data-testid="favorites-toggle"
          label={collapsed ? "Expand favorites" : "Collapse favorites"}
          size="sm"
          aria-expanded={!collapsed}
          aria-controls="favorites-list"
          onClick={onToggleCollapsed}
        >
          <ChevronRight
            size={14}
            aria-hidden
            className={`transition-transform duration-150 ${collapsed ? "" : "rotate-90"}`}
          />
        </IconButton>
      </div>
      {!collapsed && (
        <ul id="favorites-list" aria-labelledby="favorites-heading" className="list-none px-2 m-0">
          {entries.map((e) => {
            const open = e.id === openId;
            return (
              <li
                key={e.id}
                data-testid={`fav-row-${e.id}`}
                className={`group flex items-center gap-1 rounded h-7 pl-1 pr-1 transition-colors ease-swift duration-150 ${
                  open ? "bg-muted text-fg" : "text-fg-muted hover:text-fg hover:bg-muted/60"
                }`}
              >
                <FileText size={14} aria-hidden className="text-fg-muted shrink-0 ml-1" />
                <Link
                  data-testid={`fav-link-${e.id}`}
                  to={`/doc/${e.id}`}
                  aria-current={open ? "page" : undefined}
                  className="flex-1 min-w-0 truncate text-[13px] no-underline text-inherit py-1"
                >
                  {e.title}
                </Link>
                <IconButton
                  data-testid={`fav-unstar-${e.id}`}
                  label="Remove from favorites"
                  size="sm"
                  className={REVEAL_ON_HOVER}
                  onClick={(ev) => {
                    ev.preventDefault();
                    // The row goes at once: a keyboard user's focus would
                    // drop to <body> with it. Move it first.
                    if (document.activeElement === ev.currentTarget) {
                      focusAfterUnstar(sectionRef.current, entries, e.id)?.focus();
                    }
                    favs.toggle(e);
                  }}
                >
                  <Star size={14} aria-hidden fill="currentColor" className="text-accent" />
                </IconButton>
              </li>
            );
          })}
        </ul>
      )}
    </section>
  );
}
