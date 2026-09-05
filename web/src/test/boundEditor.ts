/**
 * A real Tiptap editor bound to a real Y.Doc, for tests that need the
 * production coupling rather than a stand-in.
 *
 * Several things that matter in knot only exist once the ySyncPlugin has
 * built its binding: comment anchors resolve through `binding.mapping`, and
 * the Y.XmlFragment only gets its snake_case element names when the editor
 * actually writes through the binding. A fake `{ state: {} }` editor cannot
 * exercise any of that — it can only prove the null branches.
 *
 * jsdom is enough. No layout is involved: ProseMirror builds the document and
 * the plugin builds its binding on mount, and neither needs a box model. A
 * mount costs ~40ms, so this is a unit-test fixture, not an e2e substitute.
 *
 * Always uses `createExtensions()` — the shipped extension set — so a test
 * written against this fixture also fails when the extension set drifts.
 */

import { Editor } from "@tiptap/core";
import { Awareness } from "y-protocols/awareness";
import * as Y from "yjs";

import { createExtensions } from "../features/editor/extensions";

export type BoundEditor = {
  editor: Editor;
  ydoc: Y.Doc;
  /** The "default" fragment — the one the server reads and writes. */
  fragment: Y.XmlFragment;
  destroy: () => void;
};

/**
 * Top-level element names of the shared fragment, in document order.
 *
 * Reads the child nodes rather than `fragment.toString()`: the string form is
 * XML, so a comparison against it also depends on attribute order and text
 * content, which is more than these tests mean to assert.
 */
export function fragmentShape(bound: BoundEditor): string[] {
  return bound.fragment.toArray().map((node) => {
    const el = node as { nodeName?: string };
    return el.nodeName ?? "#text";
  });
}

export function mountBoundEditor(
  opts: { user?: { name: string; color: string } } = {},
): BoundEditor {
  const ydoc = new Y.Doc();
  const awareness = new Awareness(ydoc);
  const element = document.createElement("div");
  document.body.appendChild(element);

  const editor = new Editor({
    element,
    extensions: createExtensions({
      doc: ydoc,
      awareness,
      user: opts.user ?? { name: "Test User", color: "#336699" },
    }),
  });

  return {
    editor,
    ydoc,
    fragment: ydoc.getXmlFragment("default"),
    destroy: () => {
      editor.destroy();
      awareness.destroy();
      element.remove();
      ydoc.destroy();
    },
  };
}
