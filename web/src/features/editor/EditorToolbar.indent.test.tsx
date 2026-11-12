/**
 * Indent / Outdent in the toolbar (#78): the only way to nest a list item on
 * a touch screen, and where the Tab / Shift-Tab shortcut is spelled out.
 *
 * They take the two diagram buttons' slot while the caret is in a list item,
 * so the toolbar keeps its width and never wraps onto a second row as the
 * caret moves in and out of a list (the reason the table actions live in one
 * menu too).
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import type { JSONContent } from "@tiptap/core";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

const p = (text: string): JSONContent => ({ type: "paragraph", content: [{ type: "text", text }] });
const li = (...content: JSONContent[]): JSONContent => ({ type: "list_item", content });
const ul = (...items: JSONContent[]): JSONContent => ({ type: "bullet_list", content: items });

describe("EditorToolbar indent and outdent", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
  });

  /** "intro", then a list "one" / "two". */
  function renderToolbar() {
    bound = mountBoundEditor();
    bound.editor.commands.setContent({ type: "doc", content: [p("intro"), ul(li(p("one")), li(p("two")))] });
    return render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <EditorToolbar editor={bound.editor} docId="doc-1" />
      </QueryClientProvider>,
    );
  }

  /** Puts the caret at the end of the first text containing `word`. */
  function caretIn(word: string) {
    let at = -1;
    bound!.editor.state.doc.descendants((node, position) => {
      if (at < 0 && node.isText && node.text?.includes(word)) at = position + node.nodeSize;
      return at < 0;
    });
    act(() => placeSelection(bound!, at));
  }

  /** The list as JSON without attributes (list items carry `checked: null`). */
  function shape(): string {
    const withoutAttrs = (key: string, value: unknown) => (key === "attrs" ? undefined : value);
    return JSON.stringify(bound!.editor.getJSON().content?.[1], withoutAttrs);
  }

  it("shows neither button outside a list, and the diagram buttons instead", () => {
    renderToolbar();
    caretIn("intro");
    expect(screen.queryByTestId("toolbar-indent")).toBeNull();
    expect(screen.queryByTestId("toolbar-outdent")).toBeNull();
    expect(screen.getByTestId("toolbar-mermaid")).toBeInTheDocument();
    expect(screen.getByTestId("toolbar-excalidraw")).toBeInTheDocument();
  });

  it("takes the diagram buttons' slot in a list, keeping the button count", () => {
    const { container } = renderToolbar();
    caretIn("intro");
    const outside = container.querySelectorAll("button").length;
    caretIn("two");
    expect(screen.getByTestId("toolbar-indent")).toBeInTheDocument();
    expect(screen.getByTestId("toolbar-outdent")).toBeInTheDocument();
    expect(screen.queryByTestId("toolbar-mermaid")).toBeNull();
    expect(screen.queryByTestId("toolbar-excalidraw")).toBeNull();
    expect(container.querySelectorAll("button")).toHaveLength(outside);
  });

  it("names the keyboard shortcuts", () => {
    renderToolbar();
    caretIn("two");
    expect(screen.getByTestId("toolbar-indent")).toHaveAttribute("aria-label", "Indent (Tab)");
    expect(screen.getByTestId("toolbar-outdent")).toHaveAttribute("aria-label", "Outdent (Shift+Tab)");
  });

  it("disables Indent on the first item, which has nothing to nest under", () => {
    renderToolbar();
    caretIn("one");
    expect(screen.getByTestId("toolbar-indent")).toBeDisabled();
    caretIn("two");
    expect(screen.getByTestId("toolbar-indent")).toBeEnabled();
  });

  it("Indent nests the item and Outdent lifts it back", () => {
    renderToolbar();
    caretIn("two");
    fireEvent.click(screen.getByTestId("toolbar-indent"));
    expect(shape()).toBe(JSON.stringify(ul(li(p("one"), ul(li(p("two")))))));

    // Nested as deep as it can go under "one": Indent has nothing left.
    expect(screen.getByTestId("toolbar-indent")).toBeDisabled();
    fireEvent.click(screen.getByTestId("toolbar-outdent"));
    expect(shape()).toBe(JSON.stringify(ul(li(p("one")), li(p("two")))));
  });
});
