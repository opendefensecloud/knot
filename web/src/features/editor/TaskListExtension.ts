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

export const TaskListExtension = Extension.create({
  name: "knotTaskList",

  addGlobalAttributes() {
    return [
      {
        types: ["list_item"],
        attributes: {
          checked: {
            default: null,
            keepOnSplit: false,
            parseHTML: (el) => {
              if (el.getAttribute("data-checked") === "true") return true;
              if (el.getAttribute("data-checked") === "false") return false;
              return null;
            },
            renderHTML: (attrs) => {
              if (attrs.checked === true) return { "data-checked": "true" };
              if (attrs.checked === false) return { "data-checked": "false" };
              return {};
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
            if (node.attrs.checked === null || node.attrs.checked === undefined) return false;
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
            const tr = view.state.tr.setNodeAttribute(
              nodePos,
              "checked",
              !node.attrs.checked,
            );
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
    };
  },
});
