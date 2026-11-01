import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { isScrolledPast, useScrolledPast } from "./useScrolledPast";

afterEach(() => vi.unstubAllGlobals());

function entry(isIntersecting: boolean, bottom: number, rootTop = 0) {
  return {
    isIntersecting,
    boundingClientRect: { bottom } as DOMRectReadOnly,
    rootBounds: { top: rootTop } as DOMRectReadOnly,
  };
}

describe("isScrolledPast", () => {
  it("is true once the element is above the root", () => {
    expect(isScrolledPast(entry(false, -20))).toBe(true);
    expect(isScrolledPast(entry(false, 100, 100))).toBe(true);
  });

  it("is false below the root and while intersecting", () => {
    expect(isScrolledPast(entry(false, 900))).toBe(false);
    expect(isScrolledPast(entry(true, 10))).toBe(false);
  });
});

describe("useScrolledPast", () => {
  it("stays false without IntersectionObserver", () => {
    vi.stubGlobal("IntersectionObserver", undefined);
    const { result } = renderHook(() => useScrolledPast(document.createElement("div")));
    expect(result.current).toBe(false);
  });

  it("calls onBeforeChange before the state flips", () => {
    let fire: ((entries: unknown[]) => void) | null = null;
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        constructor(cb: (entries: unknown[]) => void) {
          fire = cb;
        }
        observe() {}
        disconnect() {}
      },
    );
    const seen: Array<{ next: boolean; stateAtCall: boolean }> = [];
    const el = document.createElement("div");
    const { result } = renderHook(() => {
      const scrolledPast = useScrolledPast(el, (next) => seen.push({ next, stateAtCall: scrolledPast }));
      return scrolledPast;
    });

    act(() => fire!([entry(false, -10)]));
    expect(result.current).toBe(true);
    expect(seen).toEqual([{ next: true, stateAtCall: false }]);

    // An unchanged reading does not call back again.
    act(() => fire!([entry(false, -40)]));
    expect(seen).toHaveLength(1);

    act(() => fire!([entry(true, 30)]));
    expect(result.current).toBe(false);
    expect(seen[1]).toEqual({ next: false, stateAtCall: true });
  });
});
