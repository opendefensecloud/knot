import { create } from "zustand";

import { DEFAULT_SKIN_ID, findSkin, schemeOf } from "../styles/skins";

export type Toast = {
  id: number;
  kind: "info" | "warn" | "error";
  text: string;
};

export type PendingAnchor = {
  positionY: string;
  positionYEnd: string;
  anchorText: string;
};

/** The light/dark axis. Derived from the active skin; never set directly. */
export type Theme = "light" | "dark";

/** Document layout mode. "fixed" is the classic narrow column. */
export type DocWidth = "fixed" | "wide";

type UiState = {
  sidebarOpen: boolean;
  toggleSidebar: () => void;
  toasts: Toast[];
  notify: (kind: Toast["kind"], text: string) => void;
  dismiss: (id: number) => void;
  paletteOpen: boolean;
  openPalette: () => void;
  closePalette: () => void;
  togglePalette: () => void;
  // Comment sidebar
  commentSidebarOpen: boolean;
  openCommentSidebar: () => void;
  closeCommentSidebar: () => void;
  pendingAnchor: PendingAnchor | null;
  setPendingAnchor: (a: PendingAnchor) => void;
  clearPendingAnchor: () => void;
  // Active comment thread — drives both the in-editor highlight emphasis
  // and the sidebar scroll-into-view + focus ring.
  activeCommentId: string | null;
  setActiveCommentId: (id: string | null) => void;
  // Skin — the palette + typography set. `theme` is its light/dark
  // scheme, kept in the store so consumers can branch on it cheaply.
  skin: string;
  theme: Theme;
  setSkin: (id: string) => void;
  // Document width — a global reading preference, not a document property.
  docWidth: DocWidth;
  setDocWidth: (w: DocWidth) => void;
  toggleDocWidth: () => void;
};

let nextId = 1;

/** Exported for tests and for the pre-paint stamp in main.tsx. Falls
 *  back to the default skin for anything unknown, and honours the old
 *  `knot.theme = "dark"` preference from before skins existed. */
export function readInitialSkin(): string {
  try {
    const stored = localStorage.getItem("knot.skin");
    if (findSkin(stored)) return stored!;
    if (localStorage.getItem("knot.theme") === "dark") return "dark";
  } catch {
    /* storage unavailable */
  }
  return DEFAULT_SKIN_ID;
}

/** Exported for tests: the storage read has to survive a disabled or
 *  throwing Storage, which `readInitialTheme` above does not. */
export function readInitialDocWidth(): DocWidth {
  try {
    return localStorage.getItem("knot.docWidth") === "wide" ? "wide" : "fixed";
  } catch {
    return "fixed";
  }
}

function applyDocWidth(w: DocWidth) {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-doc-width", w);
  }
  try {
    localStorage.setItem("knot.docWidth", w);
  } catch {
    /* storage unavailable — the mode still applies for this session */
  }
}

/** Exported for main.tsx, which stamps the attributes before first paint
 *  so a dark-skin user never sees the light palette flash in. */
export function stampSkin(id: string) {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-skin", id);
    document.documentElement.setAttribute("data-theme", schemeOf(id));
  }
}

function applySkin(id: string) {
  stampSkin(id);
  try {
    localStorage.setItem("knot.skin", id);
  } catch {
    /* storage unavailable — the skin still applies for this session */
  }
}

export const useUi = create<UiState>((set, get) => ({
  sidebarOpen: true,
  toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
  toasts: [],
  notify: (kind, text) =>
    set((s) => ({ toasts: [...s.toasts, { id: nextId++, kind, text }] })),
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
  paletteOpen: false,
  openPalette: () => set({ paletteOpen: true }),
  closePalette: () => set({ paletteOpen: false }),
  togglePalette: () => set((s) => ({ paletteOpen: !s.paletteOpen })),
  commentSidebarOpen: false,
  openCommentSidebar: () => set({ commentSidebarOpen: true }),
  closeCommentSidebar: () => set({ commentSidebarOpen: false }),
  pendingAnchor: null,
  setPendingAnchor: (a) => set({ pendingAnchor: a }),
  clearPendingAnchor: () => set({ pendingAnchor: null }),
  activeCommentId: null,
  setActiveCommentId: (id) => set({ activeCommentId: id }),
  skin: readInitialSkin(),
  theme: schemeOf(readInitialSkin()),
  setSkin: (id) => {
    const skinId = findSkin(id) ? id : DEFAULT_SKIN_ID;
    applySkin(skinId);
    set({ skin: skinId, theme: schemeOf(skinId) });
  },
  docWidth: readInitialDocWidth(),
  setDocWidth: (w) => { applyDocWidth(w); set({ docWidth: w }); },
  toggleDocWidth: () => {
    const next: DocWidth = get().docWidth === "fixed" ? "wide" : "fixed";
    applyDocWidth(next);
    set({ docWidth: next });
  },
}));
