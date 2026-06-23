import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, waitFor, cleanup } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { authApi } from "../../auth/session.api";
import LoginPage from "./LoginPage";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function renderLogin() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <LoginPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function mockConfig(cfg: {
  setup_available: boolean;
  oidc_enabled: boolean;
  password_login_enabled: boolean;
}) {
  vi.spyOn(authApi, "config").mockResolvedValue({ ok: cfg });
}

describe("LoginPage conditional options", () => {
  it("shows a prominent SSO button only when oidc is enabled", async () => {
    mockConfig({ setup_available: false, oidc_enabled: true, password_login_enabled: true });
    renderLogin();
    const sso = await screen.findByTestId("login-sso");
    expect(sso).toBeInTheDocument();
    expect(sso.className).toContain("bg-accent");
  });

  it("hides the SSO button when oidc is disabled", async () => {
    mockConfig({ setup_available: false, oidc_enabled: false, password_login_enabled: true });
    renderLogin();
    await screen.findByTestId("login-form");
    await waitFor(() => {
      expect(screen.queryByTestId("login-sso")).toBeNull();
    });
  });

  it("shows the setup link only when setup is available", async () => {
    mockConfig({ setup_available: true, oidc_enabled: false, password_login_enabled: true });
    renderLogin();
    expect(await screen.findByTestId("login-setup")).toBeInTheDocument();
  });

  it("hides the setup link when setup is unavailable", async () => {
    mockConfig({ setup_available: false, oidc_enabled: false, password_login_enabled: true });
    renderLogin();
    await screen.findByTestId("login-form");
    await waitFor(() => {
      expect(screen.queryByTestId("login-setup")).toBeNull();
    });
  });
});
