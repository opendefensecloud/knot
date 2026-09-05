/**
 * Unit tests for anchor.ts.
 *
 * Three layers, in descending order of what they actually protect:
 *
 *   1. Against a LIVE ySync binding (mountBoundEditor). This is the layer that
 *      matters: it is the only one that executes `getMapping()` for real, and
 *      so the only one that fails if anchor.ts ever reads the plugin state
 *      through a key the mounted plugin did not register under. Everything
 *      about comment anchoring is silent when that happens — encode returns
 *      null and the caller persists an empty string — so there is no second
 *      chance to notice.
 *   2. The binding's position primitives with a hand-built mapping, which
 *      pin the relative-position semantics we depend on.
 *   3. The null branches, via a fake editor.
 *
 * An earlier version of this file had only (2) and (3). Both stay green when
 * the production path is completely broken.
 */

/* eslint-disable @typescript-eslint/no-unsafe-argument, @typescript-eslint/no-explicit-any, @typescript-eslint/no-unsafe-assignment */

import { afterEach, describe, expect, it } from "vitest";
import * as Y from "yjs";
import {
  absolutePositionToRelativePosition,
  relativePositionToAbsolutePosition,
} from "@tiptap/y-tiptap";

import { fragmentShape, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import {
  decodeAnchor,
  decodeAnchorRange,
  encodeAnchor,
  encodeAnchorRange,
} from "./anchor";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type AnyMap = Map<any, any>;

/** Build a minimal ProsemirrorMapping from an XmlFragment with one XmlText. */
function buildMapping(fragment: Y.XmlFragment, text: Y.XmlText): AnyMap {
  const mapping: AnyMap = new Map();
  mapping.set(fragment, { nodeSize: fragment.length + 2 });
  mapping.set(text, { nodeSize: text.length });
  return mapping;
}

// ---------------------------------------------------------------------------
// (1) Against a live ySync binding — the layer that guards the real path
// ---------------------------------------------------------------------------

describe("anchors against a live ySync binding", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  /** Mount, type `text` into the empty first paragraph, return the fixture. */
  function withText(text: string): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.insertContent(text);
    return bound;
  }

  it("reaches the ySync mapping, so encodeAnchor returns an anchor", () => {
    const { editor, ydoc } = withText("Hello world");

    const anchor = encodeAnchor(editor, ydoc, 7);

    // The assertion that matters. A null here means getMapping() could not
    // find the binding, which is indistinguishable from "no editor yet" to
    // every caller — KnotEditor persists it as position_y: "".
    expect(anchor).not.toBeNull();
    expect(anchor).toMatch(/^[A-Za-z0-9+/]+=*$/);
  });

  it("round-trips a position through encode and decode", () => {
    const { editor, ydoc } = withText("Hello world");

    const anchor = encodeAnchor(editor, ydoc, 7);
    expect(decodeAnchor(editor, ydoc, anchor!)).toBe(7);
  });

  it("keeps a range over the same text when earlier text is inserted", () => {
    const { editor, ydoc } = withText("Hello world");
    // "world" occupies [7, 12): position 1 is the first character of the
    // paragraph, and "Hello " is six characters.
    expect(editor.state.doc.textBetween(7, 12)).toBe("world");

    const { start, end } = encodeAnchorRange(editor, ydoc, 7, 12);
    expect(start).not.toBeNull();
    expect(end).not.toBeNull();

    // A peer types ahead of the anchored word.
    editor.commands.insertContentAt(1, "Once again: ");

    const range = decodeAnchorRange(editor, ydoc, start!, end!);
    expect(range).not.toBeNull();
    // Offsets moved; the anchored text did not. This is the property a
    // comment highlight actually depends on — asserting the numbers alone
    // would pass for an anchor that had silently stopped tracking.
    expect(range).not.toEqual({ from: 7, to: 12 });
    expect(editor.state.doc.textBetween(range!.from, range!.to)).toBe("world");
  });

  it("resolves an anchor written into a snake_case list item", () => {
    // Guards the renamed-node bridge as well as the anchor: knot's schema is
    // snake_case, and the binding is what carries those names into the Y.Doc.
    bound = mountBoundEditor();
    const { editor, ydoc } = bound;
    editor.commands.insertContent({
      type: "bullet_list",
      content: [
        {
          type: "list_item",
          content: [{ type: "paragraph", content: [{ type: "text", text: "buy milk" }] }],
        },
      ],
    });
    expect(fragmentShape(bound)).toContain("bullet_list");

    const at = editor.state.doc.content.size - 2;
    const anchor = encodeAnchor(editor, ydoc, at);
    expect(anchor).not.toBeNull();
    expect(decodeAnchor(editor, ydoc, anchor!)).toBe(at);
  });
});

// ---------------------------------------------------------------------------
// (2) The primitives, with a hand-built mapping
// ---------------------------------------------------------------------------

describe("Yjs relative position primitives", () => {
  it("round-trips position 0 on an empty XmlFragment", () => {
    const ydoc = new Y.Doc();
    const fragment = ydoc.getXmlFragment("default");
    const mapping: AnyMap = new Map();
    mapping.set(fragment, { nodeSize: 2 });

    const rel = absolutePositionToRelativePosition(0, fragment, mapping);
    const bytes = Y.encodeRelativePosition(rel);
    const decoded = Y.decodeRelativePosition(bytes);
    const abs = relativePositionToAbsolutePosition(ydoc, fragment, decoded, mapping);
    // Position 0 resolves to 0 on an empty fragment
    expect(abs).toBe(0);
  });

  it("encodes and decodes a mid-text position", () => {
    const ydoc = new Y.Doc();
    const fragment = ydoc.getXmlFragment("default");

    // Insert "Hello World" into a text node inside the fragment
    const text = new Y.XmlText();
    ydoc.transact(() => {
      fragment.insert(0, [text]);
      text.insert(0, "Hello World");
    });

    const mapping = buildMapping(fragment, text);
    const targetPos = 6; // position of 'W' in "Hello World"

    const rel = absolutePositionToRelativePosition(targetPos, fragment, mapping);
    const bytes = Y.encodeRelativePosition(rel);

    // Survive a base64 encode/decode cycle (btoa/atob are available in jsdom)
    const b64 = btoa(String.fromCharCode(...bytes));
    const restored = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
    const decoded = Y.decodeRelativePosition(restored);

    const abs = relativePositionToAbsolutePosition(ydoc, fragment, decoded, mapping);
    expect(abs).toBe(targetPos);
  });

  it("returns null for a resolved position after text is deleted", () => {
    const ydoc = new Y.Doc();
    const fragment = ydoc.getXmlFragment("default");

    const text = new Y.XmlText();
    ydoc.transact(() => {
      fragment.insert(0, [text]);
      text.insert(0, "Temporary");
    });

    const mapping = buildMapping(fragment, text);
    const rel = absolutePositionToRelativePosition(4, fragment, mapping);

    // Delete the text node entirely
    ydoc.transact(() => {
      fragment.delete(0, 1);
    });

    // After deletion the mapping no longer contains the old text node;
    // relativePositionToAbsolutePosition returns null.
    const emptyMapping: AnyMap = new Map();
    emptyMapping.set(fragment, { nodeSize: 2 });

    const abs = relativePositionToAbsolutePosition(ydoc, fragment, rel, emptyMapping);
    expect(abs).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// (3) Null branches, via a fake editor
// ---------------------------------------------------------------------------

describe("encodeAnchor / decodeAnchor — null-safety", () => {
  it("encodeAnchor returns null when editor has no ySyncPlugin state", async () => {
    const { encodeAnchor } = await import("./anchor");
    const ydoc = new Y.Doc();
    // Fake editor: ySyncPluginKey.getState returns undefined for unknown state
    const fakeEditor = { state: {} };
    const result = encodeAnchor(fakeEditor as any, ydoc, 0);
    expect(result).toBeNull();
  });

  it("decodeAnchor returns null when editor has no ySyncPlugin state", async () => {
    const { decodeAnchor } = await import("./anchor");
    const ydoc = new Y.Doc();
    const fakeEditor = { state: {} };
    const result = decodeAnchor(fakeEditor as any, ydoc, "AAAA");
    expect(result).toBeNull();
  });
});

describe("encodeAnchorRange / decodeAnchorRange", () => {
  it("encodeAnchorRange returns {start:null,end:null} when mapping is missing", async () => {
    const { encodeAnchorRange } = await import("./anchor");
    const ydoc = new Y.Doc();
    const fakeEditor = { state: {} };
    const r = encodeAnchorRange(fakeEditor as any, ydoc, 0, 5);
    expect(r).toEqual({ start: null, end: null });
  });

  it("decodeAnchorRange returns null when either anchor fails to resolve", async () => {
    const { decodeAnchorRange } = await import("./anchor");
    const ydoc = new Y.Doc();
    const fakeEditor = { state: {} };
    const r = decodeAnchorRange(fakeEditor as any, ydoc, "AAAA", "AAAA");
    expect(r).toBeNull();
  });
});
