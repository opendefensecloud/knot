/**
 * Link titles must survive the editor.
 *
 * `tools/schema.json` declares a `title` attribute on the link mark, and both
 * halves of the Rust side honour it: `from_markdown` parses
 * `[text](url "title")` into the attribute, and `to_markdown` writes it back
 * out. The editor was the only participant that did not declare it, and
 * ProseMirror drops attributes its schema does not know — so a title that
 * arrived through markdown import survived in storage exactly until someone
 * opened the document and typed, at which point the editor rewrote the mark
 * without it.
 *
 * Silent, one-way, and invisible to a round-trip test that never opens the
 * editor in between — which is why import-export.spec.ts could not see it.
 */

import { afterEach, describe, expect, it } from "vitest";

import { fragmentXml, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";

type LinkMark = { type: string; attrs?: Record<string, unknown> };
type TextNode = { type: string; text?: string; marks?: LinkMark[] };

function firstLinkAttrs(bound: BoundEditor): Record<string, unknown> | undefined {
  const json = bound.editor.getJSON() as {
    content?: { content?: TextNode[] }[];
  };
  for (const block of json.content ?? []) {
    for (const inline of block.content ?? []) {
      const link = inline.marks?.find((m) => m.type === "link");
      if (link) return link.attrs;
    }
  }
  return undefined;
}

describe("the link mark carries a title", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("keeps a title set through the document JSON", () => {
    bound = mountBoundEditor();
    bound.editor.commands.insertContent({
      type: "paragraph",
      content: [
        {
          type: "text",
          text: "knot",
          marks: [
            { type: "link", attrs: { href: "https://example.test", title: "Home page" } },
          ],
        },
      ],
    });

    expect(firstLinkAttrs(bound)).toMatchObject({
      href: "https://example.test",
      title: "Home page",
    });
  });

  it("keeps a title parsed out of imported HTML", () => {
    // The shape `from_markdown` produces for `[knot](url "Home page")`.
    bound = mountBoundEditor();
    bound.editor.commands.insertContent(
      '<p><a href="https://example.test" title="Home page">knot</a></p>',
    );

    expect(firstLinkAttrs(bound)?.title).toBe("Home page");
  });

  it("keeps the title in the Y.Doc, not just in the editor's own state", () => {
    // The editor's JSON is derived; the fragment is what is persisted and
    // fanned out, so assert the durable copy too.
    bound = mountBoundEditor();
    bound.editor.commands.insertContent({
      type: "paragraph",
      content: [
        {
          type: "text",
          text: "knot",
          marks: [{ type: "link", attrs: { href: "https://example.test", title: "Home page" } }],
        },
      ],
    });

    // y-prosemirror stores an inline mark as a nested XML element, so the
    // durable form is `<paragraph><link href=… title=…>knot</link></paragraph>`.
    expect(fragmentXml(bound)).toContain('title="Home page"');
  });

  it("omits the attribute entirely when there is no title", () => {
    // A bare link must not start rendering `title=""` — that would change
    // every existing link's markdown output.
    bound = mountBoundEditor();
    bound.editor.commands.insertContent(
      '<p><a href="https://example.test">knot</a></p>',
    );

    expect(firstLinkAttrs(bound)?.title ?? null).toBeNull();
  });
});
