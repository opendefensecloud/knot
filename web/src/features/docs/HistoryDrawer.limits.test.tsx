import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useUi } from "../../stores/ui";
import { HistoryDrawer } from "./HistoryDrawer";

vi.mock("../../lib/history.api", () => ({
  historyApi: {
    list: () =>
      Promise.resolve({
        ok: [{ snapshot_seq: 7, byte_size: 2048, created_at: "2026-10-03T10:00:00Z" }],
      }),
    preview: () => Promise.resolve({ ok: "# Earlier\n" }),
    restore: () =>
      Promise.resolve({
        error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 11 }, status: 429 },
      }),
  },
}));

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  useUi.setState({ toasts: [] });
});

describe("HistoryDrawer when a restore is rate limited", () => {
  it("says how long to wait instead of 'Restore failed'", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    useUi.setState({ toasts: [] });
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <HistoryDrawer docId="d1" onClose={() => {}} />
      </QueryClientProvider>,
    );
    fireEvent.click(await screen.findByTestId("history-snap-7"));
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() =>
      expect(useUi.getState().toasts).toMatchObject([
        { kind: "warn", text: "You're doing that too fast. Try again in 11 s.", key: "limit" },
      ]),
    );
  });
});
