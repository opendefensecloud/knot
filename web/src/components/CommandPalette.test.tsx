import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { docsApi } from "../features/docs/docs.api";
import { useUi } from "../stores/ui";
import { CommandPalette } from "./CommandPalette";

function renderPalette() {
  useUi.setState({ paletteOpen: true });
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <MemoryRouter>
        <CommandPalette />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  useUi.setState({ toasts: [], paletteOpen: false });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("CommandPalette 'Create new document'", () => {
  it("names the page quota when the new page is refused", async () => {
    vi.spyOn(docsApi, "create").mockResolvedValue({
      error: {
        code: "quota.exceeded",
        message: "quota exceeded",
        details: { quota: "pages", scope: "user", limit: 10 },
        status: 403,
      },
    });
    renderPalette();
    fireEvent.click(screen.getByTestId("cmdk-item-action:create"));
    await waitFor(() =>
      expect(useUi.getState().toasts).toMatchObject([
        { kind: "warn", text: "You've reached your page limit.", key: "limit" },
      ]),
    );
  });

  it("stays silent for other failures, as before", async () => {
    const create = vi.spyOn(docsApi, "create").mockResolvedValue({
      error: { code: "internal", message: "boom", details: {}, status: 500 },
    });
    renderPalette();
    fireEvent.click(screen.getByTestId("cmdk-item-action:create"));
    await waitFor(() => expect(create).toHaveBeenCalled());
    expect(useUi.getState().toasts).toEqual([]);
  });
});
