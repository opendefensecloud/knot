import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

import { DocTree } from "./DocTree";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

type Role = "owner" | "editor" | "viewer";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

function doc(id: string, title: string, sortKey: string, effectiveRole: Role) {
  return {
    id,
    workspace_id: "w1",
    parent_id: null,
    title,
    sort_key: sortKey,
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
    restricted: false,
    within_restricted: false,
    parent_hidden: false,
    effective_role: effectiveRole,
  };
}

function renderTree(workspaceRole: Role, pages: unknown[]) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/auth/session") {
        return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role: workspaceRole });
      }
      if (url === "/api/workspace") {
        return json({ id: "w1", slug: "default", name: "W", role: workspaceRole, archive_retention_days: 14 });
      }
      if (url === "/api/docs") return json(pages);
      return json({ error: { code: "not_found", message: url, details: {} } }, 404);
    }),
  );
  function CurrentPath() {
    return <p data-testid="current-path">{useLocation().pathname}</p>;
  }
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter initialEntries={["/"]}>
          <Routes>
            <Route
              path="*"
              element={
                <>
                  <DocTree />
                  <CurrentPath />
                </>
              }
            />
          </Routes>
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

function moreButton(id: string): HTMLElement | null {
  return within(screen.getByTestId(`tree-row-${id}`)).queryByRole("button", { name: "More" });
}

describe("TreeRow — audit log", () => {
  it.each<Role>(["owner", "editor"])("offers an %s of the page its audit log", async (role) => {
    renderTree("viewer", [doc("runbook", "Runbook", "a", role)]);

    await waitFor(() => expect(moreButton("runbook")).not.toBeNull());
    fireEvent.click(moreButton("runbook")!);
    fireEvent.click(within(screen.getByTestId("context-menu")).getByTestId("ctx-audit"));

    await waitFor(() => expect(screen.getByTestId("current-path")).toHaveTextContent("/doc/runbook/audit"));
  });

  it("offers a viewer of the page no audit log, whatever the workspace role", async () => {
    renderTree("owner", [doc("runbook", "Runbook", "a", "viewer"), doc("notes", "Notes", "b", "editor")]);

    await waitFor(() => expect(moreButton("notes")).not.toBeNull());
    expect(moreButton("runbook")).toBeNull();
    expect(screen.queryByTestId("ctx-audit")).toBeNull();
  });
});
