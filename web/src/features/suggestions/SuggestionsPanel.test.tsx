/**
 * The Suggestions tab: the sidebar's two tabs with their counts, pending
 * cards in document order, and the collapsed "Resolved" section.
 */

import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Comment } from "../../lib/comments.api";
import type { SuggestionWithReplies } from "../../lib/suggestions.api";
import { useUi } from "../../stores/ui";
import { CommentSidebar } from "../comments/CommentSidebar";
import { SuggestionsPanel } from "./SuggestionsPanel";

const api = vi.hoisted(() => ({
  list: vi.fn(),
  create: vi.fn(),
  accept: vi.fn(),
  reject: vi.fn(),
  withdraw: vi.fn(),
  reply: vi.fn(),
}));
vi.mock("../../lib/suggestions.api", () => ({ suggestionsApi: api }));

const commentsList = vi.hoisted(() => vi.fn());
vi.mock("../../lib/comments.api", () => ({
  ALLOWED_EMOJIS: [],
  commentsApi: { list: commentsList },
}));

const OWNER = "00000000-0000-0000-0000-00000000000a";
const AUTHOR = "00000000-0000-0000-0000-00000000000b";

vi.mock("../workspace/workspace.api", () => ({
  workspaceApi: {
    listMembers: vi.fn(() =>
      Promise.resolve({
        ok: [
          { user_id: "00000000-0000-0000-0000-00000000000a", display_name: "Olga Owner", email: "o@x.test", role: "owner" },
          { user_id: "00000000-0000-0000-0000-00000000000b", display_name: "Anna Author", email: "a@x.test", role: "editor" },
        ],
      }),
    ),
  },
}));

vi.mock("../../auth/useEffectiveRole", () => ({
  useEffectiveRole: () => ({ workspace: "owner", doc: "owner" }),
}));

vi.mock("../../auth/SessionContext", () => ({
  useSession: () => ({ data: { ok: { user_id: "00000000-0000-0000-0000-00000000000a" } } }),
}));

function suggestion(id: string, over: Partial<SuggestionWithReplies> = {}): SuggestionWithReplies {
  return {
    id,
    doc_id: "d1",
    author_id: AUTHOR,
    kind: "replace",
    position_y: "AAcEAA==",
    position_y_end: "AAcFQQ==",
    original_text: `old ${id}`,
    context_before: "",
    proposed_text: `new ${id}`,
    reason: null,
    status: "pending",
    decided_by: null,
    decided_at: null,
    created_at: "2026-11-02T10:00:00Z",
    updated_at: "2026-11-02T10:00:00Z",
    replies: [],
    ...over,
  };
}

function comment(id: string, resolved: boolean): Comment {
  return {
    id,
    doc_id: "d1",
    thread_id: id,
    parent_id: null,
    author_id: AUTHOR,
    body: `comment ${id}`,
    position_y: null,
    position_y_end: null,
    anchor_text: null,
    created_at: "2026-11-02T09:00:00Z",
    updated_at: "2026-11-02T09:00:00Z",
    resolved_at: resolved ? "2026-11-02T09:30:00Z" : null,
    reactions: {},
  };
}

const PENDING = [
  suggestion("s-late", { created_at: "2026-11-02T10:03:00Z" }),
  suggestion("s-unplaced", { created_at: "2026-11-02T10:01:00Z" }),
  suggestion("s-early", { created_at: "2026-11-02T10:02:00Z" }),
];
const RESOLVED = [
  suggestion("s-acc", { status: "accepted", decided_by: OWNER, decided_at: "2026-11-02T11:00:00Z" }),
  suggestion("s-rej", { status: "rejected", decided_by: OWNER, decided_at: "2026-11-02T11:05:00Z" }),
];

function wrap(ui: React.ReactElement) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(<QueryClientProvider client={qc}>{ui}</QueryClientProvider>);
}

/** The ids of the cards in `root`, in DOM order. */
function cardIds(root: HTMLElement): string[] {
  return within(root)
    .queryAllByTestId(/^suggestion-card-/)
    .map((el) => el.getAttribute("data-testid")!.replace("suggestion-card-", ""));
}

beforeEach(() => {
  api.list.mockReset();
  api.list.mockImplementation((_doc: string, includeResolved?: boolean) =>
    Promise.resolve({ ok: includeResolved ? [...PENDING, ...RESOLVED] : PENDING }),
  );
  commentsList.mockReset();
  commentsList.mockResolvedValue({ ok: [comment("c1", false), comment("c2", false)] });
  useUi.setState({
    commentSidebarOpen: true,
    sidebarTab: "comments",
    pendingAnchor: null,
    pendingSuggestion: null,
    activeSuggestionId: null,
    suggestionLink: null,
    suggestionLive: {
      "s-late": { state: "same", currentText: "old s-late", from: 40 },
      "s-early": { state: "changed", currentText: "older", from: 5 },
      // s-unplaced has no live state: it goes last.
    },
  });
});

afterEach(cleanup);

describe("SuggestionsPanel", () => {
  it("panel_tabs_show_pending_count", async () => {
    wrap(<CommentSidebar docId="d1" />);
    const comments = screen.getByTestId("sidebar-tab-comments");
    const suggestions = screen.getByTestId("sidebar-tab-suggestions");
    await waitFor(() => expect(suggestions).toHaveTextContent("Suggestions 3"));
    await waitFor(() => expect(comments).toHaveTextContent("Comments 2"));
    expect(comments).toHaveAttribute("aria-pressed", "true");
    expect(suggestions).toHaveAttribute("aria-pressed", "false");
    expect(screen.getByTestId("comment-show-resolved")).toBeInTheDocument();
    expect(screen.queryByTestId(/^suggestion-card-/)).toBeNull();

    fireEvent.click(suggestions);
    expect(useUi.getState().sidebarTab).toBe("suggestions");
    expect(suggestions).toHaveAttribute("aria-pressed", "true");
    expect(comments).toHaveAttribute("aria-pressed", "false");
    // The comments' resolved toggle belongs to the comments tab only.
    expect(screen.queryByTestId("comment-show-resolved")).toBeNull();
    await screen.findByTestId("suggestion-card-s-early");

    fireEvent.click(comments);
    expect(useUi.getState().sidebarTab).toBe("comments");
    expect(await screen.findByText("comment c1")).toBeInTheDocument();
  });

  it("orders_pending_cards_by_document_position", async () => {
    const { container } = wrap(<SuggestionsPanel docId="d1" />);
    await screen.findByTestId("suggestion-card-s-early");
    expect(cardIds(container)).toEqual(["s-early", "s-late", "s-unplaced"]);
  });

  it("resolved_section_lists_decided", async () => {
    wrap(<SuggestionsPanel docId="d1" />);
    await screen.findByTestId("suggestion-card-s-early");
    // Collapsed: nothing resolved is fetched or shown.
    expect(api.list).not.toHaveBeenCalledWith("d1", true);
    expect(screen.queryByTestId("suggestion-card-s-acc")).toBeNull();

    const toggle = screen.getByTestId("suggestion-show-resolved");
    expect(toggle).toHaveTextContent("Resolved");
    fireEvent.click(toggle);

    const accepted = await screen.findByTestId("suggestion-card-s-acc");
    expect(api.list).toHaveBeenCalledWith("d1", true);
    expect(accepted).toHaveAttribute("data-state", "accepted");
    expect(accepted).toHaveTextContent("Accepted");
    await waitFor(() => expect(accepted).toHaveTextContent("Olga Owner"));
    const rejected = screen.getByTestId("suggestion-card-s-rej");
    expect(rejected).toHaveAttribute("data-state", "rejected");
    expect(rejected).toHaveTextContent("Rejected");
    // Pending ones stay in the pending list, not repeated under Resolved.
    expect(screen.getAllByTestId("suggestion-card-s-early")).toHaveLength(1);
  });

  it("a_link_to_a_resolved_suggestion_opens_the_resolved_section", async () => {
    useUi.setState({ activeSuggestionId: "s-acc", suggestionLink: { suggestionId: "s-acc" } });
    wrap(<SuggestionsPanel docId="d1" />);
    const card = await screen.findByTestId("suggestion-card-s-acc");
    expect(card).toHaveAttribute("data-active", "true");
  });

  it("shows_the_composer_while_a_suggestion_is_pending", () => {
    useUi.setState({
      pendingSuggestion: {
        kind: "replace",
        positionY: "AAcEAA==",
        positionYEnd: "AAcFQQ==",
        originalText: "charlie",
        contextBefore: "",
      },
    });
    wrap(<SuggestionsPanel docId="d1" />);
    expect(screen.getByTestId("suggestion-composer")).toBeInTheDocument();
  });

  it("shows_an_empty_state", async () => {
    api.list.mockResolvedValue({ ok: [] });
    wrap(<SuggestionsPanel docId="d1" />);
    expect(await screen.findByText("No suggestions yet.")).toBeInTheDocument();
  });
});
