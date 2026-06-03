/**
 * Unit tests for anchor.ts.
 *
 * Full round-trip (encodeAnchor → decodeAnchor) requires a live Tiptap editor
 * with the ySyncPlugin mounted; that's impractical in jsdom. Instead we:
 *   1. Test the y-prosemirror primitives directly with a hand-built mapping.
 *   2. Test that encodeAnchor/decodeAnchor handle null mapping gracefully.
 */

/* eslint-disable @typescript-eslint/no-unsafe-argument, @typescript-eslint/no-explicit-any, @typescript-eslint/no-unsafe-assignment */

import { describe, expect, it } from "vitest";
import * as Y from "yjs";
import {
  absolutePositionToRelativePosition,
  relativePositionToAbsolutePosition,
} from "y-prosemirror";

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
// Tests
// ---------------------------------------------------------------------------

describe("y-prosemirror relative position primitives", () => {
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
