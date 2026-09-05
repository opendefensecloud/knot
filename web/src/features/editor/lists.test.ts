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

import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";

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
