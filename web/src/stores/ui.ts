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

/** Left sidebar width in whole CSS pixels. The default is the width it had
 *  before it could be resized. */
export const SIDEBAR_WIDTH_MIN = 200;
export const SIDEBAR_WIDTH_MAX = 480;
export const SIDEBAR_WIDTH_DEFAULT = 260;
/** The sidebar never squeezes the content column below this. 380 is the
 *  narrowest the fixed 260px sidebar ever left it (640 - 260, at the
 *  tablet edge), so dragging the sidebar wider can never go below that. */
export const CONTENT_WIDTH_MIN = 380;

/** What the sidebar may span in a window this wide. The minimum wins when
 *  the window is too narrow for both — below 640px it is a drawer anyway. */
export function sidebarWidthBounds(viewportWidth: number): { min: number; max: number } {
  const max = Math.min(SIDEBAR_WIDTH_MAX, viewportWidth - CONTENT_WIDTH_MIN);
  return { min: SIDEBAR_WIDTH_MIN, max: Math.max(SIDEBAR_WIDTH_MIN, max) };
}

/** The width the sidebar renders at: the saved preference, clamped to what
 *  this window allows. The preference itself is left alone, so narrowing
 *  the window and widening it again brings the chosen width back. */
export function effectiveSidebarWidth(pref: number, viewportWidth: number): number {
  const { min, max } = sidebarWidthBounds(viewportWidth);
  return Math.min(max, Math.max(min, pref));
}

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
  // Left sidebar width — the saved preference. What actually renders is
  // effectiveSidebarWidth(sidebarWidth, window width).
  sidebarWidth: number;
  setSidebarWidth: (w: number) => void;
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

/** Whole pixels inside the absolute range — what is worth saving, whatever
 *  the current window allows. */
function clampSidebarWidth(w: number): number {
  return Math.min(SIDEBAR_WIDTH_MAX, Math.max(SIDEBAR_WIDTH_MIN, Math.round(w)));
}

/** Exported for AppShell, which parses the raw string itself: re-read from
 *  storage on mount, and from another tab's storage event. Anything that is
 *  not a plain number — missing, empty, "340px", "Infinity" — is the
 *  default; a number out of range is clamped into it rather than thrown
 *  away. */
export function parseSidebarWidth(raw: string | null): number {
  if (raw === null || raw.trim() === "") return SIDEBAR_WIDTH_DEFAULT;
  const n = Number(raw);
  return Number.isFinite(n) ? clampSidebarWidth(n) : SIDEBAR_WIDTH_DEFAULT;
}

/** Exported for tests and for the pre-paint stamp in main.tsx. */
export function readInitialSidebarWidth(): number {
  try {
    return parseSidebarWidth(localStorage.getItem("knot.sidebarWidth"));
  } catch {
    return SIDEBAR_WIDTH_DEFAULT;
  }
}

/** Writes the width the sidebar renders at to --knot-sidebar-w on <html>,
 *  where AppShell's grid and layout.css's comment-rail gate both read it.
 *  Exported for main.tsx (before first paint), AppShell (on mount, window
 *  resizes and other tabs) and the resize handle, which stamps once, when a
 *  drag ends: every element inherits the variable, so it is no per-frame
 *  tool. */
export function stampSidebarWidth(pref: number) {
  if (typeof document === "undefined") return;
  const px = effectiveSidebarWidth(pref, window.innerWidth);
  document.documentElement.style.setProperty("--knot-sidebar-w", `${px}px`);
}

function applySidebarWidth(w: number) {
  stampSidebarWidth(w);
  try {
    localStorage.setItem("knot.sidebarWidth", String(w));
  } catch {
    /* storage unavailable — the width still applies for this session */
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
  sidebarWidth: readInitialSidebarWidth(),
  setSidebarWidth: (w) => {
    const next = clampSidebarWidth(w);
    applySidebarWidth(next);
    set({ sidebarWidth: next });
  },
}));
