/**
 * The toolbar's Link button, wired to a real bound editor: what the popover
 * shows is the link Apply then changes. toolbarLink.test.ts covers the
 * helpers; this pins that EditorToolbar actually uses them.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { linkRuns, placeSelection } from "../../test/editorInput";
import { EditorToolbar } from "./EditorToolbar";

function renderToolbar(bound: BoundEditor, docId = "doc-1") {
  return render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <EditorToolbar editor={bound.editor} docId={docId} />
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

function listedDoc(id: string, title: string, within_restricted: boolean, restricted = false) {
  return {
    id,
    workspace_id: "w1",
    parent_id: null,
    title,
    sort_key: "a",
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    restricted,
    within_restricted,
    parent_hidden: false,
    effective_role: "editor",
  };
}

/** GET /api/docs: an open page, a restricted page and a page inside it. */
function stubDocList() {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const body =
        url === "/api/docs"
          ? [
              listedDoc("open-page", "Open page", false),
              listedDoc("secret-page", "Payroll", true, true),
              listedDoc("secret-child", "Salaries", true),
            ]
          : { error: { code: "not_found", message: url, details: {} } };
      return Promise.resolve(
        new Response(JSON.stringify(body), {
          status: url === "/api/docs" ? 200 : 404,
          headers: { "Content-Type": "application/json" },
        }),
      );
    }),
  );
}

describe("EditorToolbar document link picker", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    bound?.destroy();
    bound = null;
  });

  async function pickDocument(title: string) {
    fireEvent.click(screen.getByTestId("toolbar-link"));
    fireEvent.click(screen.getByTestId("link-mode-doc"));
    const results = screen.getByTestId("link-doc-results");
    fireEvent.click(await within(results).findByRole("button", { name: new RegExp(title) }));
  }

  it("locks restricted pages in the list", async () => {
    stubDocList();
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);
    renderToolbar(bound, "open-page");

    fireEvent.click(screen.getByTestId("toolbar-link"));
    fireEvent.click(screen.getByTestId("link-mode-doc"));
    const results = screen.getByTestId("link-doc-results");
    const payroll = await within(results).findByRole("button", { name: /Payroll/ });
    expect(within(payroll).getByLabelText("Restricted")).toBeInTheDocument();
    const open = within(results).getByRole("button", { name: /Open page/ });
    expect(within(open).queryByLabelText("Restricted")).toBeNull();
  });

  it("inserts 'Restricted page' as the text when linking from an unrestricted page", async () => {
    stubDocList();
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);
    renderToolbar(bound, "open-page");

    await pickDocument("Salaries");
    expect(linkRuns(bound)).toEqual([
      { text: "see", href: null },
      { text: "Restricted page", href: "knot://doc/secret-child" },
    ]);
  });

  // Readers differ by branch even inside one restricted page, so a page in
  // the same subtree doesn't show that every reader here may read the target.
  it("inserts 'Restricted page' when the current page is restricted too", async () => {
    stubDocList();
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);
    renderToolbar(bound, "secret-page");

    await pickDocument("Salaries");
    expect(linkRuns(bound)).toEqual([
      { text: "see", href: null },
      { text: "Restricted page", href: "knot://doc/secret-child" },
    ]);
  });

  it("inserts the title of an unrestricted page from a restricted one", async () => {
    stubDocList();
    bound = mountBoundEditor();
    bound.editor.commands.setContent("<p>see</p>");
    placeSelection(bound, 4);
    renderToolbar(bound, "secret-child");

    await pickDocument("Open page");
    expect(linkRuns(bound)).toEqual([
      { text: "see", href: null },
      { text: "Open page", href: "knot://doc/open-page" },
    ]);
  });
});
