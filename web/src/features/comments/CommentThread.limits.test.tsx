/**
 * Comment writes refused by a rate limit (issue #88) say so, in place of
 * their generic error.
 */

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Comment } from "../../lib/comments.api";
import { useUi } from "../../stores/ui";
import { CommentThread } from "./CommentThread";

const api = vi.hoisted(() => ({ resolve: vi.fn(), unresolve: vi.fn(), reply: vi.fn(), addReaction: vi.fn(), removeReaction: vi.fn() }));
vi.mock("../../lib/comments.api", () => ({ commentsApi: api, ALLOWED_EMOJIS: ["👍"] }));
vi.mock("../../auth/SessionContext", () => ({
  useSession: () => ({ data: { ok: { user_id: "u1", display_name: "Anna", email: "a@x.test", role: "editor" } } }),
}));
vi.mock("../../auth/useEffectiveRole", () => ({
  useEffectiveRole: () => ({ workspace: "editor", doc: "editor" }),
}));
vi.mock("../workspace/workspace.api", () => ({
  workspaceApi: { listMembers: () => Promise.resolve({ ok: [] }) },
}));

const ROOT: Comment = {
  id: "c1",
  doc_id: "d1",
  thread_id: "t1",
  parent_id: null,
  author_id: "u1",
  body: "Looks good",
  position_y: null,
  position_y_end: null,
  anchor_text: null,
  created_at: "2026-11-12T10:00:00Z",
  updated_at: "2026-11-12T10:00:00Z",
  resolved_at: null,
  reactions: {},
};

beforeEach(() => {
  // jsdom has no layout; the thread scrolls itself into view when active.
  Element.prototype.scrollIntoView = vi.fn();
  for (const fn of Object.values(api)) fn.mockReset();
  useUi.setState({ toasts: [] });
});
afterEach(cleanup);

describe("CommentThread when a limit refuses a write", () => {
  it("says to slow down when resolving is refused", async () => {
    api.resolve.mockResolvedValue({
      error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 3 }, status: 429 },
    });
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <CommentThread docId="d1" threadId="t1" root={ROOT} replies={[]} />
      </QueryClientProvider>,
    );
    fireEvent.click(screen.getByTestId("comment-resolve-t1"));
    await waitFor(() =>
      expect(useUi.getState().toasts).toMatchObject([
        { kind: "warn", text: "You're doing that too fast. Try again in 3 s.", key: "limit" },
      ]),
    );
  });
});
