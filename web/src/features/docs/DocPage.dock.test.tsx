import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { useLayoutEffect, type MutableRefObject, type ReactNode } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";
import { restorePlatform, stubPlatform } from "../../test/platform";
import type { KnotEditorHandle } from "../editor/readingCaret";

// What DocPage may ask of the editor, as spies: where the caret lands is the
// editor's business (readingCaret.test.ts); whether DocPage asks is its own.
const editorHandle = vi.hoisted(() => ({
  placeCaretAtReadingLine: vi.fn(),
  revealCaretBelowBand: vi.fn(),
}));

// The real editor needs a WebSocket. The stand-in shows what DocPage gave it,
// including the controls it hands over for the end of the toolbar, and has a
// focusable .ProseMirror so a test can put focus "in the text".
vi.mock("../editor/KnotEditor", () => ({
  KnotEditor: (props: {
    editMode: boolean;
    toolbarTrailing?: ReactNode;
    handleRef?: MutableRefObject<KnotEditorHandle | null>;
  }) => {
    const { handleRef } = props;
    useLayoutEffect(() => {
      if (!handleRef) return;
      handleRef.current = editorHandle;
      return () => { handleRef.current = null; };
    }, [handleRef]);
    return (
      <div data-testid="editor" data-edit-mode={String(props.editMode)}>
        <div data-testid="mock-prose" className="ProseMirror" tabIndex={0} />
        {props.toolbarTrailing && <div data-testid="mock-trailing">{props.toolbarTrailing}</div>}
      </div>
    );
  },
}));

import DocPage from "./DocPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.clearAllMocks();
  restorePlatform();
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

  it("ignores a held-down Ctrl+E, without handing it to the browser", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");

    // fireEvent returns false once a handler called preventDefault.
    expect(fireEvent.keyDown(window, { key: "e", ctrlKey: true, repeat: true })).toBe(false);
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "false");
  });

  it("toggles with ⌘E on a Mac, and leaves Ctrl+E (end of line) to the editor", async () => {
    stubPlatform("MacIntel");
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    const editor = await screen.findByTestId("editor");

    fireEvent.keyDown(window, { key: "e", metaKey: true });
    expect(editor).toHaveAttribute("data-edit-mode", "true");
    expect(screen.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-label", "Stop editing (⌘E)");
    expect(screen.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-keyshortcuts", "Meta+E");

    // Not claimed: no preventDefault, and still editing.
    expect(fireEvent.keyDown(screen.getByTestId("mock-prose"), { key: "e", ctrlKey: true })).toBe(true);
    expect(editor).toHaveAttribute("data-edit-mode", "true");
  });

  it("leaves Meta+E to the system outside Apple platforms", async () => {
    stubPlatform("Linux x86_64");
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    const editor = await screen.findByTestId("editor");

    expect(fireEvent.keyDown(window, { key: "e", metaKey: true })).toBe(true);
    expect(editor).toHaveAttribute("data-edit-mode", "false");
    expect(screen.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-label", "Edit (Ctrl+E)");
    expect(screen.getByTestId("toggle-edit-mode")).toHaveAttribute("aria-keyshortcuts", "Control+E");
  });

  it("entering edit mode at the top puts the caret in the text", async () => {
    stubObserver();
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");

    // From nowhere in particular (focus on <body>), by keyboard.
    fireEvent.keyDown(window, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "true");
    expect(editorHandle.placeCaretAtReadingLine).toHaveBeenCalledOnce();

    // Leaving never places a caret.
    fireEvent.keyDown(window, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "false");
    expect(editorHandle.placeCaretAtReadingLine).toHaveBeenCalledOnce();

    // From the header's Edit button.
    const toggle = screen.getByTestId("toggle-edit-mode");
    toggle.focus();
    fireEvent.click(toggle);
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "true");
    expect(editorHandle.placeCaretAtReadingLine).toHaveBeenCalledTimes(2);

    // At the top there is no band over the text to clear.
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(editorHandle.revealCaretBelowBand).not.toHaveBeenCalled();
  });

  it("entering edit mode mid-document clears the band over the caret, even with no reading position held", async () => {
    // jsdom has no layout, so DocPage finds no reading anchor here: the case
    // where a page is condensed but the anchor could not be captured. The
    // band is there all the same, and the caret just placed can sit under it
    // once the toolbar grows.
    const observer = stubObserver();
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    observer.scrollPast();

    fireEvent.click(within(screen.getByTestId("doc-dock")).getByTestId("dock-toggle-edit-mode"));
    expect(editorHandle.placeCaretAtReadingLine).toHaveBeenCalledOnce();
    await new Promise((resolve) => setTimeout(resolve, 50));
    expect(editorHandle.revealCaretBelowBand).toHaveBeenCalledOnce();
  });

  it("entering edit mode from the title keeps focus in the title", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    const title = screen.getByTestId("doc-title");
    title.focus();

    fireEvent.keyDown(title, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "true");
    expect(editorHandle.placeCaretAtReadingLine).not.toHaveBeenCalled();
    expect(title).toHaveFocus();
  });

  it("entering edit mode from a link outside the page, such as the tree, puts the caret in the text", async () => {
    // Opening a page from the tree leaves focus on the row's link; the
    // shortcut from there must still land in the text.
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    const treeLink = document.body.appendChild(document.createElement("a"));
    treeLink.href = "/doc/d2";
    treeLink.textContent = "Another page";
    treeLink.focus();

    fireEvent.keyDown(treeLink, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "true");
    expect(editorHandle.placeCaretAtReadingLine).toHaveBeenCalledOnce();
    treeLink.remove();
  });

  it("entering edit mode from inside a dialog keeps focus in the dialog", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    const dialog = document.body.appendChild(document.createElement("div"));
    dialog.setAttribute("role", "dialog");
    const dialogButton = dialog.appendChild(document.createElement("button"));
    dialogButton.focus();

    fireEvent.keyDown(dialogButton, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "true");
    expect(editorHandle.placeCaretAtReadingLine).not.toHaveBeenCalled();
    expect(dialogButton).toHaveFocus();
    dialog.remove();
  });

  it("leaving edit mode from the text hands focus to the Edit toggle", async () => {
    vi.stubGlobal("fetch", server("editor"));
    renderDoc();
    await screen.findByTestId("editor");
    fireEvent.click(screen.getByTestId("toggle-edit-mode"));
    const prose = screen.getByTestId("mock-prose");
    prose.focus();

    fireEvent.keyDown(prose, { key: "e", ctrlKey: true });
    expect(screen.getByTestId("editor")).toHaveAttribute("data-edit-mode", "false");
    expect(screen.getByTestId("toggle-edit-mode")).toHaveFocus();
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
