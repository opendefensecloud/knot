import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Doc, Favorite } from "../../lib/validators";
import { useUi } from "../../stores/ui";

import { FavoritesSection } from "./FavoritesSection";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  try {
    localStorage.clear();
  } catch {
    // Some tests make localStorage throw.
  }
});

beforeEach(() => {
  useUi.setState({ toasts: [] });
});

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

function fav(id: string, title = id.toUpperCase()): Favorite {
  return { doc_id: id, title, icon: null, favorited_at: "2026-11-14T10:00:00Z" };
}

function doc(id: string, title: string, extra: Partial<Doc> = {}): Doc {
  return {
    id,
    workspace_id: "w1",
    parent_id: null,
    title,
    sort_key: "m",
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
    restricted: false,
    within_restricted: false,
    parent_hidden: false,
    effective_role: "viewer",
    ...extra,
  };
}

/** A fake `/api/me/favorites` holding the list. */
function server(initial: Favorite[]) {
  const answer = (url: string, init?: RequestInit): Response => {
    const method = init?.method ?? "GET";
    if (url === "/api/me/favorites" && method === "GET") return json(s.list);
    const m = /^\/api\/me\/favorites\/([^/]+)$/.exec(url);
    if (m && method === "DELETE") {
      const id = decodeURIComponent(m[1] ?? "");
      s.list = s.list.filter((f) => f.doc_id !== id);
      return new Response(null, { status: 204 });
    }
    if (m && method === "PUT") return new Response(null, { status: 204 });
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  };
  const s = {
    list: [...initial],
    fetch: vi.fn((url: string, init?: RequestInit) => Promise.resolve(answer(url, init))),
  };
  return s;
}

function Where() {
  const loc = useLocation();
  return <div data-testid="where">{loc.pathname}</div>;
}

function renderSection(docs: Doc[] | undefined, at = "/") {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const ui = (d: Doc[] | undefined) => (
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[at]}>
        <FavoritesSection docs={d} />
        <Where />
      </MemoryRouter>
    </QueryClientProvider>
  );
  const r = render(ui(docs));
  return { ...r, rerenderWith: (d: Doc[] | undefined) => r.rerender(ui(d)) };
}

const rowIds = () =>
  [...document.querySelectorAll("[data-testid^='fav-row-']")].map((el) =>
    el.getAttribute("data-testid")!.slice("fav-row-".length),
  );

describe("FavoritesSection", () => {
  it("renders favorites in order with fav-link testids", async () => {
    vi.stubGlobal("fetch", server([fav("b"), fav("a"), fav("c")]).fetch);
    renderSection([doc("a", "Alpha"), doc("b", "Beta"), doc("c", "Gamma")]);

    const section = await screen.findByTestId("favorites");
    expect(within(section).getByRole("heading", { name: "Favorites" })).toBeInTheDocument();
    expect(rowIds()).toEqual(["b", "a", "c"]);
    const link = screen.getByTestId("fav-link-b");
    expect(link).toHaveAttribute("href", "/doc/b");
    expect(link).toHaveTextContent("Beta");
    expect(screen.getByTestId("fav-unstar-b")).toHaveAccessibleName("Remove from favorites");
  });

  it("uses the API's titles until the docs list has loaded", async () => {
    vi.stubGlobal("fetch", server([fav("a", "From API")]).fetch);
    renderSection(undefined);

    expect(await screen.findByTestId("fav-link-a")).toHaveTextContent("From API");
  });

  it("hidden when there are no visible favorites", async () => {
    const s = server([]);
    vi.stubGlobal("fetch", s.fetch);
    renderSection([doc("a", "Alpha")]);
    await waitFor(() => expect(s.fetch).toHaveBeenCalled());
    // Give the (empty) list time to arrive.
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.queryByTestId("favorites")).toBeNull();
  });

  it("hidden when every favorite is missing from the docs list", async () => {
    const s = server([fav("x"), fav("y")]);
    vi.stubGlobal("fetch", s.fetch);
    renderSection([doc("a", "Alpha")]);
    await waitFor(() => expect(s.fetch).toHaveBeenCalled());
    await new Promise((r) => setTimeout(r, 20));
    expect(screen.queryByTestId("favorites")).toBeNull();
  });

  it("favorite title follows the docs list", async () => {
    vi.stubGlobal("fetch", server([fav("a", "Old"), fav("gone"), fav("b")]).fetch);
    const { rerenderWith } = renderSection([doc("a", "New"), doc("b", "Beta")]);

    expect(await screen.findByTestId("fav-link-a")).toHaveTextContent("New");
    expect(screen.queryByTestId("fav-row-gone")).toBeNull();
    expect(rowIds()).toEqual(["a", "b"]);

    // A rename lands in the docs list: the entry follows without a refetch.
    rerenderWith([doc("a", "Newer"), doc("b", "Beta")]);
    expect(screen.getByTestId("fav-link-a")).toHaveTextContent("Newer");
  });

  it("skips pages the docs list shows as archived or templates", async () => {
    vi.stubGlobal("fetch", server([fav("a"), fav("t"), fav("z")]).fetch);
    renderSection([doc("a", "Alpha"), doc("t", "Tpl", { is_template: true }), doc("z", "Zed", { archived: true })]);

    await screen.findByTestId("fav-row-a");
    expect(rowIds()).toEqual(["a"]);
  });

  it("collapse persists", async () => {
    vi.stubGlobal("fetch", server([fav("a")]).fetch);
    const first = renderSection([doc("a", "Alpha")]);

    const toggle = await screen.findByTestId("favorites-toggle");
    expect(toggle).toHaveAttribute("aria-expanded", "true");
    fireEvent.click(toggle);
    expect(screen.queryByTestId("fav-row-a")).toBeNull();
    expect(toggle).toHaveAttribute("aria-expanded", "false");
    expect(localStorage.getItem("knot.favoritesCollapsed")).toBe("1");
    first.unmount();

    renderSection([doc("a", "Alpha")]);
    expect(await screen.findByTestId("favorites-toggle")).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("fav-row-a")).toBeNull();

    fireEvent.click(screen.getByTestId("favorites-toggle"));
    expect(screen.getByTestId("fav-row-a")).toBeInTheDocument();
    expect(localStorage.getItem("knot.favoritesCollapsed")).not.toBe("1");
  });

  it("collapse works when localStorage throws", async () => {
    vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.spyOn(Storage.prototype, "removeItem").mockImplementation(() => {
      throw new Error("denied");
    });
    vi.stubGlobal("fetch", server([fav("a")]).fetch);
    renderSection([doc("a", "Alpha")]);

    fireEvent.click(await screen.findByTestId("favorites-toggle"));
    expect(screen.queryByTestId("fav-row-a")).toBeNull();
    fireEvent.click(screen.getByTestId("favorites-toggle"));
    expect(screen.getByTestId("fav-row-a")).toBeInTheDocument();
  });

  it("unstar from favorites keeps the current route", async () => {
    const s = server([fav("a"), fav("b")]);
    vi.stubGlobal("fetch", s.fetch);
    renderSection([doc("a", "Alpha"), doc("b", "Beta")], "/doc/a");

    fireEvent.click(await screen.findByTestId("fav-unstar-a"));
    await waitFor(() => expect(screen.queryByTestId("fav-row-a")).toBeNull());
    expect(screen.getByTestId("where")).toHaveTextContent("/doc/a");
    await waitFor(() =>
      expect(s.fetch.mock.calls.some(([u, i]) => u === "/api/me/favorites/a" && i?.method === "DELETE")).toBe(true),
    );
    expect(screen.getByTestId("where")).toHaveTextContent("/doc/a");
    expect(rowIds()).toEqual(["b"]);
  });

  it("open favorite has selected style", async () => {
    vi.stubGlobal("fetch", server([fav("a"), fav("b")]).fetch);
    renderSection([doc("a", "Alpha"), doc("b", "Beta")], "/doc/b");

    const open = await screen.findByTestId("fav-row-b");
    expect(open.className).toContain("bg-muted");
    expect(screen.getByTestId("fav-link-b")).toHaveAttribute("aria-current", "page");
    expect(screen.getByTestId("fav-row-a").className.split(/\s+/)).not.toContain("bg-muted");
  });

  it("unstar button is revealed on hover and keyboard focus", async () => {
    vi.stubGlobal("fetch", server([fav("a")]).fetch);
    renderSection([doc("a", "Alpha")]);

    const btn = await screen.findByTestId("fav-unstar-a");
    expect(btn.className).toContain("group-hover:opacity-100");
    expect(btn.className).toContain("group-focus-within:opacity-100");
    expect(btn.className).toContain("focus-visible:opacity-100");
  });
});
