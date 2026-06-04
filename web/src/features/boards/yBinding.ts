/**
 * yBinding — bridges Excalidraw's scene state to a Y.Doc `elements` map.
 *
 * Strategy (Option A): one Y.Map keyed by element id. Each value is a
 * cloned ExcalidrawElement. Last-write-wins per id, scoped by Excalidraw's
 * monotonic `version` field.
 *
 * The suppress flag is required because `observeDeep` fires SYNCHRONOUSLY
 * inside `ydoc.transact`. If we did not set the flag before transact, our
 * own writes would echo back into Excalidraw mid-render and loop.
 */

import * as Y from "yjs";
import type { ExcalidrawElement } from "@excalidraw/excalidraw/element/types";
import type { ExcalidrawImperativeAPI } from "@excalidraw/excalidraw/types";

export type ExcalidrawBinding = {
  onChange: (next: readonly ExcalidrawElement[]) => void;
  destroy: () => void;
};

export function bindExcalidraw(
  api: ExcalidrawImperativeAPI,
  ydoc: Y.Doc,
): ExcalidrawBinding {
  const elements = ydoc.getMap<ExcalidrawElement>("elements");
  let suppressOnChange = false;

  // Y → Excalidraw (initial + remote updates).
  function pushToExcalidraw() {
    if (suppressOnChange) return;
    const arr = Array.from(elements.values());
    // Excalidraw orders by fractional index (`el.index`); passing as-is is
    // fine — the renderer sorts internally. Avoid pre-sorting here.
    // Excalidraw treats input as immutable; do not mutate.
    api.updateScene({ elements: arr });
  }
  elements.observeDeep(pushToExcalidraw);
  pushToExcalidraw();

  // Excalidraw → Y (last-write-wins per element id).
  function onChange(next: readonly ExcalidrawElement[]) {
    // CRITICAL: set BEFORE transact. observeDeep fires synchronously inside
    // the transact body. If we toggled the flag inside or after, the
    // observer would see `false` and push our own write back into Excalidraw.
    suppressOnChange = true;
    try {
      ydoc.transact(() => {
        const nextIds = new Set<string>();
        for (const el of next) {
          nextIds.add(el.id);
          const prev = elements.get(el.id);
          if (!prev || prev.version !== el.version) {
            elements.set(el.id, globalThis.structuredClone(el));
          }
        }
        for (const id of Array.from(elements.keys())) {
          if (!nextIds.has(id)) elements.delete(id);
        }
      });
    } finally {
      suppressOnChange = false;
    }
  }

  function destroy() {
    elements.unobserveDeep(pushToExcalidraw);
  }

  return { onChange, destroy };
}
