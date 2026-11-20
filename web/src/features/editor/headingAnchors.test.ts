/**
 * Headings get GitHub-style ids in the editor's DOM (decorations, never
 * stored), and `tocEntries` lists them for a table of contents.
 */

import { afterEach, describe, expect, it } from "vitest";

import { fragmentXml, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { tocEntries } from "./headingAnchors";

let bound: BoundEditor | null = null;
afterEach(() => {
  bound?.destroy();
  bound = null;
});

describe("heading anchors", () => {
  it("gives headings slug ids with the h- prefix in the DOM", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<h2>Tasks</h2><p>x</p><h2>Tasks</h2><h3>Café</h3>");
    const ids = Array.from(bound.editor.view.dom.querySelectorAll("h2, h3")).map((h) => h.id);
    expect(ids).toEqual(["h-tasks", "h-tasks-1", "h-cafe"]);
    expect(fragmentXml(bound)).not.toContain("tasks");
  });

  it("tocEntries respects maxLevel and indents relative, capped at 4", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<h2>B</h2><h3>C</h3><h6>F</h6><h1>A</h1>");
    const doc = bound.editor.state.doc;
    expect(tocEntries(doc, 6).map((e) => [e.level, e.text, e.slug])).toEqual([
      [1, "B", "h-b"],
      [2, "C", "h-c"],
      [4, "F", "h-f"],
      [0, "A", "h-a"],
    ]);
    expect(tocEntries(doc, 3).map((e) => [e.level, e.text])).toEqual([
      [1, "B"],
      [2, "C"],
      [0, "A"],
    ]);
  });

  it("leaves out headings in table cells, which Markdown writes as text", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(
      "<table><tr><td><h2>Tasks</h2></td></tr></table><h2>Tasks</h2>",
    );
    expect(tocEntries(bound.editor.state.doc, 3).map((e) => e.slug)).toEqual(["h-tasks"]);
    const ids = Array.from(bound.editor.view.dom.querySelectorAll("h2")).map((h) => h.id);
    expect(ids).toEqual(["", "h-tasks"]);
  });

  it("ends a heading's anchor text at a hard break, as Markdown does", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<h2>A<br>B</h2>");
    expect(tocEntries(bound.editor.state.doc, 3).map((e) => [e.text, e.slug])).toEqual([["A", "h-a"]]);
  });

  it("skips empty headings but keeps the slugs of the rest stable", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<h2></h2><h2>B</h2><h2>  </h2>");
    expect(tocEntries(bound.editor.state.doc, 3).map((e) => [e.level, e.text, e.slug])).toEqual([
      [0, "B", "h-b"],
    ]);
  });
});
