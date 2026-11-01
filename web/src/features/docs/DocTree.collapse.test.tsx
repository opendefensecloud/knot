import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

import { DocTree } from "./DocTree";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

function doc(id: string, title: string, parent_id: string | null) {
  return {
    id,
    workspace_id: "w1",
    parent_id,
    title,
    sort_key: "m",
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
  };
}

function server(docs: ReturnType<typeof doc>[]) {
  return vi.fn((url: string) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role: "viewer" });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role: "viewer", archive_retention_days: 14 });
    }
    if (url === "/api/docs") return json(docs);
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderTree() {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter>
          <DocTree />
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

describe("DocTree — collapse all", () => {
  it("collapses every page with subpages, then expands them again", async () => {
    vi.stubGlobal(
      "fetch",
      server([
        doc("folder", "Folder", null),
        doc("child", "Child", "folder"),
        doc("grandchild", "Grandchild", "child"),
        doc("other", "Other", null),
        doc("other-child", "Other child", "other"),
      ]),
    );
    renderTree();

    await screen.findByTestId("doc-row-grandchild");
    fireEvent.click(screen.getByRole("button", { name: "Collapse all" }));

    expect(screen.getByTestId("doc-row-folder")).toBeInTheDocument();
    expect(screen.getByTestId("doc-row-other")).toBeInTheDocument();
    expect(screen.queryByTestId("doc-row-child")).toBeNull();
    expect(screen.queryByTestId("doc-row-other-child")).toBeNull();

    // One level down, the nested parent is still collapsed; with a page open
    // again the button collapses once more.
    fireEvent.click(screen.getAllByRole("button", { name: "Expand" })[0]!);
    expect(screen.getByTestId("doc-row-child")).toBeInTheDocument();
    expect(screen.queryByTestId("doc-row-grandchild")).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "Collapse all" }));
    expect(screen.queryByTestId("doc-row-child")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "Expand all" }));
    expect(screen.getByTestId("doc-row-grandchild")).toBeInTheDocument();
    expect(screen.getByTestId("doc-row-other-child")).toBeInTheDocument();
  });

  it("is not offered when no page has subpages", async () => {
    vi.stubGlobal("fetch", server([doc("a", "A", null), doc("b", "B", null)]));
    renderTree();

    await screen.findByTestId("doc-row-b");
    expect(screen.queryByRole("button", { name: "Collapse all" })).toBeNull();
    expect(screen.queryByRole("button", { name: "Expand all" })).toBeNull();
  });
});
