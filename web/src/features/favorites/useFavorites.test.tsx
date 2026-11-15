import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, renderHook, waitFor } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Favorite } from "../../lib/validators";
import { useUi } from "../../stores/ui";

import { useFavorites } from "./useFavorites";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

beforeEach(() => {
  useUi.setState({ toasts: [] });
});

function json(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

function noContent() {
  return new Response(null, { status: 204 });
}

function fav(id: string, title = id.toUpperCase()): Favorite {
  return { doc_id: id, title, icon: null, favorited_at: "2026-11-14T10:00:00Z" };
}

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => { resolve = r; });
  return { promise, resolve };
}

/** A fake `/api/me/favorites` holding the list. `hold` makes the next
 *  PUT/DELETE wait until released; `fail` answers it with an error. */
function server(initial: Favorite[]) {
  const s = {
    list: [...initial],
    calls: [] as string[],
    hold: null as ReturnType<typeof deferred<void>> | null,
    fail: null as { status: number; code: string; details?: Record<string, unknown> } | null,
    fetch: vi.fn(async (url: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      s.calls.push(`${method} ${url}`);
      if (url === "/api/me/favorites" && method === "GET") return json(s.list);
      const m = /^\/api\/me\/favorites\/([^/]+)$/.exec(url);
      if (m && (method === "PUT" || method === "DELETE")) {
        const id = decodeURIComponent(m[1] ?? "");
        const hold = s.hold;
        s.hold = null;
        if (hold) await hold.promise;
        const fail = s.fail;
        s.fail = null;
        if (fail) {
          return json({ error: { code: fail.code, message: "", details: fail.details ?? {} } }, fail.status);
        }
        if (method === "PUT" && !s.list.some((f) => f.doc_id === id)) s.list.push(fav(id));
        if (method === "DELETE") s.list = s.list.filter((f) => f.doc_id !== id);
        return noContent();
      }
      return json({ error: { code: "not_found", message: url, details: {} } }, 404);
    }),
  };
  return s;
}

function renderFavorites() {
  const client = new QueryClient({ defaultOptions: { queries: { retry: false }, mutations: { retry: false } } });
  const wrapper = ({ children }: { children: ReactNode }) => (
    <QueryClientProvider client={client}>{children}</QueryClientProvider>
  );
  return renderHook(() => useFavorites(), { wrapper });
}

const ids = (favorites: Favorite[] | undefined) => favorites?.map((f) => f.doc_id);

describe("useFavorites", () => {
  it("lists favorites in server order", async () => {
    const s = server([fav("b"), fav("a"), fav("c")]);
    vi.stubGlobal("fetch", s.fetch);
    const { result } = renderFavorites();

    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(ids(result.current.favorites)).toEqual(["b", "a", "c"]);
    expect(result.current.favorites?.[0]?.title).toBe("B");
    expect(result.current.isFavorite("a")).toBe(true);
    expect(result.current.isFavorite("z")).toBe(false);
    expect([...result.current.favoriteIds]).toEqual(["b", "a", "c"]);
  });

  it("ready is false until the first response", async () => {
    const s = server([fav("a")]);
    const gate = deferred<void>();
    vi.stubGlobal("fetch", vi.fn(async (url: string, init?: RequestInit) => {
      await gate.promise;
      return s.fetch(url, init);
    }));
    const { result } = renderFavorites();

    expect(result.current.ready).toBe(false);
    expect(result.current.favorites).toBeUndefined();
    expect(result.current.isFavorite("a")).toBe(false);

    gate.resolve();
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(result.current.isFavorite("a")).toBe(true);
  });

  it("toggle stars optimistically and appends", async () => {
    const s = server([fav("a")]);
    vi.stubGlobal("fetch", s.fetch);
    const { result } = renderFavorites();
    await waitFor(() => expect(result.current.ready).toBe(true));

    s.hold = deferred<void>();
    const hold = s.hold;
    act(() => result.current.toggle({ id: "n", title: "New page", icon: "📄" }));

    await waitFor(() => expect(result.current.isFavorite("n")).toBe(true));
    expect(ids(result.current.favorites)).toEqual(["a", "n"]);
    expect(result.current.favorites?.at(-1)).toMatchObject({ doc_id: "n", title: "New page", icon: "📄" });
    expect(s.calls).toContain("PUT /api/me/favorites/n");

    hold.resolve();
    await waitFor(() => expect(s.calls.filter((c) => c === "GET /api/me/favorites")).toHaveLength(2));
    await waitFor(() => expect(result.current.favorites?.at(-1)?.title).toBe("N"));
    expect(result.current.isFavorite("n")).toBe(true);
  });

  it("toggle unstars optimistically", async () => {
    const s = server([fav("a"), fav("b")]);
    vi.stubGlobal("fetch", s.fetch);
    const { result } = renderFavorites();
    await waitFor(() => expect(result.current.ready).toBe(true));

    s.hold = deferred<void>();
    const hold = s.hold;
    act(() => result.current.toggle({ id: "a", title: "A", icon: null }));

    await waitFor(() => expect(result.current.isFavorite("a")).toBe(false));
    expect(ids(result.current.favorites)).toEqual(["b"]);
    expect(s.calls).toContain("DELETE /api/me/favorites/a");

    hold.resolve();
    await waitFor(() => expect(s.calls.filter((c) => c === "GET /api/me/favorites")).toHaveLength(2));
    expect(ids(result.current.favorites)).toEqual(["b"]);
  });

  it("star rolls back on server error", async () => {
    const s = server([fav("a"), fav("b")]);
    vi.stubGlobal("fetch", s.fetch);
    const { result } = renderFavorites();
    await waitFor(() => expect(result.current.ready).toBe(true));

    s.hold = deferred<void>();
    const hold = s.hold;
    s.fail = { status: 409, code: "favorite.limit_reached", details: { limit: 200 } };
    act(() => result.current.toggle({ id: "n", title: "N", icon: null }));
    await waitFor(() => expect(result.current.isFavorite("n")).toBe(true));

    hold.resolve();
    await waitFor(() => expect(result.current.isFavorite("n")).toBe(false));
    expect(ids(result.current.favorites)).toEqual(["a", "b"]);
    expect(useUi.getState().toasts.map((t) => t.text)).toEqual([
      "You have 200 favorites, the most allowed. Remove one to add another.",
    ]);
  });

  it("unstar rolls back on server error, back in its place", async () => {
    const s = server([fav("a"), fav("b"), fav("c")]);
    vi.stubGlobal("fetch", s.fetch);
    const { result } = renderFavorites();
    await waitFor(() => expect(result.current.ready).toBe(true));

    s.hold = deferred<void>();
    const hold = s.hold;
    s.fail = { status: 500, code: "internal", details: {} };
    act(() => result.current.toggle({ id: "b", title: "B", icon: null }));
    await waitFor(() => expect(result.current.isFavorite("b")).toBe(false));

    hold.resolve();
    await waitFor(() => expect(result.current.isFavorite("b")).toBe(true));
    expect(ids(result.current.favorites)).toEqual(["a", "b", "c"]);
    expect(useUi.getState().toasts.map((t) => t.text)).toEqual(["Couldn't remove from favorites"]);
  });

  it("toggles are sent one at a time, in the order they were made", async () => {
    const s = server([]);
    vi.stubGlobal("fetch", s.fetch);
    const { result } = renderFavorites();
    await waitFor(() => expect(result.current.ready).toBe(true));

    s.hold = deferred<void>();
    const hold = s.hold;
    act(() => result.current.toggle({ id: "n", title: "N", icon: null }));
    await waitFor(() => expect(result.current.isFavorite("n")).toBe(true));
    act(() => result.current.toggle({ id: "n", title: "N", icon: null }));
    await waitFor(() => expect(result.current.isFavorite("n")).toBe(false));

    // The DELETE waits for the PUT before it.
    expect(s.calls.filter((c) => c !== "GET /api/me/favorites")).toEqual(["PUT /api/me/favorites/n"]);

    hold.resolve();
    await waitFor(() =>
      expect(s.calls.filter((c) => c !== "GET /api/me/favorites")).toEqual([
        "PUT /api/me/favorites/n",
        "DELETE /api/me/favorites/n",
      ]),
    );
    await waitFor(() => expect(s.calls.at(-1)).toBe("GET /api/me/favorites"));
    expect(result.current.isFavorite("n")).toBe(false);
    expect(s.list).toEqual([]);
  });

  it("toggle does nothing before favorites have loaded", async () => {
    const s = server([fav("a")]);
    const gate = deferred<void>();
    vi.stubGlobal("fetch", vi.fn(async (url: string, init?: RequestInit) => {
      await gate.promise;
      return s.fetch(url, init);
    }));
    const { result } = renderFavorites();

    act(() => result.current.toggle({ id: "a", title: "A", icon: null }));
    gate.resolve();
    await waitFor(() => expect(result.current.ready).toBe(true));
    expect(s.calls).toEqual(["GET /api/me/favorites"]);
    expect(result.current.isFavorite("a")).toBe(true);
  });
});
