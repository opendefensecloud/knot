/**
 * One suggestion card: what it shows for each live state, which buttons a
 * role gets, and what Accept sends and does with the server's refusals.
 */

import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Role } from "../../auth/useEffectiveRole";
import type { SuggestionWithReplies } from "../../lib/suggestions.api";
import { useUi } from "../../stores/ui";
import type { SuggestionLive } from "./liveState";
import { SuggestionCard } from "./SuggestionCard";

const api = vi.hoisted(() => ({
  list: vi.fn(),
  create: vi.fn(),
  accept: vi.fn(),
  reject: vi.fn(),
  withdraw: vi.fn(),
  reply: vi.fn(),
}));
vi.mock("../../lib/suggestions.api", () => ({ suggestionsApi: api }));

vi.mock("../workspace/workspace.api", () => ({
  workspaceApi: { listMembers: vi.fn(() => Promise.resolve({ ok: [] })) },
}));

const OWNER = "00000000-0000-0000-0000-00000000000a";
const AUTHOR = "00000000-0000-0000-0000-00000000000b";
const OTHER = "00000000-0000-0000-0000-00000000000c";

function suggestion(over: Partial<SuggestionWithReplies> = {}): SuggestionWithReplies {
  return {
    id: "s1",
    doc_id: "d1",
    author_id: AUTHOR,
    kind: "replace",
    position_y: "AAcEAA==",
    position_y_end: "AAcFQQ==",
    original_text: "charlie",
    context_before: "alpha bravo ",
    proposed_text: "charlotte",
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

const NAMES: Record<string, string> = { [OWNER]: "Olga Owner", [AUTHOR]: "Anna Author", [OTHER]: "Ed Editor" };

function renderCard(
  s: SuggestionWithReplies,
  opts: { role?: Role | null; userId?: string; live?: SuggestionLive | null } = {},
) {
  const { role = "owner", userId = OWNER, live } = opts;
  useUi.setState({ suggestionLive: live ? { [s.id]: live } : {} });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  render(
    <QueryClientProvider client={qc}>
      <SuggestionCard
        docId="d1"
        suggestion={s}
        role={role}
        currentUserId={userId}
        nameOf={(id) => (id ? (NAMES[id] ?? "Unknown") : "Unknown")}
      />
    </QueryClientProvider>,
  );
  return { invalidate, card: screen.getByTestId(`suggestion-card-${s.id}`) };
}

/** The label/value rows a card shows, in order. */
function rows(card: HTMLElement): string[] {
  return within(card)
    .queryAllByTestId("suggestion-row")
    .map((r) => r.textContent ?? "");
}

beforeEach(() => {
  for (const fn of Object.values(api)) fn.mockReset();
  useUi.setState({ activeSuggestionId: null, toasts: [] });
});

afterEach(cleanup);

describe("SuggestionCard", () => {
  it("card_without_live_state_is_unknown_and_accept_disabled", () => {
    const { card } = renderCard(suggestion(), { live: null });
    expect(card).toHaveAttribute("data-state", "unknown");
    expect(rows(card)).toEqual(["Nowcharlie", "Suggestedcharlotte"]);
    expect(screen.getByTestId("suggestion-accept-s1")).toBeDisabled();
    expect(screen.getByTestId("suggestion-reject-s1")).toBeEnabled();
  });

  it("card_same_changed_removed_rows", () => {
    let r = renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    expect(r.card).toHaveAttribute("data-state", "same");
    expect(rows(r.card)).toEqual(["Nowcharlie", "Suggestedcharlotte"]);
    expect(screen.queryByTestId("suggestion-note-s1")).toBeNull();
    expect(screen.getByTestId("suggestion-accept-s1")).toBeEnabled();
    cleanup();

    r = renderCard(suggestion(), { live: { state: "changed", currentText: "charles", from: 3 } });
    expect(r.card).toHaveAttribute("data-state", "changed");
    expect(rows(r.card)).toEqual(["When suggestedcharlie", "Nowcharles", "Suggestedcharlotte"]);
    expect(screen.getByTestId("suggestion-note-s1")).toHaveTextContent("Changed since it was suggested");
    expect(screen.getByTestId("suggestion-accept-s1")).toBeEnabled();
    cleanup();

    r = renderCard(suggestion(), { live: { state: "removed", currentText: "", from: null } });
    expect(r.card).toHaveAttribute("data-state", "removed");
    expect(screen.getByTestId("suggestion-note-s1")).toHaveTextContent(
      "The text this suggestion was about was removed.",
    );
    expect(screen.getByTestId("suggestion-accept-s1")).toBeDisabled();
    expect(screen.getByTestId("suggestion-reject-s1")).toBeEnabled();
    cleanup();

    // A deletion reads "Remove"; an insertion "Add", after its context.
    r = renderCard(suggestion({ kind: "delete", proposed_text: "" }), {
      live: { state: "same", currentText: "charlie", from: 3 },
    });
    expect(rows(r.card)).toEqual(["Removecharlie"]);
    cleanup();

    r = renderCard(suggestion({ kind: "insert", original_text: "", proposed_text: " delta" }), {
      live: { state: "same", currentText: "", from: 3 },
    });
    expect(rows(r.card)).toEqual(["Add delta"]);
    expect(r.card).toHaveTextContent("…alpha bravo");
  });

  it("owner_sees_accept_reject_editor_sees_waiting", () => {
    const live: SuggestionLive = { state: "same", currentText: "charlie", from: 3 };
    renderCard(suggestion(), { role: "owner", userId: OWNER, live });
    expect(screen.getByTestId("suggestion-accept-s1")).toHaveTextContent("Accept");
    expect(screen.getByTestId("suggestion-reject-s1")).toHaveTextContent("Reject");
    expect(screen.queryByTestId("suggestion-waiting-s1")).toBeNull();
    expect(screen.queryByTestId("suggestion-withdraw-s1")).toBeNull();
    cleanup();

    renderCard(suggestion(), { role: "editor", userId: OTHER, live });
    expect(screen.queryByTestId("suggestion-accept-s1")).toBeNull();
    expect(screen.queryByTestId("suggestion-reject-s1")).toBeNull();
    expect(screen.getByTestId("suggestion-waiting-s1")).toHaveTextContent("Waiting for an owner");
    expect(screen.getByTestId("suggestion-reply-input-s1")).toBeInTheDocument();
    cleanup();

    // A viewer reads the card but has nothing to do on it.
    renderCard(suggestion(), { role: "viewer", userId: OTHER, live });
    expect(screen.queryByTestId("suggestion-accept-s1")).toBeNull();
    expect(screen.queryByTestId("suggestion-withdraw-s1")).toBeNull();
    expect(screen.queryByTestId("suggestion-reply-input-s1")).toBeNull();
  });

  it("author_sees_withdraw", async () => {
    api.withdraw.mockResolvedValue({ ok: suggestion({ status: "withdrawn" }) });
    const { invalidate } = renderCard(suggestion(), {
      role: "editor",
      userId: AUTHOR,
      live: { state: "same", currentText: "charlie", from: 3 },
    });
    const withdraw = screen.getByTestId("suggestion-withdraw-s1");
    expect(withdraw).toHaveTextContent("Withdraw");
    fireEvent.click(withdraw);
    await waitFor(() => expect(api.withdraw).toHaveBeenCalledWith("d1", "s1"));
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["suggestions", "d1"] }));
  });

  it("a_downgraded_author_does_not_see_withdraw", () => {
    // The server only lets an Editor or Owner withdraw; a viewer now, the
    // author would only get a refusal.
    const live: SuggestionLive = { state: "same", currentText: "charlie", from: 3 };
    for (const role of ["viewer", null] as const) {
      renderCard(suggestion(), { role, userId: AUTHOR, live });
      expect(screen.queryByTestId("suggestion-withdraw-s1"), String(role)).toBeNull();
      cleanup();
    }

    // An author who owns the page now still may.
    renderCard(suggestion(), { role: "owner", userId: AUTHOR, live });
    expect(screen.getByTestId("suggestion-withdraw-s1")).toBeInTheDocument();
  });

  it("accept_sends_current_text_as_seen_text", async () => {
    api.accept.mockResolvedValue({ ok: suggestion({ status: "accepted" }) });
    const { invalidate } = renderCard(suggestion(), {
      live: { state: "changed", currentText: "charles", from: 3 },
    });
    fireEvent.click(screen.getByTestId("suggestion-accept-s1"));
    await waitFor(() => expect(api.accept).toHaveBeenCalledWith("d1", "s1", "charles"));
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["suggestions", "d1"] }));
    cleanup();

    // An insertion has no range: the seen text is empty.
    renderCard(suggestion({ id: "s2", kind: "insert", original_text: "", proposed_text: "x" }), {
      live: { state: "same", currentText: "", from: 3 },
    });
    fireEvent.click(screen.getByTestId("suggestion-accept-s2"));
    await waitFor(() => expect(api.accept).toHaveBeenCalledWith("d1", "s2", ""));
  });

  it("accept_409_changed_shows_note_and_refetches", async () => {
    api.accept.mockResolvedValue({
      error: { code: "suggestion.changed", message: "changed", details: {}, status: 409 },
    });
    const { invalidate } = renderCard(suggestion(), {
      live: { state: "same", currentText: "charlie", from: 3 },
    });
    fireEvent.click(screen.getByTestId("suggestion-accept-s1"));
    await waitFor(() =>
      expect(screen.getByTestId("suggestion-note-s1")).toHaveTextContent(
        "The text changed while you were looking. Check it and accept again.",
      ),
    );
    expect(invalidate).toHaveBeenCalledWith({ queryKey: ["suggestions", "d1"] });
    // Never retried by itself, and the owner can try again.
    expect(api.accept).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("suggestion-accept-s1")).toBeEnabled();
  });

  it("accept_409_anchor_lost_and_not_pending_refetch_without_a_toast", async () => {
    for (const code of ["suggestion.anchor_lost", "suggestion.not_pending"]) {
      api.accept.mockResolvedValue({
        error: { code, message: code, details: {}, status: 409 },
      });
      const { invalidate } = renderCard(suggestion(), {
        live: { state: "same", currentText: "charlie", from: 3 },
      });
      fireEvent.click(screen.getByTestId("suggestion-accept-s1"));
      await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["suggestions", "d1"] }));
      expect(useUi.getState().toasts).toEqual([]);
      cleanup();
    }
  });

  it("accept_409_anchor_lost_says_the_text_was_removed", async () => {
    // The card read the page as "same", the server could not find the text:
    // the refusal is never silent, even before a reclassification agrees.
    api.accept.mockResolvedValue({
      error: { code: "suggestion.anchor_lost", message: "lost", details: {}, status: 409 },
    });
    renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    fireEvent.click(screen.getByTestId("suggestion-accept-s1"));
    await waitFor(() =>
      expect(screen.getByTestId("suggestion-note-s1")).toHaveTextContent(
        "The text this suggestion was about was removed.",
      ),
    );
    cleanup();

    // Someone else deciding it first leaves no note; the refetch shows it.
    api.accept.mockResolvedValue({
      error: { code: "suggestion.not_pending", message: "decided", details: {}, status: 409 },
    });
    const { invalidate } = renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    fireEvent.click(screen.getByTestId("suggestion-accept-s1"));
    await waitFor(() => expect(invalidate).toHaveBeenCalled());
    expect(screen.queryByTestId("suggestion-note-s1")).toBeNull();
  });

  it("reject_and_reply_call_the_api", async () => {
    api.reject.mockResolvedValue({ ok: suggestion({ status: "rejected" }) });
    api.reply.mockResolvedValue({ ok: {} as never });
    renderCard(suggestion({
      replies: [{
        id: "r1", suggestion_id: "s1", author_id: AUTHOR, body: "please?",
        created_at: "2026-11-02T10:05:00Z", updated_at: "2026-11-02T10:05:00Z",
      }],
    }), { live: { state: "same", currentText: "charlie", from: 3 } });
    expect(screen.getByTestId("suggestion-card-s1")).toHaveTextContent("please?");

    fireEvent.click(screen.getByTestId("suggestion-reject-s1"));
    await waitFor(() => expect(api.reject).toHaveBeenCalledWith("d1", "s1"));

    fireEvent.change(screen.getByTestId("suggestion-reply-input-s1"), { target: { value: "no thanks" } });
    fireEvent.click(screen.getByTestId("suggestion-reply-submit-s1"));
    await waitFor(() => expect(api.reply).toHaveBeenCalledWith("d1", "s1", "no thanks", []));
  });

  it("focusing_a_card_makes_it_active", () => {
    renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    const card = screen.getByTestId("suggestion-card-s1");
    expect(card).not.toHaveAttribute("data-active");
    fireEvent.click(card);
    expect(useUi.getState().activeSuggestionId).toBe("s1");
    expect(card).toHaveAttribute("data-active", "true");
  });

  it("keyboard_enter_on_a_card_makes_it_active", () => {
    renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    const card = screen.getByTestId("suggestion-card-s1");
    expect(card).toHaveAttribute("tabindex", "0");
    // Keys typed in the reply box are the reply's, not the card's.
    fireEvent.keyDown(screen.getByTestId("suggestion-reply-input-s1"), { key: "Enter" });
    expect(useUi.getState().activeSuggestionId).toBeNull();
    fireEvent.keyDown(card, { key: "Enter" });
    expect(useUi.getState().activeSuggestionId).toBe("s1");
  });

  it("a_link_scrolls_the_page_to_the_highlight_once_it_renders", async () => {
    renderCard(suggestion(), { live: null });
    const scrolled = vi.fn();
    try {
      // The link lands before the editor has synced: no highlight yet.
      act(() => useUi.getState().openSuggestionLink("s1"));
      await new Promise((r) => window.requestAnimationFrame(() => r(null)));
      expect(scrolled).not.toHaveBeenCalled();

      // The editor draws the highlight and classifies the suggestion.
      const mark = document.createElement("span");
      mark.setAttribute("data-suggestion-id", "s1");
      mark.scrollIntoView = scrolled;
      document.body.appendChild(mark);
      act(() => useUi.getState().setSuggestionLive({ s1: { state: "same", currentText: "charlie", from: 3 } }));
      await waitFor(() => expect(scrolled).toHaveBeenCalledTimes(1));

      // Once per activation, not on every later live update.
      act(() => useUi.getState().setSuggestionLive({ s1: { state: "same", currentText: "charlie", from: 9 } }));
      await new Promise((r) => window.requestAnimationFrame(() => r(null)));
      expect(scrolled).toHaveBeenCalledTimes(1);
    } finally {
      document.querySelectorAll("[data-suggestion-id]").forEach((el) => el.remove());
      useUi.setState({ commentSidebarOpen: false, sidebarTab: "comments", suggestionLink: null });
    }
  });

  it("resolved_card_shows_status_and_decider", () => {
    const { card } = renderCard(
      suggestion({ status: "accepted", decided_by: OWNER, decided_at: new Date().toISOString() }),
      { live: null },
    );
    expect(card).toHaveAttribute("data-state", "accepted");
    expect(card).toHaveTextContent("Accepted");
    expect(card).toHaveTextContent("Olga Owner");
    expect(screen.queryByTestId("suggestion-accept-s1")).toBeNull();
    expect(screen.queryByTestId("suggestion-reject-s1")).toBeNull();
    expect(screen.queryByTestId("suggestion-reply-input-s1")).toBeNull();
  });
});

describe("SuggestionCard when a limit refuses a decision", () => {
  const TOO_FAST = {
    error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 2 }, status: 429 },
  };
  const TOO_FAST_TOAST = { kind: "warn", text: "You're doing that too fast. Try again in 2 s.", key: "limit" };

  it("says to slow down when an accept is refused", async () => {
    api.accept.mockResolvedValue(TOO_FAST);
    renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    fireEvent.click(screen.getByTestId("suggestion-accept-s1"));
    await waitFor(() => expect(useUi.getState().toasts).toMatchObject([TOO_FAST_TOAST]));
  });

  it("says to slow down when a reject is refused", async () => {
    api.reject.mockResolvedValue(TOO_FAST);
    renderCard(suggestion(), { live: { state: "same", currentText: "charlie", from: 3 } });
    fireEvent.click(screen.getByTestId("suggestion-reject-s1"));
    await waitFor(() => expect(useUi.getState().toasts).toMatchObject([TOO_FAST_TOAST]));
  });
});
