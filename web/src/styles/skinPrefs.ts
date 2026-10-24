/** How the showing skin is chosen: picked by hand, or switched between a
 *  light and a dark pick by the system's appearance or by the sun. The
 *  preferences live in `localStorage`, like the other layout preferences,
 *  and every read and write here survives a missing or throwing storage.
 *  Nothing here touches the store or `<html>`. */

import { isDaytime, type LatLon } from "../lib/sun";
import { resolveSunLocation } from "../lib/sunLocation";
import { DEFAULT_DARK_SKIN_ID, DEFAULT_LIGHT_SKIN_ID, DEFAULT_SKIN_ID, findSkin, isSkinOfScheme, type Scheme } from "./skins";

export type SkinMode = "manual" | "system" | "sun";

/** `light` and `dark` are named after the scheme, so `SKIN_KEYS[scheme]` is
 *  that slot's key. */
export const SKIN_KEYS = {
  manual: "knot.skin",
  legacyTheme: "knot.theme",
  mode: "knot.skinMode",
  light: "knot.skinLight",
  dark: "knot.skinDark",
  location: "knot.sunLocation",
} as const;

/** `manual` is the skin picked by hand, `light` and `dark` the automatic
 *  modes' two picks, and `location` the one the user typed, if any. */
export type SkinPrefs = { mode: SkinMode; manual: string; light: string; dark: string; location: LatLon | null };

const DEFAULT_SLOT: Record<Scheme, string> = { light: DEFAULT_LIGHT_SKIN_ID, dark: DEFAULT_DARK_SKIN_ID };

const DARK_SCHEME_QUERY = "(prefers-color-scheme: dark)";

/** Anything but a known mode is manual, today's behaviour. */
export function parseSkinMode(raw: string | null): SkinMode {
  return raw === "system" || raw === "sun" ? raw : "manual";
}

/** A slot holding a skin of the other scheme counts as unset. */
export function parseSlot(raw: string | null, scheme: Scheme): string {
  return isSkinOfScheme(raw, scheme) ? raw! : DEFAULT_SLOT[scheme];
}

const within = (n: unknown, limit: number): n is number =>
  typeof n === "number" && Number.isFinite(n) && Math.abs(n) <= limit;

/** A stored `{"lat": number, "lon": number}` in decimal degrees, or `null`
 *  for anything else, so a corrupt value falls back to the time zone. */
export function parseSunLocation(raw: string | null): LatLon | null {
  if (raw === null) return null;
  let value: unknown;
  try {
    value = JSON.parse(raw);
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null) return null;
  const { lat, lon } = value as Record<string, unknown>;
  return within(lat, 90) && within(lon, 180) ? { lat, lon } : null;
}

function readPref(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null; // storage unavailable
  }
}

/** The skin picked by hand. Falls back to the default skin for anything
 *  unknown, and honours the old `knot.theme = "dark"` preference from
 *  before skins existed. */
export function readManualSkin(): string {
  try {
    const stored = localStorage.getItem(SKIN_KEYS.manual);
    if (findSkin(stored)) return stored!;
    if (localStorage.getItem(SKIN_KEYS.legacyTheme) === "dark") return "dark";
  } catch {
    /* storage unavailable */
  }
  return DEFAULT_SKIN_ID;
}

export function readSkinPrefs(): SkinPrefs {
  return {
    mode: parseSkinMode(readPref(SKIN_KEYS.mode)),
    manual: readManualSkin(),
    light: parseSlot(readPref(SKIN_KEYS.light), "light"),
    dark: parseSlot(readPref(SKIN_KEYS.dark), "dark"),
    location: parseSunLocation(readPref(SKIN_KEYS.location)),
  };
}

/** Whether the user has a pick of their own for this slot, as opposed to
 *  the default `readSkinPrefs` fills in. */
export function hasStoredSlot(scheme: Scheme): boolean {
  return isSkinOfScheme(readPref(SKIN_KEYS[scheme]), scheme);
}

/** Stores one preference; `null` removes it. */
export function writePref(key: string, value: string | null): void {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, value);
  } catch {
    /* storage unavailable — the preference still applies for this session */
  }
}

/** The skin to show, from the preferences and the environment. */
export function resolveSkin(prefs: SkinPrefs, env: { systemDark: boolean; daytime: boolean }): string {
  switch (prefs.mode) {
    case "manual":
      return prefs.manual;
    case "system":
      return env.systemDark ? prefs.dark : prefs.light;
    case "sun":
      return env.daytime ? prefs.light : prefs.dark;
  }
}

/** Whether the system is in dark mode. Without `matchMedia` (jsdom, for
 *  one) the system counts as light. */
export function systemPrefersDark(): boolean {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return false;
  return window.matchMedia(DARK_SCHEME_QUERY).matches;
}

/** The skin to show at `now`, asking the system and the sun. */
export function currentSkin(prefs: SkinPrefs, now: Date = new Date()): string {
  return resolveSkin(prefs, {
    systemDark: systemPrefersDark(),
    daytime: isDaytime(now, resolveSunLocation(prefs.location, now).loc),
  });
}
