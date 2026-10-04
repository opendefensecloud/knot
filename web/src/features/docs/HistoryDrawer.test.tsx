import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { HistoryDrawer } from "./HistoryDrawer";

vi.mock("../../lib/history.api", () => ({
  historyApi: {
    list: () =>
      Promise.resolve({
        ok: [{ snapshot_seq: 7, byte_size: 2048, created_at: "2026-10-03T10:00:00Z" }],
      }),
    preview: () => Promise.resolve({ ok: "# Earlier\n" }),
    restore: () => Promise.resolve({ ok: undefined }),
  },
}));

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("HistoryDrawer", () => {
  // A restore changes the content under the restorer's name: the byline
  // should list them right away, not on its next poll.
  it("refreshes the byline after a restore", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(qc, "invalidateQueries");
    render(
      <QueryClientProvider client={qc}>
        <HistoryDrawer docId="d1" onClose={() => {}} />
      </QueryClientProvider>,
    );

    fireEvent.click(await screen.findByTestId("history-snap-7"));
    fireEvent.click(await screen.findByTestId("history-restore"));

    await waitFor(() =>
      expect(invalidate).toHaveBeenCalledWith({ queryKey: ["contributors", "d1"] }),
    );
  });
});
