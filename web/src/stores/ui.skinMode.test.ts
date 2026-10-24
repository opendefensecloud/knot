import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi } from "./ui";

const berlin = { lat: 52.52, lon: 13.405 };
// Berlin's sun sets at 19:33 UTC on the June solstice.
const BERLIN_MIDDAY = new Date("2026-06-21T11:00:00Z");
const BERLIN_NIGHT = new Date("2026-06-21T21:00:00Z");

const ui = () => useUi.getState();
const html = document.documentElement;

/** jsdom has no matchMedia, which reads as a light system. */
function stubDarkSystem() {
  vi.stubGlobal("matchMedia", () => ({ matches: true }));
}

/** Pins the clock, so day and night don't depend on when the tests run. */
function pinClock(at: Date) {
  vi.useFakeTimers();
  vi.setSystemTime(at);
}

beforeEach(() => {
  localStorage.clear();
  // Drops the slots, mode and location earlier tests left in the store.
  ui().adoptStoredSkinPrefs();
  ui().setSkin("light");
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("initial state", () => {
  // Drops the fresh copies, so nothing after this imports one by accident.
  afterEach(() => {
    vi.resetModules();
  });

  it("starts from the stored preferences and the skin they resolve to", async () => {
    localStorage.setItem("knot.skinMode", "system");
    localStorage.setItem("knot.skinLight", "sage");
    localStorage.setItem("knot.skinDark", "gruvbox");
    localStorage.setItem("knot.sunLocation", '{"lat":52.52,"lon":13.4}');
    stubDarkSystem();
    vi.resetModules();
    const fresh = (await import("./ui")).useUi.getState();
    expect(fresh).toMatchObject({
      skinMode: "system",
      manualSkin: "light",
      lightSkin: "sage",
      darkSkin: "gruvbox",
      sunLocation: { lat: 52.52, lon: 13.4 },
      skin: "gruvbox",
      theme: "dark",
    });
  });

  it("resolves the skin at load, so sun mode starts dark after sunset", async () => {
    // main.tsx stamps this skin before first paint.
    localStorage.setItem("knot.skinMode", "sun");
    localStorage.setItem("knot.skinLight", "paper");
    localStorage.setItem("knot.skinDark", "nord");
    localStorage.setItem("knot.sunLocation", JSON.stringify(berlin));
    pinClock(BERLIN_NIGHT);
    vi.resetModules();
    const fresh = (await import("./ui")).useUi.getState();
    expect(fresh.skin).toBe("nord");
    expect(fresh.theme).toBe("dark");
  });
});

describe("setSkin", () => {
  it("picks a skin by hand, whatever mode was on", () => {
    ui().setSkinMode("system");
    ui().setSkin("nord");
    expect(ui().skinMode).toBe("manual");
    expect(ui().manualSkin).toBe("nord");
    expect(ui().skin).toBe("nord");
    expect(localStorage.getItem("knot.skin")).toBe("nord");
    expect(localStorage.getItem("knot.skinMode")).toBe("manual");
  });
});

describe("setSkinMode", () => {
  it("seeds the slot of the skin showing the first time into an automatic mode", () => {
    ui().setSkin("nord");
    ui().setSkinMode("sun");
    expect(ui().skinMode).toBe("sun");
    expect(localStorage.getItem("knot.skinMode")).toBe("sun");
    expect(ui().darkSkin).toBe("nord");
    expect(localStorage.getItem("knot.skinDark")).toBe("nord");
    expect(ui().lightSkin).toBe("light");
    expect(localStorage.getItem("knot.skinLight")).toBeNull();
  });

  it("leaves a slot alone that already holds a pick", () => {
    localStorage.setItem("knot.skinDark", "dracula");
    ui().adoptStoredSkinPrefs();
    ui().setSkin("nord");
    ui().setSkinMode("system");
    expect(ui().darkSkin).toBe("dracula");
    expect(localStorage.getItem("knot.skinDark")).toBe("dracula");
  });

  it("keeps the skin showing when going back to picking by hand", () => {
    ui().setSkinMode("system");
    ui().setLightSkin("paper");
    expect(ui().skin).toBe("paper");
    ui().setSkinMode("manual");
    expect(ui().manualSkin).toBe("paper");
    expect(ui().skin).toBe("paper");
    expect(localStorage.getItem("knot.skin")).toBe("paper");
    expect(localStorage.getItem("knot.skinMode")).toBe("manual");
  });

  it("does nothing when the mode is unchanged", () => {
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    ui().setSkinMode("manual");
    expect(setItem).not.toHaveBeenCalled();
  });

  it("still switches when storage rejects the write", () => {
    ui().setSkin("nord");
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    ui().setSkinMode("system");
    expect(ui().skinMode).toBe("system");
    expect(ui().darkSkin).toBe("nord");
    expect(ui().skin).toBe("light"); // a light system shows the light slot
    expect(html).toHaveAttribute("data-skin", "light");
  });
});

describe("setLightSkin / setDarkSkin", () => {
  it("take only a skin of the slot's scheme", () => {
    ui().setLightSkin("nord");
    ui().setDarkSkin("paper");
    expect(ui().lightSkin).toBe("light");
    expect(ui().darkSkin).toBe("dark");
    expect(localStorage.getItem("knot.skinLight")).toBeNull();
    expect(localStorage.getItem("knot.skinDark")).toBeNull();
  });

  it("store the slot and show it while it applies", () => {
    ui().setSkinMode("system");
    ui().setLightSkin("sage");
    expect(ui().lightSkin).toBe("sage");
    expect(localStorage.getItem("knot.skinLight")).toBe("sage");
    expect(ui().skin).toBe("sage");
    expect(html).toHaveAttribute("data-skin", "sage");

    stubDarkSystem();
    ui().setDarkSkin("gruvbox");
    expect(localStorage.getItem("knot.skinDark")).toBe("gruvbox");
    expect(ui().skin).toBe("gruvbox");
    expect(html).toHaveAttribute("data-theme", "dark");
  });

  it("leave the hand-picked skin showing in manual mode", () => {
    ui().setDarkSkin("nord");
    expect(ui().darkSkin).toBe("nord");
    expect(ui().skin).toBe("light");
  });
});

describe("setSunLocation", () => {
  it("stores a typed location to two decimals, and removes it for null", () => {
    ui().setSunLocation({ lat: 52.51963, lon: 13.40495 });
    expect(ui().sunLocation).toEqual({ lat: 52.52, lon: 13.4 });
    expect(localStorage.getItem("knot.sunLocation")).toBe('{"lat":52.52,"lon":13.4}');
    ui().setSunLocation(null);
    expect(ui().sunLocation).toBeNull();
    expect(localStorage.getItem("knot.sunLocation")).toBeNull();
  });

  it("ignores a location storage wouldn't read back", () => {
    ui().setSunLocation(berlin);
    const before = ui().sunLocation;
    const stored = localStorage.getItem("knot.sunLocation");
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    const removeItem = vi.spyOn(Storage.prototype, "removeItem");
    for (const loc of [{ lat: NaN, lon: 0 }, { lat: 91, lon: 0 }]) {
      ui().setSunLocation(loc);
      expect(ui().sunLocation, JSON.stringify(loc)).toBe(before);
    }
    expect(localStorage.getItem("knot.sunLocation")).toBe(stored);
    expect(setItem).not.toHaveBeenCalled();
    expect(removeItem).not.toHaveBeenCalled();
  });

  it("shows the skin for the sun at the new location", () => {
    pinClock(BERLIN_NIGHT); // early morning in Tokyo
    ui().setSunLocation(berlin);
    ui().setSkinMode("sun");
    expect(ui().skin).toBe("dark");
    ui().setSunLocation({ lat: 35.68, lon: 139.69 });
    expect(ui().skin).toBe("light");
  });
});

describe("refreshSkin", () => {
  it("follows the sun in sun mode without writing storage", () => {
    pinClock(BERLIN_MIDDAY);
    ui().setSunLocation(berlin);
    ui().setDarkSkin("nord");
    ui().setSkinMode("sun");
    expect(ui().skin).toBe("light");

    const setItem = vi.spyOn(Storage.prototype, "setItem");
    vi.setSystemTime(BERLIN_NIGHT);
    ui().refreshSkin();
    expect(ui().skin).toBe("nord");
    expect(ui().theme).toBe("dark");
    expect(html).toHaveAttribute("data-skin", "nord");
    expect(html).toHaveAttribute("data-theme", "dark");

    ui().refreshSkin(new Date("2026-06-22T11:00:00Z"));
    expect(ui().skin).toBe("light");
    expect(setItem).not.toHaveBeenCalled();
  });

  it("leaves <html> and the store alone when the skin showing is unchanged", () => {
    html.setAttribute("data-skin", "sentinel");
    const listener = vi.fn();
    const unsubscribe = useUi.subscribe(listener);
    ui().refreshSkin();
    unsubscribe();
    expect(html).toHaveAttribute("data-skin", "sentinel");
    expect(listener).not.toHaveBeenCalled();
  });
});

describe("adoptStoredSkinPrefs", () => {
  it("takes the stored preferences without writing any back", () => {
    localStorage.setItem("knot.skinMode", "system");
    localStorage.setItem("knot.skinDark", "nord");
    stubDarkSystem();
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    const removeItem = vi.spyOn(Storage.prototype, "removeItem");
    ui().adoptStoredSkinPrefs();
    expect(ui().skinMode).toBe("system");
    expect(ui().darkSkin).toBe("nord");
    expect(ui().skin).toBe("nord");
    expect(html).toHaveAttribute("data-skin", "nord");
    expect(setItem).not.toHaveBeenCalled();
    expect(removeItem).not.toHaveBeenCalled();
  });
});
