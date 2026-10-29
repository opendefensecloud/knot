import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import PermissionsDialog from "./PermissionsDialog";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const ADA = "11111111-1111-1111-1111-111111111111";
const GONE = "22222222-2222-2222-2222-222222222222";

function json(body: unknown) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } }),
  );
}

function renderDialog() {
  vi.stubGlobal("fetch", (url: string) => {
    if (url.endsWith("/grants")) {
      return json([
        { principal: `user:${ADA}`, role: "editor", inherit: true },
        { principal: `user:${GONE}`, role: "viewer", inherit: false },
      ]);
    }
    if (url.endsWith("/api/workspace/members")) {
      return json([{ user_id: ADA, email: "ada@example.com", display_name: "Ada Lovelace", role: "editor" }]);
    }
    if (url.endsWith("/shares")) return json([]);
    return json(null);
  });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/doc/d1/permissions"]}>
        <Routes>
          <Route path="/doc/:id/permissions" element={<PermissionsDialog />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("PermissionsDialog grants", () => {
  it("shows a member's name and email instead of the user id", async () => {
    renderDialog();
    const row = await screen.findByTestId(`grant-user:${ADA}`);
    expect(await within(row).findByText("Ada Lovelace")).toBeTruthy();
    expect(within(row).getByText("ada@example.com")).toBeTruthy();
    expect(row.textContent).not.toContain(ADA);
  });

  it("falls back to the id for someone who is no longer a member", async () => {
    renderDialog();
    const row = await screen.findByTestId(`grant-user:${GONE}`);
    expect(await within(row).findByText("Former member")).toBeTruthy();
    expect(row.textContent).toContain(GONE);
  });
});
