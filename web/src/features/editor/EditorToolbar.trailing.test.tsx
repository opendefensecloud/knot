import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { EditorToolbar } from "./EditorToolbar";

describe("EditorToolbar doc-controls slot", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
  });

  function renderToolbar(trailing?: ReactNode, trailingVisible?: boolean) {
    bound = mountBoundEditor();
    return render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <EditorToolbar editor={bound.editor} docId="doc-1" trailing={trailing} trailingVisible={trailingVisible} />
      </QueryClientProvider>,
    );
  }

  it("marks the toolbar as a sticky band", () => {
    renderToolbar();
    expect(screen.getByTestId("editor-toolbar")).toHaveAttribute("data-sticky-band");
    expect(screen.queryByTestId("toolbar-doc-controls")).toBeNull();
  });

  it("renders the trailing controls at the end of the row", () => {
    renderToolbar(<button type="button">Comments</button>, true);
    const slot = screen.getByTestId("toolbar-doc-controls");
    expect(slot).toHaveTextContent("Comments");
    expect(slot.className).not.toContain("invisible");
    expect(slot.parentElement?.lastElementChild).toBe(slot);
  });

  it("keeps the slot laid out but invisible until it is wanted", () => {
    renderToolbar(<button type="button">Comments</button>, false);
    expect(screen.getByTestId("toolbar-doc-controls").className).toContain("invisible");
  });
});
