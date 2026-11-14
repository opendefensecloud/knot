/**
 * Pasted lists whose items carry their depth as `aria-level` /
 * `data-aria-level` instead of nesting (#78): rebuilt into nested lists
 * before ProseMirror parses them.
 */

import { afterEach, describe, expect, it } from "vitest";

import { fragmentXml, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { pasteEvent } from "../../test/editorInput";
import { nestListItemsByLevel, nestPastedListsByLevel } from "./pastedListLevels";

/** Parses `html`, nests it, and gives back the markup with whitespace and
 *  the level attributes dropped, so assertions read as structure. */
function nested(html: string): string {
  const template = document.createElement("template");
  template.innerHTML = html;
  nestListItemsByLevel(template.content);
  return template.innerHTML
    .replace(/\s+(data-)?aria-level="\d+"/g, "")
    .replace(/>\s+</g, "><")
    .trim();
}

describe("nestListItemsByLevel", () => {
  it("moves a deeper item under the item before it", () => {
    expect(nested('<ul><li aria-level="1">a</li><li aria-level="2">b</li><li aria-level="1">c</li></ul>')).toBe(
      "<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>",
    );
  });

  it("reads data-aria-level too, and nests several levels", () => {
    expect(
      nested(
        '<ul><li data-aria-level="1">a</li><li data-aria-level="2">b</li>' +
          '<li data-aria-level="3">c</li><li data-aria-level="2">d</li></ul>',
      ),
    ).toBe("<ul><li>a<ul><li>b<ul><li>c</li></ul></li><li>d</li></ul></li></ul>");
  });

  it("keeps an ordered list's sublists ordered", () => {
    expect(nested('<ol><li aria-level="1">a</li><li aria-level="2">b</li></ol>')).toBe(
      "<ol><li>a<ol><li>b</li></ol></li></ol>",
    );
  });

  it("joins Word Online's one-list-per-item wrappers into one nested list", () => {
    // Word Online: each item in its own <ul>, each <ul> in its own <div>,
    // the depth only in data-aria-level, the text in a <p>.
    const html = [
      '<div class="ListContainerWrapper"><ul><li data-aria-level="1"><p>a</p></li></ul></div>',
      '<div class="ListContainerWrapper"><ul><li data-aria-level="2"><p>b</p></li></ul></div>',
      '<div class="ListContainerWrapper"><ol><li data-aria-level="3"><p>c</p></li></ol></div>',
      '<div class="ListContainerWrapper"><ul><li data-aria-level="1"><p>d</p></li></ul></div>',
    ].join("\n");
    expect(nested(html)).toBe(
      '<div class="ListContainerWrapper"><ul>' +
        "<li><p>a</p><ul><li><p>b</p><ol><li><p>c</p></li></ol></li></ul></li>" +
        "<li><p>d</p></li>" +
        "</ul></div>",
    );
  });

  it("does not carry depth across a paragraph between two lists", () => {
    const html = '<ul><li aria-level="1">a</li></ul><p>between</p><ul><li aria-level="2">b</li></ul>';
    expect(nested(html)).toBe('<ul><li>a</li></ul><p>between</p><ul><li>b</li></ul>');
  });

  it("leaves a list that already nests by structure alone", () => {
    const html = '<ul><li aria-level="1">a<ul><li aria-level="2">b</li></ul></li></ul>';
    expect(nested(html)).toBe("<ul><li>a<ul><li>b</li></ul></li></ul>");
  });

  it("leaves Google Docs' list-inside-a-list nesting alone, so no item moves ahead of its sublist", () => {
    // The sublist is a direct child of the outer <ul>, after its parent <li>.
    // Nesting `c` by its level would put it inside `a`, before `b`.
    const html =
      '<ul><li aria-level="1">a</li><ul><li aria-level="2">b</li></ul></ul>' +
      '<ul><li aria-level="2">c</li></ul>';
    expect(nested(html)).toBe("<ul><li>a</li><ul><li>b</li></ul></ul><ul><li>c</li></ul>");
  });

  it("reports whether anything moved", () => {
    const flat = document.createElement("template");
    flat.innerHTML = '<ul><li aria-level="1">a</li><li aria-level="1">b</li></ul>';
    expect(nestListItemsByLevel(flat.content)).toBe(false);

    const deeper = document.createElement("template");
    deeper.innerHTML = '<ul><li aria-level="1">a</li><li aria-level="2">b</li></ul>';
    expect(nestListItemsByLevel(deeper.content)).toBe(true);
  });
});

describe("nestPastedListsByLevel", () => {
  it("returns HTML without levels exactly as it came", () => {
    const html = '<meta charset="utf-8"><ul><li>a</li></ul>';
    expect(nestPastedListsByLevel(html)).toBe(html);
  });

  it("returns HTML whose levels need no change exactly as it came", () => {
    const html = '<ul>\n<li aria-level="1">a</li>\n</ul>';
    expect(nestPastedListsByLevel(html)).toBe(html);
  });
});

describe("pasting levelled list items into the editor", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  it("stores them nested", () => {
    bound = mountBoundEditor();
    bound.editor.view.pasteHTML(
      '<ul><li aria-level="1">one</li><li style="margin-left:36pt" aria-level="2">two</li></ul>',
      pasteEvent(),
    );
    expect(fragmentXml(bound)).toContain(
      "<bullet_list><list_item><paragraph>one</paragraph><bullet_list><list_item><paragraph>two</paragraph>",
    );
  });
});
