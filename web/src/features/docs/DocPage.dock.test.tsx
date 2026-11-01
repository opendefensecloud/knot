import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import type { ReactNode } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";

// The real editor needs a WebSocket. The stand-in shows what DocPage gave it,
// including the controls it hands over for the end of the toolbar.
vi.mock("../editor/KnotEditor", () => ({
  KnotEditor: (props: { editMode: boolean; toolbarTrailing?: ReactNode }) => (
    <div data-testid="editor" data-edit-mode={String(props.editMode)}>
      {props.toolbarTrailing && <div data-testid="mock-trailing">{props.toolbarTrailing}</div>}
    </div>
  ),
}));

import DocPage from "./DocPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  window.sessionStorage.clear();
  useUi.setState({ commentSidebarOpen: false });
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

type Role = "owner" | "editor" | "viewer";

function docBody(role: Role, archived = false) {
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

function server(role: Role, archived = false) {
  return vi.fn((url: string) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
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
    if (url.startsWith("/api/docs/d1/comments")) return json([]);
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

/** An IntersectionObserver that reports the action row on demand. */
function stubObserver() {
  const callbacks: Array<(entries: unknown[]) => void> = [];
  vi.stubGlobal(
    "IntersectionObserver",
    class {
      constructor(cb: (entries: unknown[]) => void) {
        callbacks.push(cb);
      }
      observe() {}
      disconnect() {}
    },
  );
  const report = (scrolledPast: boolean) =>
    act(() => {
      for (const cb of callbacks) {
        cb([
          {
            isIntersecting: !scrolledPast,
            boundingClientRect: { bottom: scrolledPast ? -40 : 200 },
            rootBounds: { top: 0 },
          },
        ]);
      }
    });
  return { scrollPast: () => report(true), scrollBack: () => report(false) };
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

describe("DocPage — doc dock", () => {
  it("shows Edit and Comments once the action row scrolls away, and hands them to the toolbar while editing", async () => {
    const observer = stubObserver();
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    expect(screen.queryByTestId("doc-dock")).toBeNull();

    observer.scrollPast();
    const dock = screen.getByTestId("doc-dock");
    expect(within(dock).getByTestId("dock-toggle-edit-mode")).toBeInTheDocument();
    expect(within(dock).getByTestId("dock-open-comments")).toBeInTheDocument();

    fireEvent.click(within(dock).getByTestId("dock-toggle-edit-mode"));
    expect(screen.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "true");
    expect(screen.queryByTestId("doc-dock")).toBeNull();
    expect(within(screen.getByTestId("mock-trailing")).getByTestId("dock-toggle-edit-mode")).toHaveAttribute(
      "aria-pressed",
      "true",
    );

    observer.scrollBack();
    expect(screen.queryByTestId("doc-dock")).toBeNull();
  });

  it("toggles the comment rail from the dock", async () => {
    const observer = stubObserver();
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    observer.scrollPast();

    fireEvent.click(screen.getByTestId("dock-open-comments"));
    expect(useUi.getState().commentSidebarOpen).toBe(true);
    expect(screen.getByTestId("dock-open-comments")).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(screen.getByTestId("dock-open-comments"));
    expect(useUi.getState().commentSidebarOpen).toBe(false);
  });

  it("gives viewers Comments only", async () => {
    const observer = stubObserver();
    vi.stubGlobal("fetch", server("viewer"));
    renderDoc();
    await screen.findByTestId("editor");
    observer.scrollPast();

    const dock = screen.getByTestId("doc-dock");
    expect(within(dock).queryByTestId("dock-toggle-edit-mode")).toBeNull();
    expect(within(dock).getByTestId("dock-open-comments")).toBeInTheDocument();
  });

  it("shows no dock on an archived page", async () => {
    const observer = stubObserver();
    vi.stubGlobal("fetch", server("editor", true));
    renderDoc();
    await screen.findByTestId("archived-banner");
    observer.scrollPast();
    expect(screen.queryByTestId("doc-dock")).toBeNull();
  });

  it("offers no Edit in the Markdown view", async () => {
    const observer = stubObserver();
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    fireEvent.click(screen.getByTestId("toggle-markdown"));
    observer.scrollPast();

    expect(screen.getByTestId("doc-dock")).toBeInTheDocument();
    expect(screen.queryByTestId("dock-toggle-edit-mode")).toBeNull();
  });

  it("announces a mode change the user made, and nothing on mount", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    expect(screen.getByTestId("edit-mode-announcer")).toHaveTextContent("");

    fireEvent.click(screen.getByTestId("toggle-edit-mode"));
    expect(screen.getByTestId("edit-mode-announcer")).toHaveTextContent("Editing");
    fireEvent.keyDown(window, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("edit-mode-announcer")).toHaveTextContent("Viewing");
  });

  it("ignores a held-down ⌘E, without handing it to the browser", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");

    // fireEvent returns false once a handler called preventDefault.
    expect(fireEvent.keyDown(window, { key: "e", metaKey: true, repeat: true })).toBe(false);
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "false");
  });

  it("leaves ⌘E to the browser for viewers", async () => {
    vi.stubGlobal("fetch", server("viewer"));
    renderDoc();
    await screen.findByTestId("editor");

    expect(fireEvent.keyDown(window, { key: "e", ctrlKey: true })).toBe(true);
  });

  it("shows no dock without IntersectionObserver", async () => {
    vi.stubGlobal("IntersectionObserver", undefined);
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    expect(screen.queryByTestId("doc-dock")).toBeNull();
  });
});
