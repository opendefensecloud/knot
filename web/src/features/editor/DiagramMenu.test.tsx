/** The toolbar's diagram menu: Mermaid and Excalidraw share one button. */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

let bound: BoundEditor | null = null;
afterEach(() => {
  cleanup();
  bound?.destroy();
  bound = null;
});

describe("toolbar diagram menu", () => {
  it("offers both diagrams only once opened, and Mermaid inserts a mermaid block", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>intro</p>");
    placeSelection(bound, 6);
    render(
      <QueryClientProvider client={new QueryClient()}>
        <EditorToolbar editor={bound.editor} docId="doc-1" />
      </QueryClientProvider>,
    );
    expect(screen.queryByTestId("toolbar-mermaid")).toBeNull();
    const trigger = screen.getByTestId("toolbar-diagram");
    expect(trigger).toHaveAttribute("aria-haspopup", "menu");
    act(() => {
      fireEvent.click(trigger);
    });
    expect(screen.getByTestId("toolbar-excalidraw")).toBeInTheDocument();
    act(() => {
      fireEvent.click(screen.getByTestId("toolbar-mermaid"));
    });
    let language: unknown = null;
    bound.editor.state.doc.descendants((n) => {
      if (n.type.name === "code_block") language = n.attrs.language;
    });
    expect(language).toBe("mermaid");
    expect(screen.queryByTestId("toolbar-excalidraw")).toBeNull();
  });
});
