/**
 * The toolbar's pressed states follow the editor on their own. Tiptap 3's
 * useEditor doesn't re-render on transactions, so nothing but the toolbar
 * itself can keep `editor.isActive(...)` fresh; these drive the editor
 * directly, never through a toolbar click that would re-render it anyway.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

function renderToolbar(bound: BoundEditor) {
  return render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <EditorToolbar editor={bound.editor} docId="doc-1" />
    </QueryClientProvider>,
  );
}

describe("EditorToolbar pressed state", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
  });

  it("presses Heading 1 when the block becomes a heading", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>title</p>");
    placeSelection(bound, 3);
    renderToolbar(bound);
    expect(screen.getByTestId("toolbar-h1")).toHaveAttribute("aria-pressed", "false");

    act(() => { bound!.editor.commands.toggleHeading({ level: 1 }); });
    expect(screen.getByTestId("toolbar-h1")).toHaveAttribute("aria-pressed", "true");
  });

  it("follows the caret out of a heading and onto bold text", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<h1>title</h1><p>plain <strong>bold</strong></p>");
    placeSelection(bound, 3);
    renderToolbar(bound);
    expect(screen.getByTestId("toolbar-h1")).toHaveAttribute("aria-pressed", "true");

    act(() => { placeSelection(bound!, 16); });
    expect(screen.getByTestId("toolbar-h1")).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("toolbar-bold")).toHaveAttribute("aria-pressed", "true");
  });
});
