import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, renderHook } from "@testing-library/react";
import type { ReactNode } from "react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { HistoryEntry, HistoryPage } from "../../../lib/validators";
import { useHistoryEntries } from "./useHistoryEntries";

const api = vi.hoisted(() => ({ entries: vi.fn() }));

vi.mock("../../../lib/history.api", () => ({
  historyApi: { entries: api.entries },
}));

function session(id: number, over: Partial<HistoryEntry> = {}): HistoryEntry {
  return {
    id,
    kind: "session",
    author: { key: "u-ann", name: "Ann" },
    started_at: "2025-11-03T09:00:00Z",
    ended_at: "2025-11-03T09:30:00Z",
    open: false,
    words_added: 3,
    words_removed: 1,
    meta: {},
    ...over,
  };
}

function page(entries: HistoryEntry[], over: Partial<HistoryPage> = {}): HistoryPage {
  return {
    history_since: "2025-01-01T00:00:00Z",
    history_complete: true,
    catching_up: false,
    unavailable: false,
    entries,
    next_cursor: null,
    ...over,
  };
}

function wrapper() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return function Wrapper({ children }: { children: ReactNode }) {
    return <QueryClientProvider client={qc}>{children}</QueryClientProvider>;
  };
}

/** Advances the faked intervals (the polls) and lets the fetches settle. */
async function advance(ms: number) {
  await act(async () => {
    vi.advanceTimersByTime(ms);
    await Promise.resolve();
  });
}

/** Retries `check` on real timeouts: `waitFor` polls with `setInterval`,
 *  which these tests fake, and a hook changes no DOM to wake it. */
async function until(check: () => void) {
  for (let attempt = 0; ; attempt += 1) {
    try {
      check();
      return;
    } catch (e) {
      if (attempt >= 100) throw e;
    }
    await act(async () => {
      await new Promise((resolve) => setTimeout(resolve, 10));
    });
  }
}

beforeEach(() => {
  // Only the intervals: the polls are TanStack's `setInterval`, while its
  // notifications keep real timeouts.
  vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  api.entries.mockReset();
});

describe("useHistoryEntries", () => {
  it("a_session_that_moved_above_the_cursor_reappears_after_the_poll", async () => {
    let polled = false;
    api.entries.mockImplementation((_doc: string, cursor: string | null) => {
      if (!polled) {
        return Promise.resolve({
          ok: cursor === null
            ? page([session(9, { open: true }), session(8)], { next_cursor: "c1" })
            : page([session(5)]),
        });
      }
      // s5 was typed into again: its end moved above the cursor "c1".
      return Promise.resolve({
        ok: cursor === null
          ? page([session(5, { ended_at: "2025-11-03T10:00:00Z", open: true }), session(9, { open: true }), session(8)], {
              next_cursor: "c1",
            })
          : page([]),
      });
    });
    const { result } = renderHook(() => useHistoryEntries("d1"), { wrapper: wrapper() });
    await until(() => expect(result.current.entries.map((e) => e.id)).toEqual([9, 8]));
    await act(async () => {
      await result.current.fetchNextPage();
    });
    await until(() => expect(result.current.entries.map((e) => e.id)).toEqual([9, 8, 5]));

    polled = true;
    await advance(15_000);
    await until(() => expect(result.current.entries.map((e) => e.id)).toEqual([5, 9, 8]));
    expect(result.current.entries[0]!.ended_at).toBe("2025-11-03T10:00:00Z");
    expect(result.current.entries.filter((e) => e.id === 5)).toHaveLength(1);
  });

  it("keeps polling while the drawer is open", async () => {
    // Only sealed entries, not catching up: every 15 s.
    api.entries.mockResolvedValue({ ok: page([session(1)]) });
    const sealed = renderHook(() => useHistoryEntries("d1"), { wrapper: wrapper() });
    await until(() => expect(sealed.result.current.entries).toHaveLength(1));
    expect(api.entries).toHaveBeenCalledTimes(1);
    await advance(5_000);
    expect(api.entries).toHaveBeenCalledTimes(1);
    await advance(10_000);
    await until(() => expect(api.entries).toHaveBeenCalledTimes(2));
    sealed.unmount();
    await advance(60_000);
    expect(api.entries).toHaveBeenCalledTimes(2);

    // Nothing loaded yet: every 5 s.
    api.entries.mockReset();
    api.entries.mockResolvedValue({ ok: page([]) });
    const empty = renderHook(() => useHistoryEntries("d2"), { wrapper: wrapper() });
    await until(() => expect(empty.result.current.hasData).toBe(true));
    expect(api.entries).toHaveBeenCalledTimes(1);
    await advance(5_000);
    await until(() => expect(api.entries).toHaveBeenCalledTimes(2));
    empty.unmount();

    // Entries on a page still catching up: 15 s, not 5 s.
    api.entries.mockReset();
    api.entries.mockResolvedValue({ ok: page([session(1)], { catching_up: true }) });
    const catching = renderHook(() => useHistoryEntries("d3"), { wrapper: wrapper() });
    await until(() => expect(catching.result.current.first?.catching_up).toBe(true));
    expect(api.entries).toHaveBeenCalledTimes(1);
    await advance(5_000);
    expect(api.entries).toHaveBeenCalledTimes(1);
    await advance(10_000);
    await until(() => expect(api.entries).toHaveBeenCalledTimes(2));
    catching.unmount();
    await advance(60_000);
    expect(api.entries).toHaveBeenCalledTimes(2);
  });
});
