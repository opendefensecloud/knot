import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

// The editor needs a WebSocket; a stand-in is enough to see the action row.
vi.mock("../editor/KnotEditor", () => ({
  KnotEditor: () => <div data-testid="editor" />,
}));

import DocPage from "./DocPage";

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

function docBody(role: Role, archived: boolean) {
  return {
    id: "d1",
    workspace_id: "w1",
    parent_id: null,
    title: "Runbook",
    sort_key: "a",
    icon: null,
    created_by: "u-anna",
    archived,
    is_template: false,
    effective_role: role,
    archived_at: archived ? "2026-10-12T09:00:00Z" : null,
    archived_by: archived ? { id: "u-anna", display_name: "Anna" } : null,
    purge_at: archived ? "2026-11-11T09:00:00Z" : null,
    archive_root_id: archived ? "d1" : null,
    archive_root_title: archived ? "Runbook" : null,
  };
}

function stubServer(role: Role, archived: boolean) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/auth/session") {
        return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role: "owner" });
      }
      if (url === "/api/workspace/archive") return json([]);
      if (url === "/api/docs/d1/contributors") {
        return json({
          created_by: { id: "u-anna", display_name: "Anna" },
          created_at: "2026-03-03T10:00:00Z",
          contributors_since: "2026-03-03T10:00:00Z",
          contributors: [],
        });
      }
      if (url === "/api/docs/d1") return json(docBody(role, archived));
      return json({ error: { code: "not_found", message: url, details: {} } }, 404);
    }),
  );
}

function renderDoc() {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter initialEntries={["/doc/d1"]}>
          <Routes>
            <Route path="/doc/:id" element={<DocPage />} />
          </Routes>
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

describe("DocPage — audit log link", () => {
  it.each<[Role, boolean]>([
    ["owner", false],
    ["editor", false],
    ["owner", true],
    ["editor", true],
  ])("is offered to an %s (archived: %s)", async (role, archived) => {
    stubServer(role, archived);
    renderDoc();

    const link = await screen.findByTestId("open-audit");
    expect(link).toHaveAttribute("href", "/doc/d1/audit");
    expect(link).toHaveAccessibleName("Audit log");
  });

  it.each([false, true])("is not offered to a viewer (archived: %s)", async (archived) => {
    // A workspace owner who only views this page: the page's role decides.
    stubServer("viewer", archived);
    renderDoc();

    await screen.findByTestId("toggle-markdown");
    expect(screen.queryByTestId("open-audit")).toBeNull();
  });
});
