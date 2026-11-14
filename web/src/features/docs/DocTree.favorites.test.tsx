import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import type { Favorite } from "../../lib/validators";
import { useUi } from "../../stores/ui";

import { DocTree } from "./DocTree";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

beforeEach(() => {
  useUi.setState({ toasts: [] });
});

type Role = "owner" | "editor" | "viewer";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

function doc(id: string, title: string, parent_id: string | null, is_template = false) {
  return {
    id,
    workspace_id: "w1",
    parent_id,
    title,
    sort_key: "m",
    icon: null,
    created_by: "u1",
    archived: false,
    is_template,
    archived_at: null,
    archive_root_id: null,
  };
}

function fav(id: string, title = id): Favorite {
  return { doc_id: id, title, icon: null, favorited_at: "2026-11-14T10:00:00Z" };
}

const DOCS = [doc("alpha", "Alpha", null), doc("beta", "Beta", null), doc("tpl", "Template", null, true)];

type Opts = { role?: Role; favorites?: Favorite[]; favoritesGate?: Promise<unknown> };

/** The session, workspace and docs list, plus a fake `/api/me/favorites`. */
function server({ role = "viewer", favorites = [], favoritesGate }: Opts = {}) {
  let list = [...favorites];
  return vi.fn((url: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role, archive_retention_days: 14 });
    }
    if (url === "/api/docs") return json(DOCS);
    if (url === "/api/me/favorites" && method === "GET") {
      return (favoritesGate ?? Promise.resolve()).then(() => json(list));
    }
    const m = /^\/api\/me\/favorites\/([^/]+)$/.exec(url);
    if (m && method === "PUT") {
      const id = decodeURIComponent(m[1]!);
      if (!list.some((f) => f.doc_id === id)) list = [...list, fav(id)];
      return Promise.resolve(new Response(null, { status: 204 }));
    }
    if (m && method === "DELETE") {
      const id = decodeURIComponent(m[1]!);
      list = list.filter((f) => f.doc_id !== id);
      return Promise.resolve(new Response(null, { status: 204 }));
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderTree(fetchMock: ReturnType<typeof server>) {
  vi.stubGlobal("fetch", fetchMock);
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

/** Waits until the favorites have loaded: the star is enabled then. */
async function starReady(id: string) {
  const star = await screen.findByTestId(`tree-star-${id}`);
  await waitFor(() => expect(star).not.toBeDisabled());
  return star;
}

describe("DocTree — favorites", () => {
  it("viewer sees star and menu item", async () => {
    renderTree(server({ role: "viewer" }));
    await screen.findByText("a@x.test");
    const star = await starReady("alpha");
    expect(star).toHaveAccessibleName("Add to favorites");
    expect(star).toHaveAttribute("aria-pressed", "false");
    expect(screen.queryByRole("button", { name: "More" })).toBeNull();

    fireEvent.contextMenu(screen.getByTestId("doc-row-alpha"));
    const menu = screen.getByTestId("context-menu");
    const item = within(menu).getByTestId("ctx-favorite");
    expect(item).toHaveTextContent("Add to favorites");
    expect(within(menu).getAllByRole("menuitem")).toHaveLength(1);
    expect(screen.queryByTestId("ctx-rename")).toBeNull();
    expect(screen.queryByTestId("ctx-delete")).toBeNull();
  });

  it("puts Add to favorites first in an editor's row menu", async () => {
    renderTree(server({ role: "editor" }));
    await screen.findByText("a@x.test");
    await starReady("alpha");
    await waitFor(() => expect(screen.getAllByRole("button", { name: "More" }).length).toBeGreaterThan(0));

    fireEvent.contextMenu(screen.getByTestId("doc-row-alpha"));
    const items = within(screen.getByTestId("context-menu")).getAllByRole("menuitem");
    expect(items[0]).toHaveAttribute("data-testid", "ctx-favorite");
    expect(screen.getByTestId("ctx-rename")).toBeInTheDocument();
  });

  it("starring from the tree adds to favorites", async () => {
    const fetchMock = server({ role: "viewer" });
    renderTree(fetchMock);
    await screen.findByText("a@x.test");
    const star = await starReady("beta");
    expect(screen.queryByTestId("favorites")).toBeNull();

    fireEvent.click(star);

    expect(await screen.findByTestId("fav-row-beta")).toBeInTheDocument();
    expect(screen.getByTestId("fav-link-beta")).toHaveTextContent("Beta");
    expect(star).toHaveAttribute("aria-pressed", "true");
    expect(star).toHaveAccessibleName("Remove from favorites");
    await waitFor(() =>
      expect(fetchMock.mock.calls.some(([u, i]) => u === "/api/me/favorites/beta" && i?.method === "PUT")).toBe(true),
    );
  });

  it("unstarring from the context menu removes it from favorites", async () => {
    renderTree(server({ role: "viewer", favorites: [fav("alpha", "Alpha")] }));
    await screen.findByText("a@x.test");
    await screen.findByTestId("fav-row-alpha");

    fireEvent.contextMenu(screen.getByTestId("doc-row-alpha"));
    const item = screen.getByTestId("ctx-favorite");
    expect(item).toHaveTextContent("Remove from favorites");
    fireEvent.click(item);

    await waitFor(() => expect(screen.queryByTestId("fav-row-alpha")).toBeNull());
    expect(screen.getByTestId("tree-star-alpha")).toHaveAttribute("aria-pressed", "false");
  });

  it("star button is revealed on keyboard focus", async () => {
    renderTree(server({ role: "viewer", favorites: [fav("alpha")] }));
    await screen.findByText("a@x.test");
    await screen.findByTestId("fav-row-alpha");

    const unstarred = screen.getByTestId("tree-star-beta").className.split(/\s+/);
    expect(unstarred).toContain("opacity-0");
    expect(unstarred).toContain("group-hover:opacity-100");
    expect(unstarred).toContain("group-focus-within:opacity-100");
    expect(unstarred).toContain("focus-visible:opacity-100");

    const starred = screen.getByTestId("tree-star-alpha").className.split(/\s+/);
    expect(starred).toContain("opacity-100");
    expect(starred).not.toContain("opacity-0");
  });

  it("star is disabled while favorites load", async () => {
    const gate = new Promise<unknown>(() => {});
    renderTree(server({ role: "viewer", favoritesGate: gate }));
    await screen.findByText("a@x.test");

    const star = await screen.findByTestId("tree-star-alpha");
    expect(star).toBeDisabled();
    fireEvent.contextMenu(screen.getByTestId("doc-row-alpha"));
    expect(screen.getByTestId("ctx-favorite")).toBeDisabled();
  });

  it("offers no star on a template, which can't be a favorite", async () => {
    renderTree(server({ role: "viewer" }));
    await screen.findByText("a@x.test");
    await starReady("alpha");

    expect(screen.getByTestId("doc-row-tpl")).toBeInTheDocument();
    expect(screen.queryByTestId("tree-star-tpl")).toBeNull();
  });

  it("no duplicate ids", async () => {
    renderTree(server({ role: "editor", favorites: [fav("beta")] }));
    await screen.findByText("a@x.test");
    await screen.findByTestId("fav-row-beta");

    expect(document.querySelectorAll('[data-testid="doc-row-beta"]')).toHaveLength(1);
    expect(document.querySelectorAll('[data-testid="tree-row-beta"]')).toHaveLength(1);
    const ids = [...document.querySelectorAll("[id]")].map((el) => el.id);
    expect(ids.filter((id, i) => ids.indexOf(id) !== i)).toEqual([]);
  });

  it("renders the Favorites section above the Documents header, outside the tree", async () => {
    renderTree(server({ role: "viewer", favorites: [fav("beta")] }));
    await screen.findByText("a@x.test");
    const section = await screen.findByTestId("favorites");
    const documents = screen.getByText("Documents");

    expect(section.compareDocumentPosition(documents) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.getByTestId("doc-tree").contains(section)).toBe(true);
    expect(section.closest("[data-testid^='tree-row-']")).toBeNull();
    expect(section.closest("ul")).toBeNull();
  });
});
