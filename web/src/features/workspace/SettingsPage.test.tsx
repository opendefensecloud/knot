import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { authApi } from "../../auth/session.api";
import SettingsPage from "./SettingsPage";
import { workspaceApi } from "./workspace.api";

vi.mock("../../auth/SessionContext", () => ({
  useSession: () => ({ data: undefined }),
}));

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <SettingsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { invalidate };
}

function submitPasswordChange() {
  fireEvent.change(screen.getByTestId("pw-current"), { target: { value: "oldpassword1" } });
  fireEvent.change(screen.getByTestId("pw-new"), { target: { value: "newpassword1" } });
  fireEvent.click(screen.getByTestId("pw-submit"));
}

describe("SettingsPage password change", () => {
  it("refreshes every API key list once the change revoked the keys", async () => {
    vi.spyOn(workspaceApi, "get").mockResolvedValue({ error: { code: "x", message: "", details: {} } } as never);
    vi.spyOn(authApi, "changePassword").mockResolvedValue({ ok: undefined });
    const { invalidate } = renderPage();

    submitPasswordChange();

    expect(await screen.findByTestId("pw-ok")).toBeInTheDocument();
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["api-keys"] }));
  });

  it("leaves the key lists alone when the change failed", async () => {
    vi.spyOn(workspaceApi, "get").mockResolvedValue({ error: { code: "x", message: "", details: {} } } as never);
    vi.spyOn(authApi, "changePassword").mockResolvedValue({
      error: { code: "auth.invalid_credentials", message: "", details: {} },
    } as never);
    const { invalidate } = renderPage();

    submitPasswordChange();

    expect(await screen.findByTestId("pw-error")).toHaveTextContent("Current password is wrong.");
    expect(invalidate).not.toHaveBeenCalledWith({ queryKey: ["api-keys"] });
  });
});
