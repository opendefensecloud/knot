import { create } from "zustand";

export type Toast = {
  id: number;
  kind: "info" | "warn" | "error";
  text: string;
};

export type PendingAnchor = {
  positionY: string;
  anchorText: string;
};

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
};

let nextId = 1;

export const useUi = create<UiState>((set) => ({
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
}));
