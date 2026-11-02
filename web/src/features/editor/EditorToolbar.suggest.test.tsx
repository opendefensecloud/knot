import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { SCOPE_TOOLTIP } from "../suggestions/scope";
import { EditorToolbar } from "./EditorToolbar";

describe("EditorToolbar suggest button", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
  });

  function renderToolbar(content: unknown, onSuggest?: () => void) {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(content as never);
    return render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <EditorToolbar editor={bound.editor} docId="doc-1" onSuggest={onSuggest} />
      </QueryClientProvider>,
    );
  }

  const para = (text?: string) => ({ type: "paragraph", content: text ? [{ type: "text", text }] : [] });

  it("toolbar_suggest_disabled_without_caret_insert_scope", () => {
    // No handler at all: disabled, with the plain label.
    renderToolbar([para("hello")]);
    act(() => placeSelection(bound!, 3));
    let btn = screen.getByTestId("toolbar-suggest");
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute("aria-label", "Suggest an insertion");
    expect(btn).toHaveAttribute("title", "Suggest an insertion");
    cleanup();
    bound?.destroy();

    // A caret in an empty block: disabled, with the reason.
    renderToolbar([para()], vi.fn());
    act(() => placeSelection(bound!, 1));
    btn = screen.getByTestId("toolbar-suggest");
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute("title", SCOPE_TOOLTIP.empty_block);
    expect(btn).toHaveAttribute("aria-label", "Suggest an insertion");

    // A selection across blocks: disabled, with that reason.
    act(() => {
      bound!.editor.commands.setContent([para("First"), para("Second")] as never);
      placeSelection(bound!, 3, 10);
    });
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute("title", SCOPE_TOOLTIP.cross_block);

    // A non-empty selection is a replace, which the float covers.
    act(() => placeSelection(bound!, 2, 4));
    expect(btn).toBeDisabled();
    expect(btn).toHaveAttribute("title", "Suggest an insertion");
  });

  it("toolbar_suggest_calls_onSuggest", () => {
    const onSuggest = vi.fn();
    renderToolbar([para("hello")], onSuggest);
    act(() => placeSelection(bound!, 3));
    const btn = screen.getByTestId("toolbar-suggest");
    expect(btn).toBeEnabled();
    expect(btn).toHaveAttribute("title", "Suggest an insertion");
    fireEvent.click(btn);
    expect(onSuggest).toHaveBeenCalledTimes(1);
  });
});
