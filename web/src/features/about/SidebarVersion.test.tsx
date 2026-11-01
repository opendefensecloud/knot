import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SidebarVersion } from "./SidebarVersion";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

function renderVersion(response: () => Promise<Response>) {
  vi.stubGlobal("fetch", vi.fn(response));
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <SidebarVersion />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return qc;
}

const body = {
  version: "0.8.1",
  commit: "52fa7a1",
  released_on: "2026-10-21",
  dev: false,
  last_seen_version: "0.8.1",
  unseen: [],
};

describe("SidebarVersion", () => {
  it("shows the running version and links to the About page", async () => {
    renderVersion(() => json(body));

    const link = await screen.findByTestId("sidebar-version");
    expect(link).toHaveTextContent(/^knot 0\.8\.1$/);
    expect(link).toHaveAttribute("href", "/about");
  });

  it("says dev on a development build", async () => {
    renderVersion(() => json({ ...body, version: "dev", released_on: null, dev: true }));

    expect(await screen.findByTestId("sidebar-version")).toHaveTextContent(/^knot dev$/);
  });

  it("shows nothing when the version can't be loaded", async () => {
    const qc = renderVersion(() => json({ error: { code: "boom", message: "", details: {} } }, 500));

    await waitFor(() => expect(qc.getQueryState(["whats-new"])?.status).toBe("success"));
    expect(screen.queryByTestId("sidebar-version")).toBeNull();
  });
});
