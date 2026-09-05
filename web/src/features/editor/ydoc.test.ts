/**
 * The editor must write into the Y.Doc only what the user actually did.
 *
 * Every local transaction is persisted and fanned out to every peer, so an
 * extension that "tidies" the document — appending a trailing paragraph,
 * normalising a structure on load — is not a local convenience here. It is a
 * write to shared, durable state, it reaches `to_markdown`, and it happens
 * without anyone having typed anything.
 *
 * These tests are deliberately structural: they compare the sequence of
 * top-level element names in the Y.XmlFragment before and after an edit,
 * rather than the text. A test that seeds a document ending in a paragraph
 * cannot see the failure at all, because the tidying extension has nothing to
 * append — so the fixtures below end in a code block and a list on purpose.
 */

import { afterEach, describe, expect, it } from "vitest";
import * as Y from "yjs";

import { fragmentShape, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";

describe("the editor writes only the user's edits into the Y.Doc", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("does not append anything to a document ending in a code block", () => {
    bound = mountBoundEditor();
    const { editor } = bound;
    editor.commands.insertContent([
      { type: "paragraph", content: [{ type: "text", text: "intro" }] },
      { type: "code_block", content: [{ type: "text", text: "x = 1" }] },
    ]);

    const before = fragmentShape(bound);
    expect(before.at(-1),
      "an extension appended a node the user did not type — the document no longer ends where they left it").toBe("code_block");

    // One ordinary edit, far from the end.
    editor.commands.insertContentAt(3, "!");

    expect(editor.state.doc.textBetween(1, 7)).toContain("!");
    expect(
      fragmentShape(bound),
      "the fragment gained or lost a top-level node that the user did not type",
    ).toEqual(before);
  });

  it("does not append anything to a document ending in a list", () => {
    bound = mountBoundEditor();
    const { editor } = bound;
    editor.commands.insertContent([
      { type: "paragraph", content: [{ type: "text", text: "todo" }] },
      {
        type: "bullet_list",
        content: [
          {
            type: "list_item",
            content: [{ type: "paragraph", content: [{ type: "text", text: "one" }] }],
          },
        ],
      },
    ]);

    const before = fragmentShape(bound);
    expect(before.at(-1),
      "an extension appended a node the user did not type — the document no longer ends where they left it").toBe("bullet_list");

    editor.commands.insertContentAt(3, "!");

    expect(fragmentShape(bound)).toEqual(before);
  });

  it("mounting a document without editing it produces no update at all", () => {
    // A viewer opening a document must not dirty it. The server drops writes
    // from a viewer, but a write that reaches the socket at all is a bug
    // worth catching here rather than in an ACL log.
    bound = mountBoundEditor();
    const { editor, ydoc } = bound;
    editor.commands.insertContent([
      { type: "paragraph", content: [{ type: "text", text: "read only" }] },
      { type: "code_block", content: [{ type: "text", text: "y = 2" }] },
    ]);

    const seeded = Y.encodeStateAsUpdate(ydoc);

    // A second client opens the same document and touches nothing.
    const reader = mountBoundEditor();
    try {
      Y.applyUpdate(reader.ydoc, seeded);
      let updates = 0;
      reader.ydoc.on("update", () => {
        updates += 1;
      });
      // Give the binding the microtask it uses to reconcile after a remote
      // update, which is where an on-load rewrite would land.
      reader.editor.view.dispatch(reader.editor.state.tr);

      expect(updates, "opening a document emitted a Y.Doc update").toBe(0);
      expect(fragmentShape(reader).at(-1),
        "an extension appended a node the user did not type — the document no longer ends where they left it").toBe("code_block");
    } finally {
      reader.destroy();
    }
  });
});
