import { Editor } from "@tiptap/core";
import { describe, expect, it } from "vitest";

import { historyDiffExtensions } from "../docs/history/HistoryDiffExtension";

import { createSchemaExtensions } from "./extensions";
import { checkedState } from "./TaskListExtension";

/**
 * The `checked` attribute reaches `list_item` from two sources that disagree
 * on type, so the editor has to accept both. Importing a Markdown file with a
 * checklist used to render plain bullets because only the boolean form was
 * understood.
 */
describe("checkedState", () => {
  it("accepts the booleans the editor's own input rules set", () => {
    expect(checkedState(true)).toBe(true);
    expect(checkedState(false)).toBe(false);
  });

  it("accepts the strings a server-parsed document stores in Yjs", () => {
    expect(checkedState("true")).toBe(true);
    expect(checkedState("false")).toBe(false);
  });

  it("reports a plain bullet as null", () => {
    expect(checkedState(null)).toBeNull();
    expect(checkedState(undefined)).toBeNull();
    expect(checkedState("")).toBeNull();
    expect(checkedState("yes")).toBeNull();
  });
});

/**
 * A past version's checkbox must not tick in the history viewer: the click
 * plugin dispatches with no editable check, and ProseMirror runs
 * `handleClickOn` from mousedown, which fires on a non-editable view too.
 *
 * jsdom has no layout and no `elementFromPoint`, so ProseMirror's own
 * mousedown → `handleClickOn` path cannot be driven by a DOM click; the
 * test calls the plugin prop the way that path does.
 */
describe("the task click in the viewer", () => {
  function clickCheckbox(viewer: boolean) {
    const editor = new Editor({
      editable: false,
      // The viewer's code_block names the history marks, so its schema
      // needs them, as the viewer has them.
      extensions: [...createSchemaExtensions({ viewer }), ...(viewer ? historyDiffExtensions : [])],
      content: {
        type: "doc",
        content: [
          {
            type: "bullet_list",
            content: [
              {
                type: "list_item",
                attrs: { checked: false },
                content: [{ type: "paragraph", content: [{ type: "text", text: "task" }] }],
              },
            ],
          },
        ],
      },
    });
    const { view } = editor;
    const li = view.dom.querySelector("li[data-checked]");
    expect(li).not.toBeNull();
    // The bullet_list opens at 0, so its list_item sits at 1.
    const liPos = 1;
    const liNode = view.state.doc.nodeAt(liPos)!;
    expect(liNode.type.name).toBe("list_item");
    const event = { target: li, clientX: 0, preventDefault() {} } as unknown as MouseEvent;
    const handled = view.someProp("handleClickOn", (f) =>
      f(view, liPos + 2, liNode, liPos, event, true),
    );
    const checked = view.state.doc.nodeAt(liPos)!.attrs.checked as unknown;
    const shown = view.dom.querySelector("li[data-checked]")?.getAttribute("data-checked");
    editor.destroy();
    return { handled, checked, shown };
  }

  it("a_viewer_task_click_changes_nothing", () => {
    const viewer = clickCheckbox(true);
    expect(viewer.handled).toBeFalsy();
    expect(viewer.checked).toBe(false);
    expect(viewer.shown).toBe("false");

    // The live editor's behaviour is unchanged.
    const live = clickCheckbox(false);
    expect(live.handled).toBe(true);
    expect(live.checked).toBe(true);
    expect(live.shown).toBe("true");
  });
});
