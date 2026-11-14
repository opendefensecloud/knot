/**
 * Ticking a task when the server refuses the write (issue #88): the
 * checkbox goes back and a toast says why, the limit's own text for a
 * rate limit.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { LIMIT_TOAST_KEY } from "../../lib/limitErrors";
import { useUi } from "../../stores/ui";
import TasksPage from "./TasksPage";

type Refusal = { status: number; code: string; details: Record<string, unknown> };

const TOO_FAST: Refusal = { status: 429, code: "rate.limited", details: { retry_after_secs: 7 } };
const NO_GRANT: Refusal = { status: 403, code: "acl.no_grant", details: {} };

const OPEN_TASK = {
  id: "t1",
  doc_id: "d1",
  doc_title: "Plan",
  item_index: 0,
  text: "Write the report",
  checked: false,
  completed_at: null,
  due_at: null,
  updated_at: "2026-11-12T10:00:00Z",
};

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

/** One open task; ticking it is refused with `refusal`. */
function server(refusal: Refusal) {
  return vi.fn((url: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    if (url === "/api/workspace/tasks" && method === "GET") return json([OPEN_TASK]);
    if (url === "/api/docs/d1/tasks/0" && method === "PATCH") {
      return json({ error: { code: refusal.code, message: refusal.code, details: refusal.details } }, refusal.status);
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderTasks(refusal: Refusal) {
  const fetchMock = server(refusal);
  vi.stubGlobal("fetch", fetchMock);
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <MemoryRouter>
        <TasksPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return fetchMock;
}

beforeEach(() => {
  useUi.setState({ toasts: [] });
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("TasksPage when ticking a task is refused", () => {
  it("unticks it again and shows the rate-limit text", async () => {
    const fetchMock = renderTasks(TOO_FAST);
    fireEvent.click(await screen.findByRole("button", { name: "Mark as done" }));

    await waitFor(() =>
      expect(useUi.getState().toasts).toEqual([
        expect.objectContaining({
          kind: "warn",
          text: "You're doing that too fast. Try again in 7 s.",
          key: LIMIT_TOAST_KEY,
        }),
      ]),
    );
    expect(fetchMock).toHaveBeenCalledWith("/api/docs/d1/tasks/0", expect.objectContaining({ method: "PATCH" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Mark as done" })).toBeInTheDocument());
    expect(screen.queryByRole("button", { name: "Mark as not done" })).toBeNull();
  });

  it("says the task wasn't updated for any other refusal", async () => {
    renderTasks(NO_GRANT);
    fireEvent.click(await screen.findByRole("button", { name: "Mark as done" }));

    await waitFor(() =>
      expect(useUi.getState().toasts).toEqual([
        expect.objectContaining({ kind: "error", text: "Couldn't update the task." }),
      ]),
    );
    await waitFor(() => expect(screen.getByRole("button", { name: "Mark as done" })).toBeInTheDocument());
  });
});
