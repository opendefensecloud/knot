import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import PublicDoc from "./PublicDoc";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function renderPublic() {
  render(
    <QueryClientProvider client={new QueryClient()}>
      <MemoryRouter initialEntries={["/p/tok"]}>
        <Routes>
          <Route path="/p/:token" element={<PublicDoc />} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

describe("PublicDoc", () => {
  it("shows the shared page", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("<html><body>Hi</body></html>", { status: 200, headers: { "Content-Type": "text/html" } }),
    );
    renderPublic();
    expect(await screen.findByTestId("public-doc")).toBeInTheDocument();
  });

  it("says how long to wait when the link is rate limited", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(
        JSON.stringify({ error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 5 } } }),
        { status: 429, headers: { "Content-Type": "application/json", "Retry-After": "5" } },
      ),
    );
    renderPublic();
    expect(await screen.findByRole("heading", { name: "Too many requests" })).toBeInTheDocument();
    expect(screen.getByText("You're doing that too fast. Try again in 5 s.")).toBeInTheDocument();
  });

  it("still names the limit for a 429 whose body isn't the envelope", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response("slow down", { status: 429 }));
    renderPublic();
    expect(await screen.findByText("You're doing that too fast. Try again in a moment.")).toBeInTheDocument();
  });
});
