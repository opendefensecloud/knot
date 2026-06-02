import { create } from "zustand";

export type Toast = {
  id: number;
  kind: "info" | "warn" | "error";
  text: string;
};

type UiState = {
  sidebarOpen: boolean;
  toggleSidebar: () => void;
  toasts: Toast[];
  notify: (kind: Toast["kind"], text: string) => void;
  dismiss: (id: number) => void;
};

let nextId = 1;

export const useUi = create<UiState>((set) => ({
  sidebarOpen: true,
  toggleSidebar: () => set((s) => ({ sidebarOpen: !s.sidebarOpen })),
  toasts: [],
  notify: (kind, text) =>
    set((s) => ({ toasts: [...s.toasts, { id: nextId++, kind, text }] })),
  dismiss: (id) => set((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) })),
}));
