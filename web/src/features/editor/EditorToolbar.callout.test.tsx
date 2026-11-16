/**
 * Making callout boxes: the toolbar menu, typing `> [!warning] `, Backspace
 * in an empty box, and the Quote button inside a box.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { Node as PMNode } from "@tiptap/pm/model";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection, typeText } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

let bound: BoundEditor | null = null;
afterEach(() => {
  cleanup();
  bound?.destroy();
  bound = null;
});

/** Every blockquote of the document, outermost first: kind and text. */
function quotes(doc: PMNode): { kind: unknown; text: string }[] {
  const out: { kind: unknown; text: string }[] = [];
  doc.descendants((n) => {
    if (n.type.name === "blockquote") out.push({ kind: n.attrs.kind ?? null, text: n.textContent });
  });
  return out;
}

function toolbar(b: BoundEditor) {
  render(
    <QueryClientProvider client={new QueryClient()}>
      <EditorToolbar editor={b.editor} docId="doc-1" />
    </QueryClientProvider>,
  );
}

function pick(kind: string) {
  act(() => {
    fireEvent.click(screen.getByTestId("toolbar-callout"));
  });
  act(() => {
    fireEvent.click(screen.getByTestId(`toolbar-callout-${kind}`));
  });
}

function pressBackspace(b: BoundEditor): boolean {
  const event = new KeyboardEvent("keydown", { key: "Backspace", code: "Backspace" });
  return b.editor.view.someProp("handleKeyDown", (fn) => fn(b.editor.view, event)) ?? false;
}

describe("toolbar callout menu", () => {
  it("wraps the paragraph at the caret in a box of the picked kind", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>intro</p>");
    placeSelection(bound, 3);
    toolbar(bound);
    pick("warning");
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: "warning", text: "intro" }]);
  });

  it("turns a plain quote into a box instead of nesting one", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<blockquote><p>quoted</p></blockquote>");
    placeSelection(bound, 3);
    toolbar(bound);
    pick("tip");
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: "tip", text: "quoted" }]);
  });

  it("changes the kind of the box at the caret", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<blockquote data-kind="warning"><p>x</p></blockquote>');
    placeSelection(bound, 3);
    toolbar(bound);
    pick("note");
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: "note", text: "x" }]);
  });

  it("is a menu button, not a toggle, for screen readers", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<blockquote data-kind="warning"><p>x</p></blockquote>');
    placeSelection(bound, 3);
    toolbar(bound);
    const trigger = screen.getByTestId("toolbar-callout");
    expect(trigger).not.toHaveAttribute("aria-pressed");
    expect(trigger).toHaveAttribute("aria-haspopup", "menu");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
  });

  it("the Quote button inside a box lifts the text out", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<blockquote data-kind="warning"><p>keep</p></blockquote>');
    placeSelection(bound, 3);
    toolbar(bound);
    act(() => {
      fireEvent.click(screen.getByTestId("toolbar-blockquote"));
    });
    expect(quotes(bound.editor.state.doc)).toEqual([]);
    expect(bound.editor.state.doc.textContent).toBe("keep");
  });
});

describe("typing a callout marker", () => {
  it("> [!warning] in an empty paragraph makes a warning box", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p></p>");
    placeSelection(bound, 1);
    typeText(bound.editor.view, "> [!warning] ");
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: "warning", text: "" }]);
  });

  it("[!TIP] at the start of a quote's first paragraph gives the quote that kind", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<blockquote><p></p></blockquote>");
    placeSelection(bound, 2);
    typeText(bound.editor.view, "[!TIP] then text");
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: "tip", text: "then text" }]);
  });

  it("an unknown kind stays text", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<blockquote><p></p></blockquote>");
    placeSelection(bound, 2);
    typeText(bound.editor.view, "[!foo] ");
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: null, text: "[!foo] " }]);
  });
});

describe("Backspace in a box", () => {
  it("at the start of an empty box removes the box and leaves an empty paragraph", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<p>before</p><blockquote data-kind="note"><p></p></blockquote>');
    let inside = -1;
    bound.editor.state.doc.descendants((n, pos) => {
      if (n.type.name === "blockquote") inside = pos + 2;
    });
    placeSelection(bound, inside);
    expect(pressBackspace(bound)).toBe(true);
    expect(quotes(bound.editor.state.doc)).toEqual([]);
    const blocks: string[] = [];
    bound.editor.state.doc.forEach((n) => blocks.push(`${n.type.name}:${n.textContent}`));
    expect(blocks).toEqual(["paragraph:before", "paragraph:"]);
  });

  it("in a box with text behaves as usual", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<blockquote data-kind="note"><p>ab</p></blockquote>');
    placeSelection(bound, 4);
    pressBackspace(bound);
    expect(quotes(bound.editor.state.doc)).toEqual([{ kind: "note", text: "ab" }]);
  });
});
