/**
 * What the viewer makes of a version's ProseMirror JSON: `strict` (valid),
 * `lenient` (parses, but `check()` rejects it, as the live editor would
 * still open it) or `invalid` (the schema cannot parse it at all).
 *
 * `pm_json_rendered.json` is the server's `pm_json` render of every
 * `pm_json` fixture case (`crates/knot-history/tests/pm_json.rs` writes it).
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import type { JSONContent } from "@tiptap/core";
import { describe, expect, it } from "vitest";

import { viewerContent } from "./viewerContent";

const RENDERED = join(
  import.meta.dirname,
  "../../../../../crates/knot-history/tests/fixtures/pm_json_rendered.json",
);

interface RenderedCase {
  name: string;
  doc: unknown;
}

const doc = (...content: JSONContent[]): JSONContent => ({ type: "doc", content });
const p = (...content: JSONContent[]): JSONContent => ({ type: "paragraph", content });
const text = (s: string, marks?: unknown[]): JSONContent =>
  (marks ? { type: "text", text: s, marks } : { type: "text", text: s }) as JSONContent;

/** A corpus case's name after its `editor/` or `import/` shape prefix. */
function corpusPath(name: string): string | null {
  const m = /^(editor|import)\/(.*)$/.exec(name);
  return m?.[2] ?? null;
}

/** Cases that hold only inline content must be valid as they are. */
function mustBeStrict(name: string): boolean {
  if (name.startsWith("golden/")) return true;
  const path = corpusPath(name);
  return path !== null && (path.startsWith("paragraph/") || path.startsWith("heading/"));
}

/** The name up to its second `/`: `editor/in-bullet`, `random/1`, … */
function family(name: string): string {
  return name.split("/").slice(0, 2).join("/");
}

describe("viewerContent", () => {
  it("every rendered fixture case is viewable", () => {
    const cases = JSON.parse(readFileSync(RENDERED, "utf8")) as RenderedCase[];
    expect(cases.length).toBeGreaterThan(0);
    const lenient: Record<string, number> = {};
    for (const { name, doc } of cases) {
      const { mode } = viewerContent(doc);
      expect(mode, `${name} cannot be shown`).not.toBe("invalid");
      if (mustBeStrict(name)) expect(mode, `${name} must be strict`).toBe("strict");
      if (mode === "lenient") lenient[family(name)] = (lenient[family(name)] ?? 0) + 1;
    }
    // The corpus wraps blocks straight into list items (corpus.rs,
    // `Container::Bullet` → `bullets(vec![item(children)])`), so a case like
    // `in-bullet/heading` starts a list_item with a non-paragraph, which
    // list_item's `paragraph block*` rejects in check() and the live editor
    // still opens. Pinned per family, so a change in either direction is
    // reviewed rather than absorbed.
    expect(lenient).toMatchSnapshot();
  });

  it("linked and bold inline code is strict", () => {
    // The Import shape: the importer stores inline code with every active
    // mark on one run, sorted by type as pm_json sorts them.
    const linked = doc(
      p(text("x", [{ type: "code", attrs: true }, { type: "link", attrs: { href: "https://example.com" } }])),
    );
    const bold = doc(p(text("x", [{ type: "bold", attrs: true }, { type: "code", attrs: true }])));
    expect(viewerContent(linked).mode).toBe("strict");
    expect(viewerContent(bold).mode).toBe("strict");
  });

  it("stray text and empty containers render leniently", () => {
    const emptyItem = doc({ type: "bullet_list", content: [{ type: "list_item" }] });
    const textInItem = doc({
      type: "bullet_list",
      content: [{ type: "list_item", content: [text("bare")] }],
    });
    expect(viewerContent(emptyItem).mode).toBe("lenient");
    expect(viewerContent(textInItem).mode).toBe("lenient");
  });

  it("an empty doc gets one paragraph", () => {
    for (const empty of [{ type: "doc", content: [] }, { type: "doc" }]) {
      const result = viewerContent(empty);
      expect(result.mode).toBe("strict");
      if (result.mode === "invalid") return;
      expect(result.content.content).toEqual([{ type: "paragraph" }]);
    }
  });

  it("an unknown mark or node is invalid", () => {
    expect(viewerContent(doc(p(text("x", [{ type: "comment" }])))).mode).toBe("invalid");
    expect(viewerContent(doc({ type: "callout" })).mode).toBe("invalid");
  });

  it("keeps the content it classified", () => {
    const content = doc(p(text("kept")));
    expect(viewerContent(content)).toEqual({ mode: "strict", content });
  });
});
