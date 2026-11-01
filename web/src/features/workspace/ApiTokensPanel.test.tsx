import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import {
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { apiTokensApi } from "../../lib/apiTokens.api";
import type { ApiToken } from "../../lib/validators";

import { ApiTokensPanel } from "./ApiTokensPanel";

const TOKENS: ApiToken[] = [
  {
    id: "t1",
    name: "docs-bot",
    scopes: ["read", "create", "write"],
    created_at: new Date(Date.now() - 3_600_000).toISOString(),
    last_used_at: new Date(Date.now() - 60_000).toISOString(),
    revoked_at: null,
  },
  {
    id: "t2",
    name: "old-bot",
    scopes: ["read"],
    created_at: new Date(Date.now() - 86_400_000).toISOString(),
    last_used_at: null,
    revoked_at: new Date(Date.now() - 3_600_000).toISOString(),
  },
];

function renderPanel() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <ApiTokensPanel />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  vi.spyOn(apiTokensApi, "list").mockResolvedValue({ ok: TOKENS });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ApiTokensPanel list", () => {
  it("lists tokens with scopes and usage, and only offers Revoke on live ones", async () => {
    renderPanel();

    const live = await screen.findByTestId("api-token-t1");
    expect(within(live).getByText("docs-bot")).toBeInTheDocument();
    expect(within(live).getByText(/read, create, write/)).toBeInTheDocument();
    expect(within(live).getByText(/last used/)).toBeInTheDocument();
    expect(
      within(live).getByRole("button", { name: "Revoke" }),
    ).toBeInTheDocument();

    const revoked = screen.getByTestId("api-token-t2");
    expect(within(revoked).getByText("revoked")).toBeInTheDocument();
    expect(within(revoked).getByText(/never used/)).toBeInTheDocument();
    expect(
      within(revoked).queryByRole("button", { name: "Revoke" }),
    ).not.toBeInTheDocument();
  });

  it("shows an empty state when there are no tokens", async () => {
    vi.spyOn(apiTokensApi, "list").mockResolvedValue({ ok: [] });
    renderPanel();
    expect(await screen.findByTestId("api-tokens-empty")).toHaveTextContent(
      "No tokens yet.",
    );
  });
});

describe("ApiTokensPanel create", () => {
  it("cannot submit without a name or without a scope", async () => {
    renderPanel();
    await screen.findByTestId("api-token-t1");
    const button = screen.getByTestId("api-token-create");
    expect(button).toBeDisabled();

    fireEvent.change(screen.getByTestId("api-token-name"), {
      target: { value: "ci" },
    });
    expect(button).not.toBeDisabled();

    // Uncheck every default scope → nothing to grant, still disabled.
    for (const scope of ["read", "create", "write"]) {
      fireEvent.click(screen.getByTestId(`api-token-scope-${scope}`));
    }
    expect(button).toBeDisabled();
    expect(
      screen.getByText("Pick at least one permission."),
    ).toBeInTheDocument();
  });

  it("creates a token and shows the secret exactly once, with a copy action", async () => {
    const create = vi.spyOn(apiTokensApi, "create").mockResolvedValue({
      ok: {
        ...TOKENS[0]!,
        id: "t3",
        name: "ci-bot",
        token_secret: "knot_secret_value",
      },
    });
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.assign(navigator, { clipboard: { writeText } });

    renderPanel();
    await screen.findByTestId("api-token-t1");

    fireEvent.change(screen.getByTestId("api-token-name"), {
      target: { value: "  ci-bot  " },
    });
    fireEvent.click(screen.getByTestId("api-token-scope-delete"));
    fireEvent.click(screen.getByTestId("api-token-create"));

    const secret = await screen.findByTestId("api-token-secret");
    expect(within(secret).getByText(/shown only once/)).toBeInTheDocument();
    expect(secret).toHaveTextContent("knot_secret_value");

    // name is trimmed, scopes are sent in canonical order
    expect(create).toHaveBeenCalledWith("ci-bot", [
      "read",
      "create",
      "write",
      "delete",
    ]);

    fireEvent.click(screen.getByTestId("api-token-copy"));
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith("knot_secret_value"),
    );

    // dismissing hides the secret for good
    fireEvent.click(within(secret).getByText("Done"));
    expect(screen.queryByTestId("api-token-secret")).not.toBeInTheDocument();
  });

  it("reports a scope rejection instead of showing a secret", async () => {
    vi.spyOn(apiTokensApi, "create").mockResolvedValue({
      error: { code: "token.scopes", message: "", details: {}, status: 422 },
    });
    renderPanel();
    await screen.findByTestId("api-token-t1");

    fireEvent.change(screen.getByTestId("api-token-name"), {
      target: { value: "bad" },
    });
    fireEvent.click(screen.getByTestId("api-token-create"));

    await waitFor(() =>
      expect(screen.queryByTestId("api-token-secret")).not.toBeInTheDocument(),
    );
  });
});

describe("ApiTokensPanel revoke", () => {
  it("asks first and calls the API only on confirm", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const revoke = vi.spyOn(apiTokensApi, "revoke");
    renderPanel();

    const live = await screen.findByTestId("api-token-t1");
    fireEvent.click(within(live).getByRole("button", { name: "Revoke" }));

    expect(confirm).toHaveBeenCalledWith(
      "Revoke “docs-bot”? Anything still using it stops working immediately.",
    );
    expect(revoke).not.toHaveBeenCalled();

    confirm.mockReturnValue(true);
    fireEvent.click(within(live).getByRole("button", { name: "Revoke" }));
    await waitFor(() => expect(revoke).toHaveBeenCalledWith("t1"));
  });
});
