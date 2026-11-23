/** The toolbar inserts a table of contents box (depth 3) at the caret. */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { fragmentShape, mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

describe("toolbar table of contents", () => {
  let bound: BoundEditor | null = null;
  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
  });

  it("toolbar-toc inserts a table of contents", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>intro</p>");
    placeSelection(bound, 6);
    render(
      <QueryClientProvider client={new QueryClient()}>
        <EditorToolbar editor={bound.editor} docId="doc-1" />
      </QueryClientProvider>,
    );
    act(() => {
      fireEvent.click(screen.getByTestId("toolbar-toc"));
    });
    expect(fragmentShape(bound)).toContain("table_of_contents");
    let level: unknown = null;
    bound.editor.state.doc.descendants((n) => {
      if (n.type.name === "table_of_contents") level = n.attrs.maxLevel;
    });
    expect(Number(level)).toBe(3);
  });
});
