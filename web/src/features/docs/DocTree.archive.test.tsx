import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

import { archiveConfirmText } from "./archive";
import { DocTree } from "./DocTree";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(status === 204 ? null : JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
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

/** A workspace with Folder > (Child > Grandchild) and a 14-day retention. */
function server(role: "owner" | "editor" | "viewer") {
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role, archive_retention_days: 14 });
    }
    if (url === "/api/docs" && (!init?.method || init.method === "GET")) {
      return json([
        doc("folder", "Folder", null),
        doc("child", "Child", "folder"),
        doc("grandchild", "Grandchild", "child"),
      ]);
    }
    if (url === "/api/docs/folder" && init?.method === "DELETE") return json(null, 204);
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

describe("DocTree — move to archive", () => {
  it("offers editors 'Move to archive' and says what goes with it", async () => {
    const fetchMock = server("editor");
    vi.stubGlobal("fetch", fetchMock);
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    renderTree();

    await screen.findByTestId("doc-row-folder");
    await waitFor(() => expect(screen.getAllByRole("button", { name: "More" }).length).toBe(3));
    fireEvent.click(screen.getAllByRole("button", { name: "More" })[0]!);
    const item = screen.getByTestId("ctx-delete");
    expect(item).toHaveTextContent("Move to archive");
    await waitFor(() =>
      expect(fetchMock.mock.calls.some(([url]) => url === "/api/workspace")).toBe(true),
    );
    fireEvent.click(item);

    await waitFor(() => expect(confirm).toHaveBeenCalledWith(archiveConfirmText("Folder", 2, 14)));
    await waitFor(() =>
      expect(
        fetchMock.mock.calls.some(
          ([url, init]) => url === "/api/docs/folder" && init?.method === "DELETE",
        ),
      ).toBe(true),
    );
  });

  it("gives viewers no row menu and no archive link", async () => {
    vi.stubGlobal("fetch", server("viewer"));
    renderTree();

    await screen.findByTestId("doc-row-folder");
    expect(screen.queryByRole("button", { name: "More" })).toBeNull();
    fireEvent.contextMenu(screen.getByTestId("doc-row-folder"));
    expect(screen.queryByTestId("ctx-delete")).toBeNull();
    expect(screen.queryByTestId("sidebar-archive")).toBeNull();
  });

  // The row is a drag source, but a drag starts only from a mouse or touch
  // press: no key handler on the row may take Enter (or cancel its default)
  // from the row's own buttons and link, or keyboard users couldn't use them.
  it("lets Enter reach the row's buttons instead of starting a drag", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderTree();

    await waitFor(() => expect(screen.getAllByRole("button", { name: "More" }).length).toBe(3));
    const more = screen.getAllByRole("button", { name: "More" })[0]!;
    const notCancelled = fireEvent.keyDown(more, { key: "Enter", code: "Enter" });
    expect(notCancelled).toBe(true);
  });

  it("links editors to the archive", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderTree();

    const link = await screen.findByTestId("sidebar-archive");
    expect(link).toHaveAttribute("href", "/archive");
  });
});
