/**
 * The toolbar's Link button, wired to a real bound editor: what the popover
 * shows is the link Apply then changes. toolbarLink.test.ts covers the
 * helpers; this pins that EditorToolbar actually uses them.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { linkRuns, placeSelection } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

function renderToolbar(bound: BoundEditor) {
  return render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <EditorToolbar editor={bound.editor} docId="doc-1" />
    </QueryClientProvider>,
  );
}

function applyUrl(url: string) {
  fireEvent.change(screen.getByTestId("link-input"), { target: { value: url } });
  fireEvent.click(screen.getByTestId("link-apply"));
}

describe("EditorToolbar link button", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
  });

  it("opens on the link at the caret's end and re-points that link", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent('<p>see <a href="https://example.com">docs</a> here</p>');
    placeSelection(bound, 9); // right after "docs", where a paste leaves the caret
    renderToolbar(bound);

    fireEvent.click(screen.getByTestId("toolbar-link"));
    expect(screen.getByTestId("link-input")).toHaveValue("https://example.com");

    applyUrl("https://changed.example");
    expect(linkRuns(bound)).toEqual([
      { text: "see ", href: null },
      { text: "docs", href: "https://changed.example" },
      { text: " here", href: null },
    ]);
  });

  it("inserts the URL as a link when the caret touches no link", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);
    renderToolbar(bound);

    fireEvent.click(screen.getByTestId("toolbar-link"));
    expect(screen.getByTestId("link-input")).toHaveValue("");

    applyUrl("https://new.example/a");
    expect(linkRuns(bound)).toEqual([
      { text: "see", href: null },
      { text: "https://new.example/a", href: "https://new.example/a" },
    ]);
  });
});
