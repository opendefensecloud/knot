import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

// The editor is irrelevant here and needs a WebSocket; stub it out.
vi.mock("../editor/KnotEditor", () => ({ KnotEditor: () => null }));

import DocPage from "./DocPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

/** A server that knows one doc, d1, which the signed-in user can only view. */
function viewerServer(url: string) {
  if (url === "/auth/session") {
    return json({
      user_id: "u-vic",
      email: "vic@example.test",
      display_name: "Vic",
      workspace_id: "w1",
      role: "viewer",
    });
  }
  if (url === "/api/docs/d1") {
    return json({
      id: "d1",
      workspace_id: "w1",
      parent_id: null,
      title: "Runbook",
      sort_key: "a",
      icon: null,
      created_by: "u-alice",
      archived: false,
      is_template: false,
      effective_role: "viewer",
    });
  }
  if (url === "/api/docs/d1/contributors") {
    return json({
      created_by: { id: "u-alice", display_name: "Alice" },
      created_at: "2026-03-03T10:00:00Z",
      contributors_since: "2026-03-03T10:00:00Z",
      contributors: [
        {
          user_id: "u-bob",
          display_name: "Bob",
          first_edited_at: "2026-10-01T09:00:00Z",
          last_edited_at: "2026-10-03T11:00:00Z",
        },
      ],
    });
  }
  return json({ error: { code: "not_found", message: url, details: {} } }, 404);
}

describe("DocPage byline", () => {
  it("shows the creator and contributors to a viewer, under the title", async () => {
    vi.stubGlobal("fetch", vi.fn((url: string) => viewerServer(url)));

    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <SessionProvider>
          <MemoryRouter initialEntries={["/docs/d1"]}>
            <Routes>
              <Route path="/docs/:id" element={<DocPage />} />
            </Routes>
          </MemoryRouter>
        </SessionProvider>
      </QueryClientProvider>,
    );

    const creator = await screen.findByTestId("doc-byline-creator");
    expect(creator).toHaveTextContent("Created by Alice");
    expect(await screen.findByTestId("doc-contributors-button")).toHaveTextContent("1 contributor");
    // Directly beneath the title, above the action row.
    const title = screen.getByTestId("doc-title");
    const byline = screen.getByTestId("doc-byline");
    expect(title.nextElementSibling).toBe(byline);
    expect(byline.nextElementSibling).toContainElement(screen.getByTestId("toggle-markdown"));
  });
});
