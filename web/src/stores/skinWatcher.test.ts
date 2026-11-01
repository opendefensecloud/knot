import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SUN_RECHECK_MS, watchSkinEnvironment } from "./skinWatcher";
import { useUi } from "./ui";

const berlin = { lat: 52.52, lon: 13.405 };
// Berlin's sun sets at 19:33 UTC on the June solstice.
const BEFORE_SUNSET = new Date("2026-06-21T19:30:00Z");
const AFTER_SUNSET = new Date("2026-06-21T21:00:00Z");

const ui = () => useUi.getState();
const html = document.documentElement;

let stop: (() => void) | undefined;

/** Starts the watcher; afterEach stops it, so nothing leaks into the next
 *  test. */
function start() {
  stop = watchSkinEnvironment();
  return stop;
}

/** A system appearance the test can flip, firing the change listeners the
 *  way a browser does. jsdom has no matchMedia of its own. */
function stubSystem(initial: "light" | "dark") {
  let dark = initial === "dark";
  const listeners = new Set<EventListener>();
  const list = {
    get matches() {
      return dark;
    },
    addEventListener: (type: string, fn: EventListener) => {
      if (type === "change") listeners.add(fn);
    },
    removeEventListener: (type: string, fn: EventListener) => {
      if (type === "change") listeners.delete(fn);
    },
  };
  vi.stubGlobal("matchMedia", () => list);
  return {
    set(scheme: "light" | "dark") {
      dark = scheme === "dark";
      for (const fn of listeners) fn(new Event("change"));
    },
  };
}

/** Sun mode at Berlin with Paper by day and Nord by night. */
function followTheSunInBerlin(at: Date) {
  vi.setSystemTime(at);
  ui().setSunLocation(berlin);
  ui().setLightSkin("paper");
  ui().setDarkSkin("nord");
  ui().setSkinMode("sun");
}

function fireStorage(key: string | null, newValue: string | null = null) {
  window.dispatchEvent(new StorageEvent("storage", { key, newValue }));
}

beforeEach(() => {
  // Before the watcher starts, so its interval is a fake one to count. Only
  // the clock and intervals: jsdom queues a setTimeout for every storage
  // write, which would otherwise pile up in getTimerCount.
  vi.useFakeTimers({ toFake: ["Date", "setInterval", "clearInterval"] });
  localStorage.clear();
  // Drops the slots, mode and location earlier tests left in the store.
  ui().adoptStoredSkinPrefs();
  ui().setSkin("light");
});

afterEach(() => {
  stop?.();
  stop = undefined;
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("system mode", () => {
  it("follows the system's appearance as it changes", () => {
    const system = stubSystem("light");
    ui().setSkinMode("system");
    ui().setLightSkin("paper");
    ui().setDarkSkin("nord");
    start();
    expect(html).toHaveAttribute("data-skin", "paper");

    system.set("dark");
    expect(ui().skin).toBe("nord");
    expect(html).toHaveAttribute("data-skin", "nord");
  });

  it("starts without matchMedia", () => {
    expect(typeof window.matchMedia).toBe("undefined");
    expect(() => start()).not.toThrow();
  });
});

describe("sun mode", () => {
  it("re-checks the sun every SUN_RECHECK_MS", () => {
    followTheSunInBerlin(BEFORE_SUNSET);
    start();
    expect(ui().skin).toBe("paper");

    vi.advanceTimersByTime(5 * SUN_RECHECK_MS);
    expect(ui().skin).toBe("nord");
    expect(html).toHaveAttribute("data-skin", "nord");
  });

  it("re-checks on a timer only while following the sun", () => {
    start();
    expect(vi.getTimerCount()).toBe(0);
    ui().setSkinMode("sun");
    expect(vi.getTimerCount()).toBe(1);
    ui().setDarkSkin("nord"); // any other change keeps the one timer
    expect(vi.getTimerCount()).toBe(1);
    ui().setSkinMode("manual");
    expect(vi.getTimerCount()).toBe(0);
  });

  it.each([
    ["focus", () => window.dispatchEvent(new Event("focus"))],
    ["visibilitychange", () => document.dispatchEvent(new Event("visibilitychange"))],
    ["pageshow", () => window.dispatchEvent(new Event("pageshow"))],
  ])("re-checks on %s, as when a laptop wakes after sunset", (_, fire) => {
    followTheSunInBerlin(BEFORE_SUNSET);
    start();
    vi.setSystemTime(AFTER_SUNSET); // asleep: no timer ran
    expect(ui().skin).toBe("paper");

    fire();
    expect(ui().skin).toBe("nord");
  });

  it("leaves a page that turns hidden alone", () => {
    followTheSunInBerlin(BEFORE_SUNSET);
    start();
    vi.setSystemTime(AFTER_SUNSET);
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(ui().skin).toBe("paper");
  });
});

describe("other tabs", () => {
  it("adopts a skin preference another tab stored, without writing back", () => {
    start();
    localStorage.setItem("knot.skinMode", "system");
    const setItem = vi.spyOn(Storage.prototype, "setItem");
    fireStorage("knot.skinMode", "system");
    expect(ui().skinMode).toBe("system");
    expect(setItem).not.toHaveBeenCalled();
  });

  it("falls back to the defaults when another tab clears storage", () => {
    stubSystem("dark");
    ui().setSkinMode("system");
    ui().setDarkSkin("nord");
    start();
    expect(ui().skin).toBe("nord");

    localStorage.clear();
    fireStorage(null);
    expect(ui().skinMode).toBe("manual");
    expect(ui().skin).toBe("light");
  });

  it("ignores keys that aren't skin preferences", () => {
    const adopt = vi.spyOn(ui(), "adoptStoredSkinPrefs").mockImplementation(() => {});
    start();
    fireStorage("knot.docWidth", "wide");
    expect(adopt).not.toHaveBeenCalled();
    fireStorage("knot.skinDark", "nord");
    expect(adopt).toHaveBeenCalledTimes(1);
  });
});

describe("cleanup", () => {
  it("removes every listener, the timer and the store subscription", () => {
    const system = stubSystem("light");
    followTheSunInBerlin(BEFORE_SUNSET);
    const stopWatching = start();
    expect(vi.getTimerCount()).toBe(1);

    stopWatching();
    expect(vi.getTimerCount()).toBe(0);
    vi.setSystemTime(AFTER_SUNSET);
    system.set("dark");
    window.dispatchEvent(new Event("focus"));
    document.dispatchEvent(new Event("visibilitychange"));
    window.dispatchEvent(new Event("pageshow"));
    localStorage.setItem("knot.skinMode", "manual");
    fireStorage("knot.skinMode", "manual");
    expect(ui().skinMode).toBe("sun");
    expect(ui().skin).toBe("paper");

    ui().setSkinMode("manual");
    ui().setSkinMode("sun");
    expect(vi.getTimerCount()).toBe(0);
  });
});
