/**
 * TaskListExtension — adds GFM task-list affordances on top of the
 * existing `bullet_list` / `list_item` nodes (no new node types).
 *
 * A list item with a `checked` attribute renders with a checkbox; without
 * the attribute, it's a plain bullet. Items with `checked` round-trip
 * through markdown as `- [ ]` / `- [x]`.
 */

import { Extension } from "@tiptap/core";
import { wrappingInputRule } from "@tiptap/core";
import { Plugin, PluginKey } from "@tiptap/pm/state";

/**
 * Normalise the `checked` attribute, which reaches `list_item` from two
 * sources that disagree on type.
 *
 * The input rules and keyboard shortcuts below set a real boolean. But a
 * document parsed server-side by `knot_markdown::from_markdown` stores the
 * Yjs XML attribute as the string `"true"` / `"false"`, and y-prosemirror
 * hands whatever is stored straight through as the node attribute — so an
 * imported or templated checklist arrives as strings. Matching only the
 * boolean form rendered those items as plain bullets.
 *
 * `to_markdown` reads the attribute as a string either way, so both forms
 * already round-trip; only the editor needed to accept both.
 *
 * Returns `null` for a plain bullet (no checkbox).
 */
export function checkedState(value: unknown): boolean | null {
  if (value === true || value === "true") return true;
  if (value === false || value === "false") return false;
  return null;
}

export const TaskListExtension = Extension.create({
  name: "knotTaskList",

  addGlobalAttributes() {
    return [
      {
        types: ["list_item"],
        attributes: {
          checked: {
            default: null,
            // Carry the `checked` attr to the new item on Enter so the
            // user gets another task checkbox instead of a plain bullet.
            // (Checked → checked is mildly odd but matches Tiptap's stock
            // TaskItem behaviour; we'd need a custom keymap to reset to
            // false on split.)
            keepOnSplit: true,
            parseHTML: (el) => {
              if (el.getAttribute("data-checked") === "true") return true;
              if (el.getAttribute("data-checked") === "false") return false;
              return null;
            },
            renderHTML: (attrs) => {
              const state = checkedState(attrs.checked);
              if (state === null) return {};
              return { "data-checked": state ? "true" : "false" };
            },
          },
        },
      },
    ];
  },

  addInputRules() {
    // Match "[ ] " or "[x] " at the very start of a list item. Sets the
    // `checked` attribute and removes the typed marker.
    const itemType = this.editor?.schema.nodes.list_item;
    if (!itemType) return [];
    return [
      wrappingInputRule({
        find: /^\[ \] $/,
        type: itemType,
        getAttributes: () => ({ checked: false }),
      }),
      wrappingInputRule({
        find: /^\[x\] $/i,
        type: itemType,
        getAttributes: () => ({ checked: true }),
      }),
    ];
  },

  addProseMirrorPlugins() {
    return [
      new Plugin({
        key: new PluginKey("knotTaskListClick"),
        props: {
          handleClickOn(view, _pos, node, nodePos, event) {
            // Only handle clicks on list_item nodes that have the checked attr.
            if (node.type.name !== "list_item") return false;
            const state = checkedState(node.attrs.checked);
            if (state === null) return false;
            // The checkbox renders as a pseudo-element at negative left
            // offset from the li. A click at-or-before the li's own left
            // edge is targeting the pseudo-checkbox; clicks strictly past
            // the text content (clientX > rect.left + 4) are content
            // clicks. Using `clientX <= rect.left + 4` (4px slop)
            // correctly captures nested items where the pseudo-element
            // sits further from the viewport edge.
            const li = (event.target as HTMLElement | null)?.closest("li[data-checked]");
            if (!li) return false;
            const rect = li.getBoundingClientRect();
            if (event.clientX > rect.left + 4) return false;
            const tr = view.state.tr.setNodeAttribute(nodePos, "checked", !state);
            view.dispatch(tr);
            event.preventDefault();
            return true;
          },
        },
      }),
    ];
  },

  addKeyboardShortcuts() {
    return {
      // Mod+Shift+9 toggles the current line into a task list (the
      // first item gets `checked: false`). Mirrors Tiptap's default
      // bullet-list shortcut feel.
      "Mod-Shift-9": () => {
        const ed = this.editor;
        if (!ed) return false;
        return ed
          .chain()
          .focus()
          .toggleList("bullet_list", "list_item")
          .updateAttributes("list_item", { checked: false })
          .run();
      },
      // Pressing Enter inside a task list item splits to a new task
      // item. keepOnSplit: true preserves the attribute presence; we
      // then reset checked → false so the new row always starts as an
      // open task (regardless of whether the previous was done).
      Enter: () => {
        const ed = this.editor;
        if (!ed) return false;
        const { $from } = ed.state.selection;
        // Find the enclosing list_item, if any.
        for (let depth = $from.depth; depth > 0; depth -= 1) {
          const node = $from.node(depth);
          if (node.type.name !== "list_item") continue;
          if (checkedState(node.attrs.checked) === null) return false;
          return ed
            .chain()
            .splitListItem("list_item")
            .updateAttributes("list_item", { checked: false })
            .run();
        }
        return false;
      },
    };
  },
});
