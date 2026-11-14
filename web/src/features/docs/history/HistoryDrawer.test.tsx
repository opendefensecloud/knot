import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useState } from "react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { colorFor } from "../../../components/ui/Avatar";
import type { HistoryEntry, HistoryPage } from "../../../lib/validators";
import { copy } from "./copy";
import { HistoryDrawer } from "./HistoryDrawer";
import { entryLine } from "./timeline";

const api = vi.hoisted(() => ({ entries: vi.fn() }));

vi.mock("../../../lib/history.api", () => ({
  historyApi: { entries: api.entries },
}));

function entry(id: number, over: Partial<HistoryEntry> = {}): HistoryEntry {
  return {
    id,
    kind: "session",
    author: { key: "u-ann", name: "Ann" },
    started_at: "2025-11-03T11:00:00Z",
    ended_at: "2025-11-03T12:00:00Z",
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

const serverError = {
  error: { status: 500, code: "internal", message: "internal error", details: {} },
};

function renderDrawer() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onClose = vi.fn();
  render(
    <MemoryRouter>
      <QueryClientProvider client={qc}>
        <HistoryDrawer docId="d1" canRestore onClose={onClose} />
      </QueryClientProvider>
    </MemoryRouter>,
  );
  return { onClose };
}

/** The badge `Avatar` renders inside an entry's row. */
function avatarOf(id: number): Element {
  return screen.getByTestId(`history-entry-${id}`).querySelector('[aria-hidden="true"]')!;
}

function before(a: Element, b: Element): boolean {
  return (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
}

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  api.entries.mockReset();
});

describe("HistoryDrawer", () => {
  it("lists entries under day headings with their wording", async () => {
    const restore = entry(3, { kind: "restore", started_at: "2025-11-03T12:30:00Z", ended_at: "2025-11-03T12:30:00Z" });
    const accepted = entry(2, {
      meta: {
        accepted_by: [{ key: "u-bea", name: "Bea", former: false }, { key: null, name: null, former: true }, null],
      },
    });
    const gap = entry(4, {
      kind: "gap",
      author: null,
      started_at: "2025-11-02T12:30:00Z",
      ended_at: "2025-11-02T12:30:00Z",
      words_added: null,
      words_removed: null,
    });
    const first = entry(1, {
      author: { key: "u-bea", name: "Bea" },
      started_at: "2025-11-02T11:00:00Z",
      ended_at: "2025-11-02T12:00:00Z",
    });
    api.entries.mockResolvedValue({ ok: page([restore, accepted, gap, first]) });
    renderDrawer();

    await screen.findByTestId("history-entry-1");
    expect(screen.getAllByTestId("history-day")).toHaveLength(2);
    const now = new Date();
    for (const e of [restore, accepted, gap, first]) {
      const row = screen.getByTestId(`history-entry-${e.id}`);
      expect(row).toHaveTextContent(entryLine(e, now));
      expect(row).toHaveAttribute("data-kind", e.kind);
    }
    expect(within(screen.getByTestId("history-entry-2")).getByTestId("history-entry-note")).toHaveTextContent(
      "accepted by Bea, Unknown author, Unknown author",
    );
    expect(screen.getAllByTestId("history-entry-note")).toHaveLength(1);
    expect(avatarOf(1)).toHaveStyle({ background: colorFor("u-bea") });
    expect(avatarOf(4).getAttribute("style")).toContain("var(--color-fg-muted)");
    expect(screen.getByTestId("history-drawer")).not.toHaveTextContent("Former member");
    // Day headings first, then each day's entries newest first.
    const [day2, day1] = screen.getAllByTestId("history-day");
    expect(before(day2!, screen.getByTestId("history-entry-3"))).toBe(true);
    expect(before(screen.getByTestId("history-entry-2"), day1!)).toBe(true);
    expect(before(day1!, screen.getByTestId("history-entry-4"))).toBe(true);
  });

  it("shows earlier versions and where complete history starts", async () => {
    const baseline = (id: number, at: string) =>
      entry(id, { kind: "baseline", author: null, started_at: at, ended_at: at, words_added: null, words_removed: null });
    api.entries.mockResolvedValue({
      ok: page([entry(5), baseline(2, "2025-06-01T10:00:00Z"), baseline(1, "2025-05-01T10:00:00Z")], {
        history_complete: false,
        next_cursor: null,
      }),
    });
    renderDrawer();

    const earlier = await screen.findByTestId("history-earlier");
    expect(earlier).toHaveTextContent(copy.earlierSection);
    const since = screen.getByTestId("history-since");
    expect(since).toHaveTextContent("Complete history starts");
    expect(before(screen.getByTestId("history-entry-5"), earlier)).toBe(true);
    expect(before(earlier, screen.getByTestId("history-entry-2"))).toBe(true);
    expect(before(screen.getByTestId("history-entry-1"), since)).toBe(true);
    expect(avatarOf(2).getAttribute("style")).toContain("var(--color-fg-muted)");
  });

  it("loads more on demand", async () => {
    api.entries.mockImplementation((_doc: string, cursor: string | null) =>
      Promise.resolve({ ok: cursor === null ? page([entry(9)], { next_cursor: "c1" }) : page([entry(4)]) }),
    );
    renderDrawer();

    fireEvent.click(await screen.findByTestId("history-load-more"));
    expect(await screen.findByTestId("history-entry-4")).toBeInTheDocument();
    expect(api.entries).toHaveBeenCalledWith("d1", "c1");
    expect(screen.getByTestId("history-entry-9")).toBeInTheDocument();
    expect(screen.queryByTestId("history-load-more")).toBeNull();
  });

  it("loads more when the end of the list scrolls into view", async () => {
    const observers: { callback: IntersectionObserverCallback; observed: Element[]; disconnected: boolean }[] = [];
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        callback: IntersectionObserverCallback;
        observed: Element[] = [];
        disconnected = false;
        constructor(callback: IntersectionObserverCallback) {
          this.callback = callback;
          observers.push(this);
        }
        observe(el: Element) {
          this.observed.push(el);
        }
        unobserve() {}
        disconnect() {
          this.disconnected = true;
        }
        takeRecords() {
          return [];
        }
      },
    );
    api.entries.mockImplementation((_doc: string, cursor: string | null) =>
      Promise.resolve({ ok: cursor === null ? page([entry(9)], { next_cursor: "c1" }) : page([entry(4)]) }),
    );
    renderDrawer();

    const sentinel = await screen.findByTestId("history-sentinel");
    const live = observers.find((o) => !o.disconnected)!;
    expect(live.observed).toEqual([sentinel]);
    act(() =>
      live.callback(
        [{ isIntersecting: true, target: sentinel } as unknown as IntersectionObserverEntry],
        live as unknown as IntersectionObserver,
      ),
    );
    expect(await screen.findByTestId("history-entry-4")).toBeInTheDocument();
    expect(api.entries).toHaveBeenCalledWith("d1", "c1");
    expect(api.entries).toHaveBeenCalledTimes(2);

    await waitFor(() => expect(screen.queryByTestId("history-sentinel")).toBeNull());
    for (const o of observers) {
      act(() =>
        o.callback(
          [{ isIntersecting: true, target: sentinel } as unknown as IntersectionObserverEntry],
          o as unknown as IntersectionObserver,
        ),
      );
    }
    await act(async () => {
      await new Promise((r) => setTimeout(r, 20));
    });
    expect(api.entries).toHaveBeenCalledTimes(2);
    expect(observers.every((o) => o.disconnected)).toBe(true);
  });

  it("says when history is catching up or unavailable", async () => {
    api.entries.mockResolvedValue({ ok: page([entry(1)], { catching_up: true }) });
    renderDrawer();
    const catching = await screen.findByTestId("history-catching-up");
    expect(catching).toHaveTextContent(copy.catchingUp);
    expect(catching.closest('[aria-live="polite"]')).not.toBeNull();
    expect(screen.queryByTestId("history-unavailable")).toBeNull();
    cleanup();

    api.entries.mockResolvedValue({ ok: page([entry(1)], { unavailable: true }) });
    renderDrawer();
    const unavailable = await screen.findByTestId("history-unavailable");
    expect(unavailable).toHaveTextContent(copy.unavailable);
    expect(unavailable.closest('[aria-live="polite"]')).not.toBeNull();
    expect(screen.queryByTestId("history-catching-up")).toBeNull();
    expect(screen.getByTestId("history-entry-1")).toBeInTheDocument();
  });

  it("says when there is no history and when loading failed", async () => {
    api.entries.mockResolvedValue({ ok: page([]) });
    renderDrawer();
    const empty = await screen.findByTestId("history-empty");
    expect(empty).toHaveTextContent(copy.empty);
    expect(empty.closest('[aria-live="polite"]')).not.toBeNull();
    cleanup();

    api.entries.mockReset();
    api.entries.mockResolvedValueOnce(serverError).mockResolvedValue({ ok: page([entry(1)]) });
    renderDrawer();
    const error = await screen.findByTestId("history-error");
    expect(error).toHaveTextContent(copy.loadError);
    expect(error.closest('[aria-live="polite"]')).not.toBeNull();
    expect(screen.queryByTestId("history-refresh-error")).toBeNull();
    fireEvent.click(within(error).getByRole("button", { name: copy.retry }));
    expect(await screen.findByTestId("history-entry-1")).toBeInTheDocument();
    expect(screen.queryByTestId("history-error")).toBeNull();
    expect(api.entries).toHaveBeenCalledTimes(2);
    cleanup();

    api.entries.mockReset();
    api.entries.mockRejectedValue(new TypeError("Failed to fetch"));
    renderDrawer();
    expect(await screen.findByTestId("history-error")).toHaveTextContent(copy.loadError);
  });

  it.each([
    ["resolves an error", () => Promise.resolve(serverError)],
    ["rejects", () => Promise.reject(new TypeError("Failed to fetch"))],
  ])("a failed poll keeps the loaded timeline (the poll %s)", async (_name, failure) => {
    vi.useFakeTimers({ toFake: ["setInterval", "clearInterval"] });
    api.entries
      .mockResolvedValueOnce({ ok: page([entry(2), entry(1)]) })
      .mockImplementationOnce(failure)
      .mockResolvedValue({ ok: page([entry(2), entry(1)]) });
    renderDrawer();
    await screen.findByTestId("history-entry-1");

    await act(async () => {
      vi.advanceTimersByTime(15_000);
      await Promise.resolve();
    });
    expect(await screen.findByTestId("history-refresh-error")).toHaveTextContent(copy.refreshError);
    expect(screen.getByTestId("history-entry-1")).toBeInTheDocument();
    expect(screen.getByTestId("history-entry-2")).toBeInTheDocument();
    expect(screen.queryByTestId("history-error")).toBeNull();
    expect(before(screen.getByTestId("history-refresh-error"), screen.getByTestId("history-entry-2"))).toBe(true);

    await act(async () => {
      vi.advanceTimersByTime(15_000);
      await Promise.resolve();
    });
    await waitFor(() => expect(screen.queryByTestId("history-refresh-error")).toBeNull());
    expect(api.entries).toHaveBeenCalledTimes(3);
    expect(screen.getByTestId("history-entry-1")).toBeInTheDocument();
  });

  it("closes on the close button, the backdrop and Escape", async () => {
    api.entries.mockResolvedValue({ ok: page([entry(1)]) });
    const close = renderDrawer();
    fireEvent.click(await screen.findByTestId("history-close"));
    expect(close.onClose).toHaveBeenCalledTimes(1);
    cleanup();

    const backdrop = renderDrawer();
    fireEvent.click(await screen.findByTestId("history-backdrop"));
    expect(backdrop.onClose).toHaveBeenCalledTimes(1);
    // A click inside the panel does not close it.
    fireEvent.click(screen.getByTestId("history-drawer"));
    expect(backdrop.onClose).toHaveBeenCalledTimes(1);
    cleanup();

    const escape = renderDrawer();
    fireEvent.keyDown(await screen.findByTestId("history-entry-1"), { key: "Escape" });
    expect(escape.onClose).toHaveBeenCalledTimes(1);
  });

  it("is a labelled modal dialog and gives focus back on close", async () => {
    api.entries.mockResolvedValue({ ok: page([entry(1)], { next_cursor: "c1" }) });
    function Page() {
      const [open, setOpen] = useState(false);
      return (
        <>
          <button type="button" data-testid="open-history" onClick={() => setOpen(true)}>
            History
          </button>
          {open && <HistoryDrawer docId="d1" canRestore onClose={() => setOpen(false)} />}
        </>
      );
    }
    render(
      <MemoryRouter>
        <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
          <Page />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    const trigger = screen.getByTestId("open-history");
    trigger.focus();
    fireEvent.click(trigger);

    const dialog = screen.getByRole("dialog", { name: "History" });
    expect(dialog).toHaveAttribute("aria-modal", "true");
    const close = screen.getByTestId("history-close");
    expect(document.activeElement).toBe(close);

    // Tab from the last control comes round to the first, Shift+Tab back.
    const last = await screen.findByTestId("history-load-more");
    last.focus();
    fireEvent.keyDown(last, { key: "Tab" });
    expect(document.activeElement).toBe(close);
    fireEvent.keyDown(close, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(last);
    // In between, Tab is left to the browser.
    close.focus();
    const entryButton = screen.getByTestId("history-entry-1");
    expect(fireEvent.keyDown(close, { key: "Tab" })).toBe(true);
    expect(dialog.contains(entryButton)).toBe(true);

    fireEvent.keyDown(close, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(document.activeElement).toBe(trigger);
  });

  it("marks the selected entry", async () => {
    api.entries.mockResolvedValue({ ok: page([entry(7), entry(6)]) });
    renderDrawer();
    const row = await screen.findByTestId("history-entry-7");
    expect(row).not.toHaveAttribute("aria-current");
    expect(screen.getByTestId("history-version-pane")).toHaveTextContent(copy.selectEntry);
    fireEvent.click(row);
    expect(row).toHaveAttribute("aria-current", "true");
    expect(screen.getByTestId("history-entry-6")).not.toHaveAttribute("aria-current");
  });
});
