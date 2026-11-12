/**
 * The toolbar's table slot: Insert table outside a table, a menu of row,
 * column and table actions inside one. One button either way, so entering a
 * table never wraps the toolbar onto a second row and shifts the page.
 */

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

function renderToolbar(b: BoundEditor) {
  render(
    <QueryClientProvider client={new QueryClient()}>
      <EditorToolbar editor={b.editor} docId="doc-1" />
    </QueryClientProvider>,
  );
}

const rows = (b: BoundEditor) => b.editor.view.dom.querySelectorAll("tr").length;

describe("toolbar table menu", () => {
  it("inserts a table outside one, then offers the menu instead", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>text</p>");
    placeSelection(bound, 3);
    renderToolbar(bound);
    expect(screen.queryByTestId("toolbar-table-menu")).toBeNull();

    fireEvent.click(screen.getByTestId("toolbar-table"));
    expect(rows(bound)).toBe(3);
    expect(screen.queryByTestId("toolbar-table")).toBeNull();
    expect(screen.getByTestId("toolbar-table-menu")).toBeTruthy();
    // Nothing more joins the row: the actions wait in the closed menu.
    expect(screen.queryByTestId("toolbar-table-add-row")).toBeNull();
  });

  it("runs a table action from the menu and closes it", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>text</p>");
    bound.editor.commands.insertTable({ rows: 2, cols: 2, withHeaderRow: true });
    renderToolbar(bound);

    const trigger = screen.getByTestId("toolbar-table-menu");
    fireEvent.click(trigger);
    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    fireEvent.click(screen.getByTestId("toolbar-table-add-row"));
    expect(rows(bound)).toBe(3);
    expect(screen.queryByTestId("toolbar-table-add-row")).toBeNull();
  });

  it("goes back to Insert table when the caret leaves the table", () => {
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>text</p>");
    bound.editor.commands.insertTable({ rows: 2, cols: 2, withHeaderRow: true });
    renderToolbar(bound);
    fireEvent.click(screen.getByTestId("toolbar-table-menu"));

    act(() => placeSelection(bound!, 3));
    expect(screen.getByTestId("toolbar-table")).toBeTruthy();
    expect(screen.queryByTestId("toolbar-table-add-row")).toBeNull();
  });
});
