import { create } from "zustand";

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
  // Theme
  theme: Theme;
  setTheme: (t: Theme) => void;
  toggleTheme: () => void;
  // Document width — a global reading preference, not a document property.
  docWidth: DocWidth;
  setDocWidth: (w: DocWidth) => void;
  toggleDocWidth: () => void;
};

let nextId = 1;

function readInitialTheme(): Theme {
  if (typeof localStorage === "undefined") return "light";
  return (localStorage.getItem("knot.theme") as Theme | null) ?? "light";
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

function applyTheme(t: Theme) {
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-theme", t);
  }
  if (typeof localStorage !== "undefined") {
    localStorage.setItem("knot.theme", t);
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
  theme: readInitialTheme(),
  setTheme: (t) => { applyTheme(t); set({ theme: t }); },
  toggleTheme: () => {
    const next: Theme = get().theme === "light" ? "dark" : "light";
    applyTheme(next);
    set({ theme: next });
  },
  docWidth: readInitialDocWidth(),
  setDocWidth: (w) => { applyDocWidth(w); set({ docWidth: w }); },
  toggleDocWidth: () => {
    const next: DocWidth = get().docWidth === "fixed" ? "wide" : "fixed";
    applyDocWidth(next);
    set({ docWidth: next });
  },
}));
