/**
 * The sidebar's writes when the server refuses them for a rate limit or a
 * quota (issue #88): each shows the limit's own text as one warning in
 * place of its generic error.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { useUi } from "../../stores/ui";
import { DocTree } from "./DocTree";

const TOO_FAST = { status: 429, code: "rate.limited", details: { retry_after_secs: 7 } };
const TOO_FAST_TEXT = "You're doing that too fast. Try again in 7 s.";
const PAGE_QUOTA = { status: 403, code: "quota.exceeded", details: { quota: "pages", scope: "user", limit: 50 } };
const PAGE_QUOTA_TEXT = "You've reached your page limit.";

type Refusal = { status: number; code: string; details: Record<string, unknown> };

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

function doc(id: string, title: string, parent_id: string | null, sort_key: string, is_template = false) {
  return {
    id,
    workspace_id: "w1",
    parent_id,
    title,
    sort_key,
    icon: null,
    created_by: "u1",
    archived: false,
    is_template,
    archived_at: null,
    archive_root_id: null,
  };
}

/** Two top-level pages and a template; every write is refused with `refusal`. */
function server(refusal: Refusal, role: "owner" | "editor") {
  return vi.fn((url: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role, archive_retention_days: 30 });
    }
    if (url === "/api/docs" && method === "GET") {
      return json([doc("alpha", "Alpha", null, "a"), doc("beta", "Beta", null, "b")]);
    }
    if (url === "/api/workspace/templates") return json([doc("tpl", "Meeting notes", null, "t", true)]);
    if (method !== "GET") {
      return json({ error: { code: refusal.code, message: refusal.code, details: refusal.details } }, refusal.status);
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderTree(refusal: Refusal, role: "owner" | "editor" = "editor") {
  vi.stubGlobal("fetch", server(refusal, role));
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

async function editorRows() {
  await waitFor(() => expect(screen.getAllByRole("button", { name: "More" })).toHaveLength(2));
}

function openMenu(id: string): HTMLElement {
  fireEvent.click(within(screen.getByTestId(`tree-row-${id}`)).getByRole("button", { name: "More" }));
  return screen.getByTestId("context-menu");
}

async function expectLimitToast(text: string) {
  await waitFor(() => expect(useUi.getState().toasts).toMatchObject([{ kind: "warn", text, key: "limit" }]));
}

beforeEach(() => {
  useUi.setState({ toasts: [] });
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("DocTree when a limit refuses a write", () => {
  it("names the page quota when a new page is refused", async () => {
    renderTree(PAGE_QUOTA);
    await editorRows();
    fireEvent.click(screen.getByTestId("new-doc"));
    fireEvent.click(await screen.findByTestId("new-doc-blank"));
    await expectLimitToast(PAGE_QUOTA_TEXT);
  });

  it("names the page quota when a page from a template is refused", async () => {
    renderTree(PAGE_QUOTA);
    await editorRows();
    fireEvent.click(screen.getByTestId("new-doc"));
    fireEvent.click(await screen.findByTestId("template-card-tpl"));
    await expectLimitToast(PAGE_QUOTA_TEXT);
  });

  it("says to slow down when a rename is refused", async () => {
    vi.spyOn(window, "prompt").mockReturnValue("Renamed");
    renderTree(TOO_FAST);
    await editorRows();
    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-rename"));
    await expectLimitToast(TOO_FAST_TEXT);
  });

  it("says to slow down when moving to the archive is refused", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    renderTree(TOO_FAST);
    await editorRows();
    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-delete"));
    await expectLimitToast(TOO_FAST_TEXT);
  });

  it("says to slow down when saving as a template is refused", async () => {
    renderTree(TOO_FAST, "owner");
    await editorRows();
    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-template"));
    await expectLimitToast(TOO_FAST_TEXT);
  });

  it("says to slow down when a move is refused", async () => {
    renderTree(TOO_FAST);
    await editorRows();
    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    await expectLimitToast(TOO_FAST_TEXT);
  });
});
