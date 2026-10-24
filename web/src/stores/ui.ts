import { create } from "zustand";

import type { LatLon } from "../lib/sun";
import {
  SKIN_KEYS,
  currentSkin,
  hasStoredSlot,
  readManualSkin,
  readSkinPrefs,
  writePref,
  type SkinMode,
  type SkinPrefs,
} from "../styles/skinPrefs";
import { DEFAULT_SKIN_ID, findSkin, isSkinOfScheme, schemeOf } from "../styles/skins";

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
  // Skin — the palette + typography set. `skin` is the one showing and
  // `theme` its light/dark scheme, kept in the store so consumers can
  // branch on it cheaply. `skinMode` says which skin shows: `manualSkin`,
  // picked by hand, or in an automatic mode `lightSkin` or `darkSkin`, by
  // the system's appearance or by the sun at `sunLocation` (the time
  // zone's when null).
  skin: string;
  theme: Theme;
  skinMode: SkinMode;
  manualSkin: string;
  lightSkin: string;
  darkSkin: string;
  sunLocation: LatLon | null;
  /** Picks a skin by hand, which also switches to manual mode. */
  setSkin: (id: string) => void;
  setSkinMode: (mode: SkinMode) => void;
  // Each slot ignores a skin of the other scheme.
  setLightSkin: (id: string) => void;
  setDarkSkin: (id: string) => void;
  /** A typed location, or null to go back to the time zone's. */
  setSunLocation: (loc: LatLon | null) => void;
  /** Re-resolves the skin showing at `now`, for the watcher that follows
   *  the system and the sun. Writes no storage, and touches neither the
   *  store nor `<html>` when the skin showing stays the same. */
  refreshSkin: (now?: Date) => void;
  /** Takes the preferences as stored, e.g. after another tab changed them,
   *  without writing any back. */
  adoptStoredSkinPrefs: () => void;
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

/** The skin picked by hand. Exported for tests. */
export const readInitialSkin = readManualSkin;

/** The skin to show, resolved from the stored preferences. Exported for
 *  the pre-paint stamp in main.tsx. */
export function resolveInitialSkin(): string {
  return currentSkin(readSkinPrefs());
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

type SkinPrefsState = Pick<UiState, "skinMode" | "manualSkin" | "lightSkin" | "darkSkin" | "sunLocation">;

/** The store field for each automatic mode's slot. */
const SLOT_FIELD = { light: "lightSkin", dark: "darkSkin" } as const;

function prefsOf(s: SkinPrefsState): SkinPrefs {
  return { mode: s.skinMode, manual: s.manualSkin, light: s.lightSkin, dark: s.darkSkin, location: s.sunLocation };
}

function stateOf(p: SkinPrefs): SkinPrefsState {
  return { skinMode: p.mode, manualSkin: p.manual, lightSkin: p.light, darkSkin: p.dark, sunLocation: p.location };
}

const round2 = (n: number) => Math.round(n * 100) / 100;

/** A preference change made in this tab, plus the skin it shows. That skin
 *  is stamped even when it is the one already showing, as picking a skin
 *  always has been; only refreshSkin, which runs on every re-check, skips
 *  an unchanged one. The setters persist the change themselves. */
function withSkinShowing(s: UiState, change: Partial<SkinPrefsState>): Partial<UiState> {
  const skin = currentSkin(prefsOf({ ...s, ...change }));
  stampSkin(skin);
  return { ...change, skin, theme: schemeOf(skin) };
}

// One read of storage for the whole initial skin state.
const initialSkinPrefs = readSkinPrefs();
const initialSkin = currentSkin(initialSkinPrefs);

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
  skin: initialSkin,
  theme: schemeOf(initialSkin),
  ...stateOf(initialSkinPrefs),
  setSkin: (id) => {
    const manualSkin = findSkin(id) ? id : DEFAULT_SKIN_ID;
    writePref(SKIN_KEYS.manual, manualSkin);
    writePref(SKIN_KEYS.mode, "manual");
    set(withSkinShowing(get(), { manualSkin, skinMode: "manual" }));
  },
  setSkinMode: (mode) => {
    const s = get();
    if (mode === s.skinMode) return;
    const change: Partial<SkinPrefsState> = { skinMode: mode };
    if (mode === "manual") {
      // The skin showing becomes the one picked by hand, so nothing jumps.
      change.manualSkin = s.skin;
      writePref(SKIN_KEYS.manual, s.skin);
    } else {
      // The first time into an automatic mode, the skin showing fills its
      // scheme's slot: a Nord user who follows the sun gets Nord at night.
      const scheme = schemeOf(s.skin);
      if (!hasStoredSlot(scheme)) {
        change[SLOT_FIELD[scheme]] = s.skin;
        writePref(SKIN_KEYS[scheme], s.skin);
      }
    }
    writePref(SKIN_KEYS.mode, mode);
    set(withSkinShowing(s, change));
  },
  setLightSkin: (id) => {
    if (!isSkinOfScheme(id, "light")) return;
    writePref(SKIN_KEYS.light, id);
    set(withSkinShowing(get(), { lightSkin: id }));
  },
  setDarkSkin: (id) => {
    if (!isSkinOfScheme(id, "dark")) return;
    writePref(SKIN_KEYS.dark, id);
    set(withSkinShowing(get(), { darkSkin: id }));
  },
  setSunLocation: (loc) => {
    // Two decimals is about a kilometre, closer than sunrise needs.
    const sunLocation = loc && { lat: round2(loc.lat), lon: round2(loc.lon) };
    writePref(SKIN_KEYS.location, sunLocation && JSON.stringify(sunLocation));
    set(withSkinShowing(get(), { sunLocation }));
  },
  refreshSkin: (now) => {
    const skin = currentSkin(prefsOf(get()), now);
    if (skin === get().skin) return;
    stampSkin(skin);
    set({ skin, theme: schemeOf(skin) });
  },
  adoptStoredSkinPrefs: () => {
    set(stateOf(readSkinPrefs()));
    get().refreshSkin();
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
