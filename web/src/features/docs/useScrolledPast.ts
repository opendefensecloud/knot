import { useEffect, useRef, useState } from "react";

/** True once `entry`'s element has scrolled out above the top of its root.
 *  Below the viewport does not count: a header that is still ahead of the
 *  reader has not been scrolled past. */
export function isScrolledPast(
  entry: Pick<IntersectionObserverEntry, "isIntersecting" | "boundingClientRect" | "rootBounds">,
): boolean {
  return !entry.isIntersecting && entry.boundingClientRect.bottom <= (entry.rootBounds?.top ?? 0) + 1;
}

/**
 * Whether `el` has scrolled out above the top of the enclosing `<main>`, the
 * app's scroll container. Takes an element rather than a ref so a caller
 * whose target mounts late (after a loading branch) can pass callback-ref
 * state and have the observer follow it.
 *
 * `onBeforeChange` runs inside the observer callback, before the state flips,
 * so a caller can still see what is focused in the DOM that is about to
 * change.
 *
 * Without IntersectionObserver (jsdom) the hook stays false.
 */
export function useScrolledPast(
  el: Element | null,
  onBeforeChange?: (next: boolean) => void,
): boolean {
  const [scrolledPast, setScrolledPast] = useState(false);
  const currentRef = useRef(false);
  const onBeforeChangeRef = useRef(onBeforeChange);
  useEffect(() => {
    onBeforeChangeRef.current = onBeforeChange;
  }, [onBeforeChange]);

  useEffect(() => {
    if (!el || typeof IntersectionObserver === "undefined") return;
    const observer = new IntersectionObserver(
      (entries) => {
        const entry = entries[entries.length - 1];
        if (!entry) return;
        const next = isScrolledPast(entry);
        if (next === currentRef.current) return;
        currentRef.current = next;
        onBeforeChangeRef.current?.(next);
        setScrolledPast(next);
      },
      { root: el.closest("main"), threshold: 0 },
    );
    observer.observe(el);
    return () => {
      observer.disconnect();
      currentRef.current = false;
      setScrolledPast(false);
    };
  }, [el]);

  return scrolledPast;
}
