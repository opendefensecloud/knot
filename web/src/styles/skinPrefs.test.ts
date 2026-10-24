import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  SKIN_KEYS,
  currentSkin,
  hasStoredSlot,
  parseSkinMode,
  parseSlot,
  parseSunLocation,
  readSkinPrefs,
  resolveSkin,
  systemPrefersDark,
  writePref,
  type SkinPrefs,
} from "./skinPrefs";

/** Slots that differ from the defaults, so a test can tell which one won. */
const prefs = (over: Partial<SkinPrefs> = {}): SkinPrefs => ({
  mode: "manual",
  manual: "gruvbox",
  light: "paper",
  dark: "nord",
  location: null,
  ...over,
});

const berlin = { lat: 52.52, lon: 13.405 };

beforeEach(() => {
  localStorage.clear();
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("resolveSkin", () => {
  it("shows the hand-picked skin in manual mode, whatever the environment", () => {
    for (const systemDark of [false, true]) {
      for (const daytime of [false, true]) {
        expect(resolveSkin(prefs(), { systemDark, daytime })).toBe("gruvbox");
      }
    }
  });

  it("follows the system in system mode", () => {
    const p = prefs({ mode: "system" });
    expect(resolveSkin(p, { systemDark: true, daytime: true })).toBe("nord");
    expect(resolveSkin(p, { systemDark: false, daytime: false })).toBe("paper");
  });

  it("follows the sun in sun mode", () => {
    const p = prefs({ mode: "sun" });
    expect(resolveSkin(p, { systemDark: true, daytime: true })).toBe("paper");
    expect(resolveSkin(p, { systemDark: false, daytime: false })).toBe("nord");
  });
});

describe("parseSkinMode", () => {
  it("passes the three modes through", () => {
    expect(parseSkinMode("system")).toBe("system");
    expect(parseSkinMode("sun")).toBe("sun");
    expect(parseSkinMode("manual")).toBe("manual");
  });

  it("treats anything else as manual", () => {
    expect(parseSkinMode(null)).toBe("manual");
    expect(parseSkinMode("")).toBe("manual");
    expect(parseSkinMode("auto")).toBe("manual");
  });
});

describe("parseSlot", () => {
  it("keeps a skin of the slot's scheme", () => {
    expect(parseSlot("paper", "light")).toBe("paper");
  });

  it("falls back to the default for the wrong scheme, a missing key or an unknown id", () => {
    expect(parseSlot("nord", "light")).toBe("light");
    expect(parseSlot("paper", "dark")).toBe("dark");
    expect(parseSlot(null, "dark")).toBe("dark");
    expect(parseSlot("nope", "light")).toBe("light");
  });
});

describe("parseSunLocation", () => {
  it("reads a typed location", () => {
    expect(parseSunLocation('{"lat":52.52,"lon":13.4}')).toEqual({ lat: 52.52, lon: 13.4 });
  });

  it("rejects anything but an object of finite, in-range numbers", () => {
    for (const raw of [
      null,
      "x",
      "null",
      '{"lat":"1","lon":2}',
      '{"lat":91,"lon":0}',
      '{"lat":0,"lon":-181}',
      '{"lat":1}',
      '{"lat":1e999,"lon":0}',
    ]) {
      expect(parseSunLocation(raw), String(raw)).toBeNull();
    }
  });
});

describe("readSkinPrefs", () => {
  it("gives the defaults for empty storage", () => {
    expect(readSkinPrefs()).toEqual({ mode: "manual", manual: "light", light: "light", dark: "dark", location: null });
  });

  it("reads every stored key back", () => {
    localStorage.setItem(SKIN_KEYS.manual, "dracula");
    localStorage.setItem(SKIN_KEYS.mode, "sun");
    localStorage.setItem(SKIN_KEYS.light, "sage");
    localStorage.setItem(SKIN_KEYS.dark, "everforest");
    localStorage.setItem(SKIN_KEYS.location, '{"lat":69.65,"lon":18.96}');
    expect(readSkinPrefs()).toEqual({
      mode: "sun",
      manual: "dracula",
      light: "sage",
      dark: "everforest",
      location: { lat: 69.65, lon: 18.96 },
    });
  });

  it("gives the defaults when storage is unavailable", () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("SecurityError: storage is disabled");
    });
    expect(readSkinPrefs()).toEqual({ mode: "manual", manual: "light", light: "light", dark: "dark", location: null });
  });
});

describe("hasStoredSlot", () => {
  it("is true only for a stored skin of the slot's scheme", () => {
    expect(hasStoredSlot("light")).toBe(false);
    localStorage.setItem(SKIN_KEYS.light, "nord");
    expect(hasStoredSlot("light")).toBe(false);
    localStorage.setItem(SKIN_KEYS.light, "paper");
    expect(hasStoredSlot("light")).toBe(true);
  });
});

describe("systemPrefersDark", () => {
  it("treats the system as light without matchMedia", () => {
    vi.stubGlobal("matchMedia", undefined);
    expect(systemPrefersDark()).toBe(false);
  });

  it("asks matchMedia for the dark scheme", () => {
    const matchMedia = vi.fn(() => ({ matches: true }));
    vi.stubGlobal("matchMedia", matchMedia);
    expect(systemPrefersDark()).toBe(true);
    expect(matchMedia).toHaveBeenCalledWith("(prefers-color-scheme: dark)");
  });
});

describe("currentSkin", () => {
  it("shows the light slot by day and the dark slot by night in sun mode", () => {
    const p = prefs({ mode: "sun", location: berlin });
    expect(currentSkin(p, new Date("2026-06-21T11:00:00Z"))).toBe("paper");
    expect(currentSkin(p, new Date("2026-06-21T21:00:00Z"))).toBe("nord");
  });

  it("asks the system in system mode", () => {
    vi.stubGlobal("matchMedia", () => ({ matches: true }));
    expect(currentSkin(prefs({ mode: "system" }))).toBe("nord");
  });
});

describe("writePref", () => {
  it("stores a value and removes the key for null", () => {
    writePref(SKIN_KEYS.mode, "sun");
    expect(localStorage.getItem(SKIN_KEYS.mode)).toBe("sun");
    writePref(SKIN_KEYS.mode, null);
    expect(localStorage.getItem(SKIN_KEYS.mode)).toBeNull();
  });

  it("swallows a storage that rejects the write", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("QuotaExceededError");
    });
    expect(() => writePref(SKIN_KEYS.mode, "sun")).not.toThrow();
  });
});
