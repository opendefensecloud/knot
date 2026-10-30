/**
 * Doc links whose target the user can't read (not in the `['docs']` list)
 * get a locked style. The readable set is pushed in from React, and a new
 * set redecorates links that are already in the document.
 */

import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { setReadableDocIds } from "./InternalLinkExtension";

const READABLE = "0b8f1c1e-2c1a-4c55-9a39-2f0e6f7f0a11";
const HIDDEN = "7a1d2e3f-4b5c-4d6e-8f90-a1b2c3d4e5f6";

function withDocLinks(bound: BoundEditor) {
  bound.editor.commands.setContent({
    type: "doc",
    content: [
      {
        type: "paragraph",
        content: [
          { type: "text", text: "Open", marks: [{ type: "link", attrs: { href: `knot://doc/${READABLE}` } }] },
          { type: "text", text: " and " },
          { type: "text", text: "Hidden", marks: [{ type: "link", attrs: { href: `knot://doc/${HIDDEN}` } }] },
        ],
      },
    ],
  });
}

/** The decorated span of the link whose text is `text`. */
function chip(bound: BoundEditor, text: string): HTMLElement {
  const found = Array.from(bound.editor.view.dom.querySelectorAll<HTMLElement>(".knot-internal-link")).find(
    (el) => el.textContent === text,
  );
  if (!found) throw new Error(`no doc link "${text}"`);
  return found;
}

describe("InternalLinkExtension locked links", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("locks no link while the readable pages are unknown", () => {
    bound = mountBoundEditor();
    withDocLinks(bound);
    expect(chip(bound, "Open")).not.toHaveClass("knot-internal-link--locked");
    expect(chip(bound, "Hidden")).not.toHaveClass("knot-internal-link--locked");
  });

  it("locks links to pages the user can't read, and redecorates when the set changes", () => {
    bound = mountBoundEditor();
    withDocLinks(bound);

    setReadableDocIds(bound.editor, new Set([READABLE]));
    expect(chip(bound, "Open")).not.toHaveClass("knot-internal-link--locked");
    expect(chip(bound, "Hidden")).toHaveClass("knot-internal-link--locked");

    setReadableDocIds(bound.editor, new Set([READABLE, HIDDEN]));
    expect(chip(bound, "Hidden")).not.toHaveClass("knot-internal-link--locked");
  });
});
