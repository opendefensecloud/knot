/**
 * List commands against the renamed (snake_case) list nodes.
 *
 * knot renames Tiptap's `bulletList` / `orderedList` / `listItem` to match the
 * canonical schema. Renaming a node does not rewrite the *options* its own
 * commands and keymaps read: BulletList and OrderedList look up
 * `options.itemTypeName`, and ListItem looks up `options.bulletListTypeName`
 * and `options.orderedListTypeName`. Those default to the camelCase names, so
 * without configuration they name node types that do not exist in this schema.
 *
 * The toolbar buttons pass the names explicitly and were never affected, which
 * is why this survived: the failure is confined to the keyboard shortcuts and
 * to ListItem's own Tab / Shift-Tab handling.
 */

import type { JSONContent } from "@tiptap/core";
import { afterEach, describe, expect, it } from "vitest";

import { fragmentXml, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection, typeText } from "../../test/editorInput";

describe("list commands on renamed nodes", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function withLine(text: string): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.insertContent(text);
    return bound;
  }

  /** Top-level node types of the current document. */
  function topTypes(b: BoundEditor): string[] {
    const json = b.editor.getJSON() as { content?: { type: string }[] };
    return (json.content ?? []).map((n) => n.type);
  }

  it("toggleBulletList wraps the line in a bullet_list", () => {
    const b = withLine("a line");
    // Mod-Shift-8 routes here. It threw "There is no node type named
    // 'listItem'", so the shortcut did nothing but log an exception.
    expect(() => b.editor.commands.toggleBulletList()).not.toThrow();
    expect(topTypes(b)).toEqual(["bullet_list"]);
  });

  it("toggleOrderedList wraps the line in an ordered_list", () => {
    const b = withLine("a line");
    expect(() => b.editor.commands.toggleOrderedList()).not.toThrow();
    expect(topTypes(b)).toEqual(["ordered_list"]);
  });

  it("toggleBulletList is reversible", () => {
    const b = withLine("a line");
    b.editor.commands.toggleBulletList();
    expect(topTypes(b)).toEqual(["bullet_list"]);
    b.editor.commands.toggleBulletList();
    expect(topTypes(b)).toEqual(["paragraph"]);
  });

  it("the Mod-Shift-8 keymap reaches the same command", () => {
    // The command tests above are what the shortcut dispatches, but they do
    // not prove the keymap is wired to it. Drive a real keydown through
    // ProseMirror's keymap plugin instead.
    const b = withLine("a line");
    const handled = b.editor.view.someProp("handleKeyDown", (fn) =>
      fn(
        b.editor.view,
        new KeyboardEvent("keydown", { key: "8", code: "Digit8", shiftKey: true, ctrlKey: true }),
      ),
    );
    expect(handled, "Mod-Shift-8 was not handled by any keymap").toBe(true);
    expect(topTypes(b)).toEqual(["bullet_list"]);
  });
});

/**
 * Nesting (#78, "they are all on top level").
 *
 * Tab and Shift-Tab already nested and lifted; the tests for them pin that
 * against a new Tab owner or an extension reorder. What did not nest was
 * typing the Markdown way: "  - " at the start of an item stayed literal
 * text (a list_item must start with a paragraph, so the stock wrapping rule
 * cannot apply there), and "  - " typed after leaving a list joined that
 * list at the top level with its indent swallowed.
 */
describe("nesting list items", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function mount(...content: JSONContent[]): BoundEditor {
    bound = mountBoundEditor();
    if (content.length > 0) bound.editor.commands.setContent({ type: "doc", content });
    return bound;
  }

  /** Sends a keydown through the editor's keymaps, as the browser would. */
  function press(b: BoundEditor, key: string, modifiers: KeyboardEventInit = {}): boolean {
    const handled = b.editor.view.someProp("handleKeyDown", (handleKeyDown) =>
      handleKeyDown(b.editor.view, new KeyboardEvent("keydown", { key, ...modifiers })),
    );
    return handled === true;
  }

  /** Types `lines` the way a person does: each one, then Enter. */
  function typeLines(b: BoundEditor, ...lines: string[]) {
    lines.forEach((line, index) => {
      if (index > 0) press(b, "Enter");
      typeText(b.editor.view, line);
    });
  }

  /** Inside the `nth` empty paragraph of the document. */
  function emptyParagraph(b: BoundEditor, nth: number): number {
    const found: number[] = [];
    b.editor.state.doc.descendants((node, position) => {
      if (node.type.name === "paragraph" && node.content.size === 0) found.push(position + 1);
      return true;
    });
    const position = found[nth];
    if (position === undefined) throw new Error(`no empty paragraph #${nth}`);
    return position;
  }

  it("Tab nests the second item and Shift-Tab lifts it back", () => {
    const b = mount();
    typeLines(b, "- one", "two");
    expect(press(b, "Tab"), "Tab was not handled by any keymap").toBe(true);
    expect(outline(b)).toBe('bullet_list(list_item(p"one" bullet_list(list_item(p"two"))))');
    // The shared document carries the nesting, not just the local view.
    expect(fragmentXml(b)).toContain("<list_item><paragraph>one</paragraph><bullet_list>");

    expect(press(b, "Tab", { shiftKey: true }), "Shift-Tab was not handled").toBe(true);
    expect(outline(b)).toBe('bullet_list(list_item(p"one") list_item(p"two"))');
  });

  it("Tab nests a task item and keeps its checkbox", () => {
    const b = mount();
    typeLines(b, "[ ] task", "sub");
    press(b, "Tab");
    expect(outline(b)).toBe(
      'bullet_list(list_item[checked=false](p"task" bullet_list(list_item[checked=false](p"sub"))))',
    );
  });

  it("Tab nests an ordered item in an ordered list", () => {
    const b = mount();
    typeLines(b, "1. one", "two");
    press(b, "Tab");
    expect(outline(b)).toBe('ordered_list(list_item(p"one" ordered_list(list_item(p"two"))))');
  });

  it('an indented "  - " typed at the start of an item nests it, leaving no marker', () => {
    const b = mount();
    typeLines(b, "- one", "  - two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one" bullet_list(list_item(p"two"))))');
    expect(fragmentXml(b)).toContain("<list_item><paragraph>one</paragraph><bullet_list>");
  });

  it('the same indent on every nested line keeps those lines siblings, as in Markdown', () => {
    // "  - three" names level 2, where Enter already put it; reading the
    // indent as "one deeper than here" would hang three under two.
    const b = mount();
    typeLines(b, "- one", "  - two", "  - three");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two") list_item(p"three"))))',
    );
  });

  it("a deeper indent nests one level further, and a shallower one comes back out", () => {
    const b = mount();
    typeLines(b, "- one", "  - two", "    - three", "  - four");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two" bullet_list(list_item(p"three"))) list_item(p"four"))))',
    );
  });

  it("an indent deeper than the item above allows nests only one level", () => {
    // As in Markdown: there is no level-2 item for a level-3 one to sit in.
    const b = mount();
    typeLines(b, "- one", "      - two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one" bullet_list(list_item(p"two"))))');
  });

  it("a tab counts as one level of indent", () => {
    const b = mount();
    typeLines(b, "- one", "\t- two", "\t\t- three");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two" bullet_list(list_item(p"three"))))))',
    );
  });

  it("a four-space indent on every nested line keeps those lines siblings", () => {
    // "    - two" asks for level 3 at two columns a level, but there is only
    // a level-2 slot under "one"; read that way again, "    - three" would
    // sink under "two" and "    - four" stay beside it.
    const b = mount();
    typeLines(b, "- one", "    - two", "    - three", "    - four");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two") list_item(p"three") list_item(p"four"))))',
    );
  });

  it("four spaces a level: eight nests one further, and four comes back out", () => {
    const b = mount();
    typeLines(b, "- one", "    - two", "        - three", "    - four");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two" bullet_list(list_item(p"three"))) list_item(p"four"))))',
    );
  });

  it("CommonMark's three and six spaces under ordered items keep the same indent siblings", () => {
    const b = mount();
    typeLines(b, "1. one", "   1. two", "      1. three", "      2. four");
    expect(outline(b)).toBe(
      'ordered_list(list_item(p"one" ordered_list(list_item(p"two" ordered_list(list_item(p"three") list_item(p"four"))))))',
    );
  });

  it("a four-space indent after leaving a list keeps the next four-space line a sibling", () => {
    const b = mount();
    typeLines(b, "- one", "", "    - two", "    - three");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two") list_item(p"three"))))',
    );
  });

  it("a two-space list after a four-space one nests at two spaces a level again", () => {
    const b = mount();
    typeLines(b, "- one", "    - two", "", "", "- a", "  - b", "    - c");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two"))) list_item(p"a" bullet_list(list_item(p"b" bullet_list(list_item(p"c"))))))',
    );
  });

  it("a tab is one level whatever width a level has been typed at", () => {
    const b = mount();
    typeLines(b, "- one", "    - two", "", "", "- a", "\t- b", "\t\t- c");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two"))) list_item(p"a" bullet_list(list_item(p"b" bullet_list(list_item(p"c"))))))',
    );
  });

  it("an indented marker on an item already at that level is consumed, not nested again", () => {
    const b = mount();
    typeLines(b, "- one", "two");
    press(b, "Tab");
    press(b, "Enter");
    typeText(b.editor.view, "  - three");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"two") list_item(p"three"))))',
    );
  });

  it('a bare "- " on every line, out of habit, keeps the list flat', () => {
    // Nesting on a bare marker would put each line one deeper than the last.
    const b = mount();
    typeLines(b, "- one", "- two", "- three");
    const shape = outline(b);
    expect(shape.startsWith("bullet_list(list_item(")).toBe(true);
    expect(shape.match(/bullet_list/g)).toHaveLength(1);
    expect(shape.match(/list_item/g)).toHaveLength(3);
  });

  it("an indented marker on the first item, which has nothing to nest under, stays text", () => {
    const b = mount();
    typeText(b.editor.view, "- ");
    typeText(b.editor.view, "  - x");
    expect(outline(b)).toBe('bullet_list(list_item(p"  - x"))');
  });

  it('"  1. " in a bullet item nests it in an ordered list', () => {
    const b = mount();
    typeLines(b, "- one", "  1. two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one" ordered_list(list_item(p"two"))))');
  });

  it('"  - " in an ordered item nests it in a bullet list', () => {
    const b = mount();
    typeLines(b, "1. one", "  - two");
    expect(outline(b)).toBe('ordered_list(list_item(p"one" bullet_list(list_item(p"two"))))');
  });

  it("a marker of the other type joins an existing sublist without converting it", () => {
    // The sublist holds other items too; switching its type would number or
    // unnumber them behind the author's back.
    const b = mount(ul(li(p("one"), ul(li(p("nested")))), li(p(""))));
    placeSelection(b, emptyParagraph(b, 0));
    typeText(b.editor.view, "  1. two");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"nested") list_item(p"two"))))',
    );
  });

  it("a converted sublist joins a sublist of the same type just before it", () => {
    // sinkListItem only joins a sublist of the outer list's type, so the
    // item lands in a new bullet sublist after "a"'s ordered one; turning
    // that into a second ordered list would restart the numbering at 1.
    const b = mount(ul(li(p("one"), ol(li(p("a")))), li(p(""))));
    placeSelection(b, emptyParagraph(b, 0));
    typeText(b.editor.view, "  1. b");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" ordered_list(list_item(p"a") list_item(p"b"))))',
    );
  });

  it('"  - " in a paragraph right after a list nests under its last item', () => {
    // "- one", Enter, Enter leaves the list; the indent then means "under
    // the item above", not "a new top-level item".
    const b = mount();
    typeLines(b, "- one", "", "  - two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one" bullet_list(list_item(p"two"))))');
    expect(fragmentXml(b)).toContain("<list_item><paragraph>one</paragraph><bullet_list>");
    // The caret moved with the paragraph, so typing carries on in it.
    typeText(b.editor.view, "!");
    expect(outline(b)).toBe('bullet_list(list_item(p"one" bullet_list(list_item(p"two!"))))');
  });

  it('"  - " after a list whose last item already has a sublist joins that sublist', () => {
    const b = mount(ul(li(p("one"), ul(li(p("nested"))))), p(""));
    placeSelection(b, emptyParagraph(b, 0));
    typeText(b.editor.view, "  - two");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"nested") list_item(p"two"))))',
    );
  });

  it('"    - " after a list nests under the last item one level down', () => {
    const b = mount(ul(li(p("one"), ul(li(p("nested"))))), p(""));
    placeSelection(b, emptyParagraph(b, 0));
    typeText(b.editor.view, "    - two");
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"one" bullet_list(list_item(p"nested" bullet_list(list_item(p"two"))))))',
    );
    typeText(b.editor.view, "!");
    expect(outline(b)).toContain('list_item(p"two!")');
  });

  it("an indented marker after a task list nests a task item, as Tab does", () => {
    const b = mount();
    typeLines(b, "[ ] task", "", "  - sub");
    expect(outline(b)).toBe(
      'bullet_list(list_item[checked=false](p"task" bullet_list(list_item[checked=false](p"sub"))))',
    );
  });

  it("an indented marker after an imported task list nests a task item", () => {
    // Markdown, templates and the API bring `checked` in as the strings
    // "true" / "false" (see checkedState); those are task items too.
    const b = mount(ul({ ...li(p("task")), attrs: { checked: "false" } }), p(""));
    placeSelection(b, emptyParagraph(b, 0));
    typeText(b.editor.view, "  - sub");
    // outline() shows only a boolean `checked`: the imported parent prints
    // bare, the new item as the task item Tab would have made.
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"task" bullet_list(list_item[checked=false](p"sub"))))',
    );
  });

  it('"  1. " after a bullet list nests an ordered sublist', () => {
    const b = mount();
    typeLines(b, "- one", "", "  1. two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one" ordered_list(list_item(p"two"))))');
  });

  it('a bare "- " after a list still joins it at the top level', () => {
    const b = mount();
    typeLines(b, "- one", "", "- two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one") list_item(p"two"))');
  });

  it('"  - " in a paragraph with no list before it starts a list, as before', () => {
    const b = mount();
    typeText(b.editor.view, "  - one");
    expect(outline(b)).toBe('bullet_list(list_item(p"one"))');
  });

  it("a task marker still makes a task item, and a typed indent nests one", () => {
    const b = mount();
    typeLines(b, "[ ] task", "  - sub");
    expect(outline(b)).toBe(
      'bullet_list(list_item[checked=false](p"task" bullet_list(list_item[checked=false](p"sub"))))',
    );
  });

  it("an indented marker in a code block stays code", () => {
    const b = mount(ul(li(p("one"))), { type: "code_block", content: [] });
    placeSelection(b, b.editor.state.doc.content.size - 1);
    typeText(b.editor.view, "  - two");
    expect(outline(b)).toBe('bullet_list(list_item(p"one")) code_block"  - two"');
  });

  it('"  - " nests in a list inside a table cell', () => {
    const b = mount(table(row(cell(ul(li(p("one")), li(p("")))))));
    placeSelection(b, emptyParagraph(b, 0));
    typeText(b.editor.view, "  - two");
    expect(outline(b)).toBe(
      'table(table_row(table_cell(bullet_list(list_item(p"one" bullet_list(list_item(p"two")))))))',
    );
  });

  it("Delete at the end of an item hoists a branching sublist after it", () => {
    // Upstream's branching-list Delete keymap (@tiptap/extension-list) only
    // recognises the list types ListItem is configured with; left at the
    // camelCase defaults it never matched, and Delete here did nothing.
    // Upstream's behaviour: the sublist's items move up beside the item.
    const b = mount(ul(li(p("a"), ul(li(p("b"), ul(li(p("c")))), li(p("d"))))));
    placeSelection(b, 4); // end of "a"
    expect(press(b, "Delete")).toBe(true);
    expect(outline(b)).toBe(
      'bullet_list(list_item(p"a") list_item(p"b" bullet_list(list_item(p"c"))) list_item(p"d"))',
    );
  });
});

const textNode = (value: string): JSONContent => ({ type: "text", text: value });
const p = (value = ""): JSONContent => ({ type: "paragraph", content: value ? [textNode(value)] : [] });
const li = (...content: JSONContent[]): JSONContent => ({ type: "list_item", content });
const ul = (...items: JSONContent[]): JSONContent => ({ type: "bullet_list", content: items });
const ol = (...items: JSONContent[]): JSONContent => ({ type: "ordered_list", content: items });
const cell = (...content: JSONContent[]): JSONContent => ({ type: "table_cell", content });
const row = (...cells: JSONContent[]): JSONContent => ({ type: "table_row", content: cells });
const table = (...rows: JSONContent[]): JSONContent => ({ type: "table", content: rows });

/**
 * The document on one line: `type(children)`, a paragraph or code block as
 * `p"text"`, and a list item's `checked` as `[checked=…]` when set. Easier to
 * read in a failure than nested JSON, and exact about the nesting.
 */
function outline(b: BoundEditor): string {
  const render = (node: JSONContent): string => {
    const text = (node.content ?? []).map((child) => child.text ?? "").join("");
    if (node.type === "paragraph") return `p"${text}"`;
    if (node.type === "code_block") return `code_block"${text}"`;
    const checked = node.attrs?.checked as boolean | null | undefined;
    const attrs = typeof checked === "boolean" ? `[checked=${checked}]` : "";
    return `${node.type}${attrs}(${(node.content ?? []).map(render).join(" ")})`;
  };
  return (b.editor.getJSON().content ?? []).map(render).join(" ");
}
