/**
 * The highlight extension has to tolerate an editor on its way out.
 *
 * Switching from one document to another destroys the current editor before
 * the replacement exists, and React commits the new effect dependencies in
 * between — so effects that push comment data into the editor run at least
 * once against the outgoing instance.
 *
 * Tiptap 3 empties `extensionStorage` on destroy, which turns what used to be
 * a pointless write into a TypeError. Under React Router that is not a logged
 * warning, it is an error boundary: the document page is replaced by
 * "Unexpected Application Error!" and the user loses the editor entirely.
 */

import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";

describe("comment highlights on a destroyed editor", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound = null;
  });

  it("has its storage while the editor is alive", () => {
    bound = mountBoundEditor();
    expect(bound.editor.isDestroyed).toBe(false);
    expect(bound.editor.extensionStorage.commentsHighlight).toBeDefined();
  });

  it("loses its storage once the editor is destroyed", () => {
    // Pins the upstream behaviour this guard exists for. If a future version
    // keeps storage alive past destroy, this test says so plainly rather than
    // leaving the guard looking like superstition.
    const b = mountBoundEditor();
    b.editor.destroy();
    expect(b.editor.isDestroyed).toBe(true);
    expect(b.editor.extensionStorage.commentsHighlight).toBeUndefined();
  });

});
