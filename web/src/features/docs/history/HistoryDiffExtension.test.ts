/**
 * The history marks and the `historyChange` attribute, as the viewer's
 * schema renders them.
 *
 * A bare core `Editor` is enough for the schema's `renderHTML`, which is what
 * `getHTML()` serializes. The React node views (`code_block`,
 * `excalidraw_board`, `attachment`) render as an empty span without
 * `EditorContent`, so their DOM is asserted with the mounted viewer instead.
 */

import { Editor, type JSONContent } from "@tiptap/core";
import { afterEach, describe, expect, it } from "vitest";

import { createSchemaExtensions } from "../../editor/extensions";

import { historyDiffExtensions } from "./HistoryDiffExtension";
import { viewerContent } from "./viewerContent";

const editors: Editor[] = [];

function render(content: JSONContent): HTMLElement {
  const editor = new Editor({
    editable: false,
    extensions: [...createSchemaExtensions({ viewer: true }), ...historyDiffExtensions],
    content,
  });
  editors.push(editor);
  return new DOMParser().parseFromString(editor.getHTML(), "text/html").body;
}

afterEach(() => {
  for (const editor of editors.splice(0)) editor.destroy();
});

describe("history diff extensions", () => {
  it("renders history marks and node changes", () => {
    const html = render({
      type: "doc",
      content: [
        {
          type: "paragraph",
          attrs: { historyChange: "removed" },
          content: [
            { type: "text", text: "old", marks: [{ type: "historyDelete" }] },
            { type: "text", text: "new", marks: [{ type: "historyInsert" }] },
          ],
        },
      ],
    });
    const p = html.querySelector('p[data-history-change="removed"]');
    expect(p).not.toBeNull();
    const del = p!.querySelector("del.history-delete");
    expect(del?.textContent).toBe("old");
    expect(del?.hasAttribute("data-history-delete")).toBe(true);
    const ins = p!.querySelector("ins.history-insert");
    expect(ins?.textContent).toBe("new");
    expect(ins?.hasAttribute("data-history-insert")).toBe(true);
  });

  it("history marks render inside code blocks and inline code", () => {
    const doc: JSONContent = {
      type: "doc",
      content: [
        {
          type: "code_block",
          attrs: { language: "rust", historyChange: "changed" },
          content: [
            { type: "text", text: "fn main() {" },
            { type: "text", text: " todo!() ", marks: [{ type: "historyInsert" }] },
            { type: "text", text: "}" },
          ],
        },
        {
          type: "paragraph",
          content: [
            { type: "text", text: "call " },
            { type: "text", text: "run()", marks: [{ type: "code" }, { type: "historyInsert" }] },
          ],
        },
      ],
    };
    const classified = viewerContent(doc);
    expect(classified.mode).toBe("strict");

    const html = render(doc);
    const pre = html.querySelector('pre[data-history-change="changed"]');
    expect(pre).not.toBeNull();
    expect(pre!.querySelector("ins.history-insert")?.textContent).toBe(" todo!() ");

    const code = html.querySelector("p code");
    expect(code).not.toBeNull();
    const ins = code!.querySelector("ins.history-insert") ?? code!.closest("ins.history-insert");
    expect(ins?.textContent).toBe("run()");
  });
});
