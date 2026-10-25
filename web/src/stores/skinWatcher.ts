/** Keeps the skin showing current while the app runs: as the system's
 *  appearance changes, as the sun rises and sets, and as another tab
 *  changes the preferences. Started once from main.tsx rather than as a
 *  React hook, so it also runs on the pages outside the app shell. */

import { DARK_SCHEME_QUERY, SKIN_KEYS, type SkinMode } from "../styles/skinPrefs";

import { useUi } from "./ui";

/** How often sun mode re-checks the clock. Polling rather than a timer set
 *  for the next sunrise or sunset: timers don't survive sleep reliably, and
 *  the check costs microseconds. */
export const SUN_RECHECK_MS = 60_000;

const skinKeys: readonly string[] = Object.values(SKIN_KEYS);

/** Starts watching. The cleanup it returns removes every listener, the
 *  interval and the store subscription. */
export function watchSkinEnvironment(): () => void {
  // No arguments: refreshSkin would take an event for the time to resolve at.
  const refresh = () => useUi.getState().refreshSkin();

  // What system mode follows. Without matchMedia (jsdom, for one) the
  // system counts as light, and never changes.
  const systemScheme = typeof window.matchMedia === "function" ? window.matchMedia(DARK_SCHEME_QUERY) : null;
  systemScheme?.addEventListener("change", refresh);

  // The sun re-check runs only in sun mode, so the interval starts and
  // stops as the mode changes.
  let timer: number | undefined;
  const syncTimer = (mode: SkinMode) => {
    if (mode === "sun" && timer === undefined) {
      timer = window.setInterval(refresh, SUN_RECHECK_MS);
    } else if (mode !== "sun" && timer !== undefined) {
      window.clearInterval(timer);
      timer = undefined;
    }
  };
  syncTimer(useUi.getState().skinMode);
  const unsubscribe = useUi.subscribe((s) => syncTimer(s.skinMode));

  // A sleeping laptop runs no timers, so the page coming back re-checks
  // too: shown again, focused, or restored from the back-forward cache.
  // These listen in every mode; refreshSkin does nothing when the skin
  // stays the same.
  const onVisibilityChange = () => {
    if (document.visibilityState === "visible") refresh();
  };
  document.addEventListener("visibilitychange", onVisibilityChange);
  window.addEventListener("focus", refresh);
  window.addEventListener("pageshow", refresh);

  // Mirror the skin preferences across tabs; a null key is another tab
  // clearing storage. Adopt only, never write back: the same no-echo rule
  // AppShell follows for the width preferences.
  const onStorage = (e: StorageEvent) => {
    if (e.key === null || skinKeys.includes(e.key)) useUi.getState().adoptStoredSkinPrefs();
  };
  window.addEventListener("storage", onStorage);

  return () => {
    systemScheme?.removeEventListener("change", refresh);
    unsubscribe();
    window.clearInterval(timer);
    document.removeEventListener("visibilitychange", onVisibilityChange);
    window.removeEventListener("focus", refresh);
    window.removeEventListener("pageshow", refresh);
    window.removeEventListener("storage", onStorage);
  };
}
