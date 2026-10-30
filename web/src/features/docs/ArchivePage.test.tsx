import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

import { archiveErrorText, daysUntil, purgeConfirmText } from "./archive";
import ArchivePage from "./ArchivePage";

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

const purgeAt = new Date(Date.now() + 12 * 24 * 60 * 60 * 1000).toISOString();

const folderEntry = {
  id: "folder",
  title: "Folder",
  parent_id: "p1",
  parent_title: "Projects",
  subpage_count: 2,
  archived_at: "2026-10-01T09:00:00Z",
  archived_by: { id: "u-anna", display_name: "Anna" },
  purge_at: purgeAt,
  purging: false,
};

function server(role: "owner" | "editor", entries: unknown[]) {
  let archive = entries;
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace/archive") return json(archive);
    if (init?.method === "POST" && /^\/api\/docs\/folder\/(restore|purge)$/.test(url)) {
      archive = [];
      return json(null, 204);
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderPage() {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter>
          <ArchivePage />
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

describe("ArchivePage", () => {
  it("lists what was archived, by whom, and how long it stays", async () => {
    vi.stubGlobal("fetch", server("editor", [folderEntry]));
    renderPage();

    const row = await screen.findByTestId("archive-row-folder");
    expect(row).toHaveTextContent("Folder");
    expect(row).toHaveTextContent("in Projects");
    expect(row).toHaveTextContent("+2 subpages");
    expect(row).toHaveTextContent("Deleted by Anna");
    expect(row).toHaveTextContent(`Deleted permanently in ${daysUntil(purgeAt)} days`);
    expect(within(row).getByRole("link", { name: "Open" })).toHaveAttribute("href", "/doc/folder");
  });

  it("lets editors restore but not delete permanently", async () => {
    const fetchMock = server("editor", [folderEntry]);
    vi.stubGlobal("fetch", fetchMock);
    renderPage();

    const row = await screen.findByTestId("archive-row-folder");
    await waitFor(() => expect(within(row).getByRole("button", { name: "Restore" })).toBeEnabled());
    expect(within(row).queryByRole("button", { name: "Delete permanently" })).toBeNull();
    fireEvent.click(within(row).getByRole("button", { name: "Restore" }));

    await screen.findByText("Nothing in the archive.");
    expect(
      fetchMock.mock.calls.some(
        ([url, init]) => url === "/api/docs/folder/restore" && init?.method === "POST",
      ),
    ).toBe(true);
  });

  it("lets owners delete permanently after confirming", async () => {
    const fetchMock = server("owner", [folderEntry]);
    vi.stubGlobal("fetch", fetchMock);
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    renderPage();

    const button = await screen.findByRole("button", { name: "Delete permanently" });
    fireEvent.click(button);

    await screen.findByText("Nothing in the archive.");
    expect(confirm).toHaveBeenCalledWith(purgeConfirmText("Folder", 2));
    expect(
      fetchMock.mock.calls.some(
        ([url, init]) => url === "/api/docs/folder/purge" && init?.method === "POST",
      ),
    ).toBe(true);
  });

  it("doesn't claim a deleter it doesn't know", async () => {
    vi.stubGlobal("fetch", server("editor", [{ ...folderEntry, archived_by: null }]));
    renderPage();

    const row = await screen.findByTestId("archive-row-folder");
    expect(row).toHaveTextContent("Deleted on");
    expect(row).not.toHaveTextContent("Deleted by");
    expect(row).not.toHaveTextContent("former member");
  });

  it("shows a purge in progress and offers no restore", async () => {
    vi.stubGlobal("fetch", server("owner", [{ ...folderEntry, purging: true }]));
    renderPage();

    const row = await screen.findByTestId("archive-row-folder");
    expect(row).toHaveTextContent("Being deleted permanently");
    expect(within(row).queryByRole("button", { name: "Restore" })).toBeNull();
    expect(within(row).getByRole("button", { name: "Delete permanently" })).toBeInTheDocument();
  });

  it("doesn't call a page whose parent is hidden a top-level page", async () => {
    vi.stubGlobal(
      "fetch",
      server("editor", [
        { ...folderEntry, parent_id: null, parent_title: null, parent_hidden: true },
        { ...folderEntry, id: "top", title: "Top", parent_id: null, parent_title: null },
      ]),
    );
    renderPage();

    const hiddenParentRow = await screen.findByTestId("archive-row-folder");
    expect(hiddenParentRow).toHaveTextContent("in a page you can't see");
    expect(hiddenParentRow).not.toHaveTextContent("top level");
    expect(screen.getByTestId("archive-row-top")).toHaveTextContent("top level");
  });

  it("offers permanent deletion by the role on the page when the archive says it", async () => {
    // A workspace editor who owns an archived restricted page, and a page
    // a workspace owner may only edit.
    vi.stubGlobal(
      "fetch",
      server("editor", [
        { ...folderEntry, effective_role: "owner" },
        { ...folderEntry, id: "edited", title: "Edited", effective_role: "editor" },
      ]),
    );
    renderPage();

    const ownedRow = await screen.findByTestId("archive-row-folder");
    expect(within(ownedRow).getByRole("button", { name: "Delete permanently" })).toBeInTheDocument();
    expect(
      within(screen.getByTestId("archive-row-edited")).queryByRole("button", { name: "Delete permanently" }),
    ).toBeNull();
  });

  it("says why a permanent deletion was refused for hidden subpages", async () => {
    const notify = vi.spyOn(useUi.getState(), "notify");
    vi.spyOn(window, "confirm").mockReturnValue(true);
    const answer = server("owner", [folderEntry]);
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string, init?: RequestInit) =>
        url === "/api/docs/folder/purge" && init?.method === "POST"
          ? json({ error: { code: "doc.hidden_descendants", message: "", details: {} } }, 409)
          : answer(url, init),
      ),
    );
    renderPage();

    fireEvent.click(await screen.findByRole("button", { name: "Delete permanently" }));
    await waitFor(() =>
      expect(notify).toHaveBeenCalledWith("error", archiveErrorText("doc.hidden_descendants")),
    );
  });

  it("says so when the archive is empty", async () => {
    vi.stubGlobal("fetch", server("editor", []));
    renderPage();
    expect(await screen.findByText("Nothing in the archive.")).toBeInTheDocument();
  });
});
