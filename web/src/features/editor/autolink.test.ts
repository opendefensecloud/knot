/**
 * Autolinking must only ever react to the local user's own typing.
 *
 * Tiptap's autolink plugin is an `appendTransaction` hook that runs on every
 * document-changing transaction — including the ones y-tiptap dispatches to
 * mirror a peer's edit into this editor. So when Bob types "https://… " while
 * Alice merely has the page open in edit mode, Alice's editor appends a link
 * mark of its own, writes it into the Y.Doc, and ships it to the server as
 * Alice's edit. Every peer with the page open does the same. That spurious
 * write is what would credit Alice as a contributor to a page she never
 * touched (and it is duplicate traffic regardless: Bob's own editor already
 * linked the URL in his local transaction).
 */

import { afterEach, describe, expect, it } from "vitest";
import * as Y from "yjs";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";

type Mark = { type: string; attrs?: Record<string, unknown> };
type Inline = { type: string; text?: string; marks?: Mark[] };

function inlines(bound: BoundEditor): Inline[] {
  const json = bound.editor.getJSON() as { content?: { content?: Inline[] }[] };
  return (json.content ?? []).flatMap((block) => block.content ?? []);
}

function hasLink(bound: BoundEditor): boolean {
  return inlines(bound).some((n) => n.marks?.some((m) => m.type === "link"));
}

describe("autolink", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("links a URL the local user types", () => {
    bound = mountBoundEditor();
    bound.editor.chain().focus().insertContent("https://example.com ").run();

    expect(hasLink(bound)).toBe(true);
  });

  it("leaves a URL that arrived from a peer alone, and writes nothing back", () => {
    bound = mountBoundEditor();
    const local = bound;

    // A peer's edit, built directly against a replica of the Y.Doc so the
    // peer side runs no autolink of its own — exactly the bytes a remote
    // client without a link mark would send.
    const peer = new Y.Doc();
    Y.applyUpdate(peer, Y.encodeStateAsUpdate(local.ydoc));
    const p = new Y.XmlElement("paragraph");
    p.insert(0, [new Y.XmlText("https://example.com ")]);
    const frag = peer.getXmlFragment("default");
    frag.insert(frag.length, [p]);

    const writes: unknown[] = [];
    local.ydoc.on("update", (_u: Uint8Array, origin: unknown) => {
      if (origin !== "remote") writes.push(origin);
    });
    Y.applyUpdate(
      local.ydoc,
      Y.encodeStateAsUpdate(peer, Y.encodeStateVector(local.ydoc)),
      "remote",
    );

    // The peer's text did reach the editor…
    expect(inlines(local).map((n) => n.text ?? "").join("")).toBe("https://example.com ");
    // …without this editor decorating it or echoing anything back.
    expect(hasLink(local)).toBe(false);
    expect(writes).toEqual([]);
    peer.destroy();
  });
});
