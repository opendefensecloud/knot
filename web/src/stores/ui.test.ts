/**
 * The comment sidebar's tabs. `sidebarTab` is global, so every way into the
 * sidebar has to land on the right tab: a suggestion link on Suggestions,
 * and every comment entry point back on Comments.
 */

import { beforeEach, describe, expect, it } from "vitest";

import { useUi } from "./ui";

describe("ui sidebar tabs", () => {
  beforeEach(() => {
    useUi.setState({
      commentSidebarOpen: false,
      sidebarTab: "comments",
      activeCommentId: null,
      activeSuggestionId: null,
      threadLink: null,
      suggestionLink: null,
      pendingSuggestion: null,
      suggestionLive: {},
    });
  });

  it("openSuggestionLink_opens_sidebar_on_suggestions_tab", () => {
    useUi.getState().openSuggestionLink("s-1");
    const s = useUi.getState();
    expect(s.commentSidebarOpen).toBe(true);
    expect(s.sidebarTab).toBe("suggestions");
    expect(s.activeSuggestionId).toBe("s-1");
    expect(s.suggestionLink).toEqual({ suggestionId: "s-1" });
  });

  it("openSuggestionLink_makes_a_new_link_per_visit", () => {
    useUi.getState().openSuggestionLink("s-1");
    const first = useUi.getState().suggestionLink;
    useUi.getState().openSuggestionLink("s-1");
    expect(useUi.getState().suggestionLink).not.toBe(first);
  });

  it("openThreadLink_switches_back_to_comments_tab", () => {
    useUi.getState().openSuggestionLink("s-1");
    useUi.getState().openThreadLink("t-1");
    const s = useUi.getState();
    expect(s.commentSidebarOpen).toBe(true);
    expect(s.sidebarTab).toBe("comments");
    expect(s.activeCommentId).toBe("t-1");
  });

  it("openCommentSidebar_defaults_to_comments_tab", () => {
    useUi.getState().setSidebarTab("suggestions");
    useUi.getState().openCommentSidebar();
    expect(useUi.getState().commentSidebarOpen).toBe(true);
    expect(useUi.getState().sidebarTab).toBe("comments");

    useUi.getState().openCommentSidebar("suggestions");
    expect(useUi.getState().sidebarTab).toBe("suggestions");
  });

  it("pending_suggestion_set_and_clear", () => {
    const p = {
      kind: "replace" as const,
      positionY: "AAA=",
      positionYEnd: "AAE=",
      originalText: "brave",
      contextBefore: "a ",
    };
    useUi.getState().setPendingSuggestion(p);
    expect(useUi.getState().pendingSuggestion).toEqual(p);
    useUi.getState().clearPendingSuggestion();
    expect(useUi.getState().pendingSuggestion).toBeNull();
  });

  it("leaving_the_suggestions_tab_drops_the_pending_suggestion", () => {
    const p = {
      kind: "replace" as const,
      positionY: "AAA=",
      positionYEnd: "AAE=",
      originalText: "brave",
      contextBefore: "a ",
    };
    const open = () => {
      useUi.getState().setPendingSuggestion(p);
      useUi.getState().openCommentSidebar("suggestions");
      expect(useUi.getState().pendingSuggestion).toEqual(p);
    };

    open();
    useUi.getState().setSidebarTab("suggestions");
    expect(useUi.getState().pendingSuggestion).toEqual(p);
    useUi.getState().setSidebarTab("comments");
    expect(useUi.getState().pendingSuggestion).toBeNull();

    open();
    useUi.getState().closeCommentSidebar();
    expect(useUi.getState().pendingSuggestion).toBeNull();

    open();
    useUi.getState().openCommentSidebar();
    expect(useUi.getState().pendingSuggestion).toBeNull();

    open();
    useUi.getState().openThreadLink("t-1");
    expect(useUi.getState().pendingSuggestion).toBeNull();
  });

  it("leaving_the_suggestions_tab_drops_the_focused_suggestion", () => {
    // A stale active id or link would scroll the page back to an old
    // highlight, or open Resolved again, the next time the tab shows.
    const focused = () => {
      useUi.getState().openSuggestionLink("s-1");
      const s = useUi.getState();
      expect(s.activeSuggestionId).toBe("s-1");
      expect(s.suggestionLink).toEqual({ suggestionId: "s-1" });
    };
    const expectCleared = () => {
      const s = useUi.getState();
      expect(s.activeSuggestionId).toBeNull();
      expect(s.suggestionLink).toBeNull();
    };

    focused();
    useUi.getState().setSidebarTab("suggestions");
    expect(useUi.getState().activeSuggestionId).toBe("s-1");
    useUi.getState().setSidebarTab("comments");
    expectCleared();

    focused();
    useUi.getState().closeCommentSidebar();
    expectCleared();

    focused();
    useUi.getState().openCommentSidebar("suggestions");
    expect(useUi.getState().activeSuggestionId).toBe("s-1");
    useUi.getState().openCommentSidebar();
    expectCleared();

    focused();
    useUi.getState().openThreadLink("t-1");
    expectCleared();

    // KnotEditor, leaving the page.
    focused();
    useUi.getState().clearSuggestionFocus();
    expectCleared();
  });

  it("suggestion_live_keeps_entries_that_read_the_same", () => {
    useUi.getState().setSuggestionLive({
      a: { state: "same", currentText: "x", from: 1 },
      b: { state: "same", currentText: "y", from: 5 },
    });
    const before = useUi.getState().suggestionLive;
    useUi.getState().setSuggestionLive({
      a: { state: "same", currentText: "x", from: 1 },
      b: { state: "changed", currentText: "yy", from: 6 },
    });
    const after = useUi.getState().suggestionLive;
    expect(after.a).toBe(before.a);
    expect(after.b).toEqual({ state: "changed", currentText: "yy", from: 6 });
  });

  it("suggestion_live_is_replaced_whole", () => {
    useUi.getState().setSuggestionLive({ a: { state: "same", currentText: "x", from: 1 } });
    useUi.getState().setSuggestionLive({ b: { state: "removed", currentText: "", from: null } });
    expect(useUi.getState().suggestionLive).toEqual({ b: { state: "removed", currentText: "", from: null } });
  });
});
