import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { type ApiKey, apiKeysApi } from "./apiKeys.api";
import { ApiKeysSection } from "./ApiKeysSection";

let maxTtl: number | null = null;
let list: ReturnType<typeof vi.fn>;

vi.mock("../../auth/SessionContext", () => ({
  useSession: () => ({
    data: {
      ok: {
        user_id: "me",
        email: "me@example.org",
        display_name: "Me",
        workspace_id: "w1",
        role: "editor",
        auth_kind: maxTtl === null ? "local" : "oidc",
        max_api_key_ttl_days: maxTtl,
      },
    },
  }),
}));

const KEYS: ApiKey[] = [
  {
    id: "k1",
    name: "CI bot",
    scopes: ["read", "update"],
    created_at: "2026-10-01T00:00:00Z",
    expires_at: null,
    last_used_at: null,
    revoked_at: null,
  },
];

function renderSection(props: { targetUserId?: string } = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  render(
    <QueryClientProvider client={qc}>
      <ApiKeysSection {...props} />
    </QueryClientProvider>,
  );
  return { qc, invalidate };
}

beforeEach(() => {
  maxTtl = null;
  list = vi.spyOn(apiKeysApi, "list").mockResolvedValue({ ok: KEYS });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ApiKeysSection", () => {
  it("renders the key list with scopes and Never for no expiry", async () => {
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(row).toHaveTextContent("CI bot");
    expect(row).toHaveTextContent("read");
    expect(row).toHaveTextContent("update");
    expect(row).toHaveTextContent("Never");
  });

  it("posts name, scopes and expires_in_days, then reveals the key once without caching it", async () => {
    const create = vi.spyOn(apiKeysApi, "create").mockResolvedValue({
      ok: { id: "k2", key: "knot_SECRET", name: "deploy", scopes: ["read", "delete"], expires_at: null },
    });
    const { qc } = renderSection();
    await screen.findByTestId("api-key-row");

    expect(screen.getByTestId("api-key-scope-read")).toBeChecked();
    fireEvent.change(screen.getByTestId("api-key-name"), { target: { value: " deploy " } });
    fireEvent.click(screen.getByTestId("api-key-scope-delete"));
    fireEvent.change(screen.getByTestId("api-key-expiry"), { target: { value: "never" } });
    fireEvent.click(screen.getByTestId("api-key-submit"));

    const reveal = await screen.findByTestId("api-key-reveal");
    expect(create).toHaveBeenCalledWith({
      name: " deploy ",
      scopes: ["read", "delete"],
      expiresInDays: null,
      userId: undefined,
    });
    expect(within(reveal).getByDisplayValue("knot_SECRET")).toBeInTheDocument();
    expect(reveal).toHaveTextContent("shown once");
    expect(JSON.stringify(qc.getQueryCache().getAll().map((q) => q.state.data))).not.toContain("knot_SECRET");
    expect(JSON.stringify(qc.getMutationCache().getAll().map((m) => m.state.data))).not.toContain("knot_SECRET");
  });

  it("sends the request body the server expects", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify({ id: "k2", key: "knot_X", name: "n", scopes: ["read"], expires_at: "2027-01-01T00:00:00Z" }), {
        status: 201,
        headers: { "content-type": "application/json" },
      }),
    );
    await apiKeysApi.create({ name: " n ", scopes: ["read"], expiresInDays: 90, userId: "bot1" });
    const call = fetchMock.mock.calls[0];
    expect(call?.[0]).toBe("/api/auth/api-keys");
    const body = call?.[1]?.body;
    expect(typeof body).toBe("string");
    expect(JSON.parse(body as string)).toEqual({
      name: "n",
      scopes: ["read"],
      expires_in_days: 90,
      user_id: "bot1",
    });
  });

  it("hides Never when the session caps keys at 90 days", async () => {
    maxTtl = 90;
    renderSection();
    const select = await screen.findByTestId("api-key-expiry");
    const values = Array.from(select.querySelectorAll("option")).map((o) => o.value);
    expect(values).toEqual(["30", "90"]);
  });

  it("offers Never for a bot target even when the owner is capped, and passes user_id", async () => {
    maxTtl = 90;
    renderSection({ targetUserId: "bot1" });
    const select = await screen.findByTestId("api-key-expiry");
    expect(Array.from(select.querySelectorAll("option")).map((o) => o.value)).toContain("never");
    expect(list).toHaveBeenCalledWith("bot1");
  });

  it("revokes after confirming and invalidates the key list", async () => {
    const revoke = vi.spyOn(apiKeysApi, "revoke").mockResolvedValue({ ok: undefined });
    const { invalidate } = renderSection();
    fireEvent.click(await screen.findByTestId("api-key-revoke"));
    expect(revoke).not.toHaveBeenCalled();
    fireEvent.click(screen.getByTestId("api-key-revoke-confirm"));
    await waitFor(() => expect(revoke).toHaveBeenCalledWith("k1", undefined));
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["api-keys", "self"] }));
  });

  it("maps a server error code to a message", async () => {
    vi.spyOn(apiKeysApi, "create").mockResolvedValue({
      error: { code: "api_key.ttl_too_long", message: "", details: {}, status: 400 },
    });
    renderSection();
    fireEvent.change(await screen.findByTestId("api-key-name"), { target: { value: "x" } });
    fireEvent.click(screen.getByTestId("api-key-submit"));
    expect(await screen.findByTestId("api-key-error")).toHaveTextContent("longer than your account allows");
    expect(screen.queryByTestId("api-key-reveal")).not.toBeInTheDocument();
  });

  it("explains the live-key cap on 409", async () => {
    vi.spyOn(apiKeysApi, "create").mockResolvedValue({
      error: { code: "api_key.limit_reached", message: "", details: {}, status: 409 },
    });
    renderSection();
    fireEvent.change(await screen.findByTestId("api-key-name"), { target: { value: "x" } });
    fireEvent.click(screen.getByTestId("api-key-submit"));
    expect(await screen.findByTestId("api-key-error")).toHaveTextContent("50 active keys");
    expect(screen.queryByTestId("api-key-reveal")).not.toBeInTheDocument();
  });
});
