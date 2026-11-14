import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { authApi } from "../../auth/session.api";
import SetupPage from "./SetupPage";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("SetupPage when setup is rate limited", () => {
  it("says how long to wait instead of 'Setup failed'", async () => {
    vi.spyOn(authApi, "setup").mockResolvedValue({
      error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 3 }, status: 429 },
    });
    render(
      <QueryClientProvider client={new QueryClient()}>
        <MemoryRouter>
          <SetupPage />
        </MemoryRouter>
      </QueryClientProvider>,
    );
    const form = screen.getByTestId("setup-form");
    for (const input of Array.from(form.querySelectorAll("input"))) {
      fireEvent.change(input, { target: { value: input.type === "email" ? "anna@example.org" : "long-enough-1" } });
    }
    fireEvent.submit(form);
    expect(await screen.findByTestId("setup-error")).toHaveTextContent(
      "You're doing that too fast. Try again in 3 s.",
    );
  });
});
