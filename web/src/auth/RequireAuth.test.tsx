import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import LoginPage from "../features/auth/LoginPage";

import { RequireAuth } from "./RequireAuth";
import { authApi } from "./session.api";
import { SessionProvider } from "./SessionContext";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("RequireAuth", () => {
  it("keeps the whole link, query and hash included, through the SSO sign-in", async () => {
    vi.spyOn(authApi, "session").mockResolvedValue({
      error: { code: "auth.session_required", message: "" },
    } as Awaited<ReturnType<typeof authApi.session>>);
    vi.spyOn(authApi, "config").mockResolvedValue({
      ok: {
        setup_available: false,
        oidc_enabled: true,
        password_login_enabled: true,
      },
    });
    const link = "/doc/0b9f6a4e?thread=7c1d#comment";
    const qc = new QueryClient({
      defaultOptions: { queries: { retry: false } },
    });
    render(
      <QueryClientProvider client={qc}>
        <SessionProvider>
          <MemoryRouter initialEntries={[link]}>
            <Routes>
              <Route path="/login" element={<LoginPage />} />
              <Route element={<RequireAuth />}>
                <Route path="/doc/:id" element={<p>page</p>} />
              </Route>
            </Routes>
          </MemoryRouter>
        </SessionProvider>
      </QueryClientProvider>,
    );
    const sso = await screen.findByTestId("login-sso");
    expect(sso.getAttribute("href")).toBe(
      `/auth/oidc/login?return_to=${encodeURIComponent(link)}`,
    );
  });
});
