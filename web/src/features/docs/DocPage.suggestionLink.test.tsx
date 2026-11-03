/**
 * A `?suggestion=` link focuses its card, and leaving the page drops that
 * focus, so coming back does not scroll to an old highlight or open
 * Resolved again. Rendered under StrictMode, which runs each new component's
 * effects, their cleanups and the effects again: the drop must not undo the
 * link it is meant to outlive.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, render, waitFor } from "@testing-library/react";
import { StrictMode } from "react";
import { createMemoryRouter, RouterProvider } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

vi.mock("../editor/KnotEditor", () => ({
  KnotEditor: () => <div data-testid="editor" />,
}));

import DocPage from "./DocPage";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

const DOC = {
  id: "d1",
  workspace_id: "w1",
  parent_id: null,
  title: "Runbook",
  sort_key: "a",
  icon: null,
  created_by: "u1",
  archived: false,
  is_template: false,
  effective_role: "owner",
  archived_at: null,
  archived_by: null,
  purge_at: null,
  archive_root_id: null,
  archive_root_title: null,
};

function server() {
  return vi.fn((url: string) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role: "owner" });
    }
    if (url === "/api/docs/d1") return json(DOC);
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

beforeEach(() => {
  useUi.setState({
    commentSidebarOpen: false,
    sidebarTab: "comments",
    activeSuggestionId: null,
    suggestionLink: null,
    pendingSuggestion: null,
  });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("DocPage — suggestion link", () => {
  it("focuses the linked card and drops the focus on leaving the page", async () => {
    vi.stubGlobal("fetch", server());
    const router = createMemoryRouter(
      [
        { path: "/", element: <p>home</p> },
        { path: "/doc/:id", element: <DocPage /> },
      ],
      { initialEntries: ["/doc/d1?suggestion=s1"] },
    );
    render(
      <StrictMode>
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <SessionProvider>
            <RouterProvider router={router} />
          </SessionProvider>
        </QueryClientProvider>
      </StrictMode>,
    );

    await waitFor(() => expect(useUi.getState().suggestionLink).toEqual({ suggestionId: "s1" }));
    expect(useUi.getState().activeSuggestionId).toBe("s1");
    expect(useUi.getState().sidebarTab).toBe("suggestions");
    await waitFor(() => expect(router.state.location.search).toBe(""));
    // Still focused once the link param is gone and every effect has run.
    expect(useUi.getState().activeSuggestionId).toBe("s1");

    await act(() => router.navigate("/"));
    expect(useUi.getState().activeSuggestionId).toBeNull();
    expect(useUi.getState().suggestionLink).toBeNull();
  });
});
