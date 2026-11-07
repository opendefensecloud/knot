import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { useUi } from "../../stores/ui";
import { workspaceApi } from "../workspace/workspace.api";

import { type ApiKey, apiKeysApi } from "./apiKeys.api";
import { ApiKeysSection } from "./ApiKeysSection";

let maxTtl: number | null = null;
let list: ReturnType<typeof vi.fn>;
let listMembers: ReturnType<typeof vi.fn>;

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
    created_by: "owner1",
    created_at: "2026-10-01T00:00:00Z",
    expires_at: null,
    last_used_at: "2026-10-15T12:00:00Z",
    revoked_at: null,
  },
];

const MEMBERS = [
  { user_id: "owner1", email: "olive@example.org", display_name: "Olive Owner", role: "owner" as const, kind: "human" as const },
];

/** One key listed, with the fields that decide its state and columns overridden. */
function listOne(fields: Partial<ApiKey>) {
  list.mockResolvedValue({ ok: [{ ...KEYS[0]!, ...fields }] });
}

/** Creates a key through the form, leaving the one-time reveal dialog open. */
async function openReveal() {
  vi.spyOn(apiKeysApi, "create").mockResolvedValue({
    ok: { id: "k2", key: "knot_SECRET", name: "deploy", scopes: ["read"], expires_at: null },
  });
  renderSection();
  fireEvent.change(await screen.findByTestId("api-key-name"), { target: { value: "deploy" } });
  fireEvent.click(screen.getByTestId("api-key-submit"));
  await screen.findByTestId("api-key-reveal");
}

function setClipboard(value: unknown) {
  Object.defineProperty(navigator, "clipboard", { value, configurable: true });
}

function toastTexts(kind: "info" | "error") {
  return useUi.getState().toasts.filter((t) => t.kind === kind).map((t) => t.text);
}

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
  useUi.setState({ toasts: [] });
  list = vi.spyOn(apiKeysApi, "list").mockResolvedValue({ ok: KEYS });
  listMembers = vi.spyOn(workspaceApi, "listMembers").mockResolvedValue({ ok: MEMBERS });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  Reflect.deleteProperty(navigator, "clipboard");
});

describe("ApiKeysSection", () => {
  // Catches the expiry cell rendering last_used_at, or a Never that comes from
  // the wrong column: the two columns hold different values in each fixture.
  it("renders the key list with scopes and Never for no expiry", async () => {
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(row).toHaveTextContent("CI bot");
    expect(row).toHaveTextContent("read");
    expect(row).toHaveTextContent("update");
    expect(within(row).getByTestId("api-key-expires")).toHaveTextContent("Never");
    expect(within(row).getByTestId("api-key-last-used")).toHaveTextContent("2026");
  });

  it("shows an expiry date and a never-used key in their own columns", async () => {
    listOne({ expires_at: "2999-03-15T12:00:00Z", last_used_at: null });
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(within(row).getByTestId("api-key-expires")).toHaveTextContent("2999");
    expect(within(row).getByTestId("api-key-last-used")).toHaveTextContent("Never");
  });

  // Catches a schema without created_by: valibot drops unknown keys, so the
  // column would have nothing to show.
  it("keeps created_by when it parses the key list, and asks for the bot's keys", async () => {
    list.mockRestore();
    const fetchMock = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response(JSON.stringify(KEYS), {
        status: 200,
        headers: { "content-type": "application/json" },
      }),
    );
    const r = await apiKeysApi.list("bot1");
    expect(fetchMock.mock.calls[0]?.[0]).toBe("/api/auth/api-keys?user_id=bot1");
    expect(r).toMatchObject({ ok: [{ id: "k1", created_by: "owner1" }] });
  });

  // Catches a lookup by the wrong id, or the raw id reaching the page.
  it("names who created each of a bot's keys", async () => {
    renderSection({ targetUserId: "bot1" });
    const row = await screen.findByTestId("api-key-row");
    expect(screen.getByRole("columnheader", { name: "Created by" })).toBeInTheDocument();
    await waitFor(() => expect(within(row).getByTestId("api-key-created-by")).toHaveTextContent("Olive Owner"));
  });

  // Catches a blank cell or a bare uuid for a creator who has left the workspace.
  it("falls back to Unknown for a creator who is not a member", async () => {
    listOne({ created_by: "gone1" });
    renderSection({ targetUserId: "bot1" });
    const row = await screen.findByTestId("api-key-row");
    await waitFor(() => expect(listMembers).toHaveBeenCalled());
    expect(within(row).getByTestId("api-key-created-by")).toHaveTextContent("Unknown");
  });

  // Catches the column, and the member fetch, appearing on one's own keys.
  it("leaves the creator out of one's own keys", async () => {
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(screen.queryByRole("columnheader", { name: "Created by" })).not.toBeInTheDocument();
    expect(within(row).queryByTestId("api-key-created-by")).not.toBeInTheDocument();
    expect(listMembers).not.toHaveBeenCalled();
  });

  // Catches an expired key that still offers Revoke.
  it("labels an expired key Expired and offers no Revoke", async () => {
    listOne({ expires_at: "2020-01-01T00:00:00Z" });
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(row).toHaveTextContent("Expired");
    expect(within(row).queryByTestId("api-key-revoke")).not.toBeInTheDocument();
  });

  // Catches expiry being checked first: a revoked key was stopped on purpose.
  it("labels a key that is both revoked and expired Revoked", async () => {
    listOne({ expires_at: "2020-01-01T00:00:00Z", revoked_at: "2019-06-01T00:00:00Z" });
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(row).toHaveTextContent("Revoked");
    expect(row).not.toHaveTextContent("Expired");
    expect(within(row).queryByTestId("api-key-revoke")).not.toBeInTheDocument();
  });

  // Catches a key with any expiry date being treated as expired.
  it("still offers Revoke on a key that expires in the future", async () => {
    listOne({ expires_at: "2999-03-15T12:00:00Z" });
    renderSection();
    const row = await screen.findByTestId("api-key-row");
    expect(row).not.toHaveTextContent("Expired");
    expect(within(row).getByTestId("api-key-revoke")).toBeInTheDocument();
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

  it("applies a capped owner's limit to a bot target, and passes user_id", async () => {
    maxTtl = 90;
    renderSection({ targetUserId: "bot1" });
    const select = await screen.findByTestId("api-key-expiry");
    expect(Array.from(select.querySelectorAll("option")).map((o) => o.value)).toEqual(["30", "90"]);
    expect(list).toHaveBeenCalledWith("bot1");
  });

  it("offers Never for a bot target when the owner is uncapped", async () => {
    renderSection({ targetUserId: "bot1" });
    const select = await screen.findByTestId("api-key-expiry");
    expect(Array.from(select.querySelectorAll("option")).map((o) => o.value)).toContain("never");
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

  // Catches the panel dropping targetUserId: the DELETE would aim at the
  // owner's own keys, and a stale list would keep showing the bot's key live.
  it("revokes a bot's key for that bot and invalidates that bot's list", async () => {
    const revoke = vi.spyOn(apiKeysApi, "revoke").mockResolvedValue({ ok: undefined });
    const { invalidate } = renderSection({ targetUserId: "bot1" });
    fireEvent.click(await screen.findByTestId("api-key-revoke"));
    fireEvent.click(screen.getByTestId("api-key-revoke-confirm"));
    await waitFor(() => expect(revoke).toHaveBeenCalledWith("k1", "bot1"));
    await waitFor(() => expect(invalidate).toHaveBeenCalledWith({ queryKey: ["api-keys", "bot1"] }));
  });

  // Catches apiKeysApi.revoke losing the user_id query: the server would then
  // look for the key among the owner's own and answer not found.
  it("sends a bot's revoke to the URL the server expects", async () => {
    const fetchMock = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response(null, { status: 204 }));
    await apiKeysApi.revoke("k1", "bot1");
    const call = fetchMock.mock.calls[0];
    expect(call?.[0]).toBe("/api/auth/api-keys/k1?user_id=bot1");
    expect(call?.[1]?.method).toBe("DELETE");
  });

  // Catches the plain-HTTP case: navigator.clipboard is undefined there, so
  // calling writeText throws instead of rejecting and no toast appears.
  it("reports a failed copy when the clipboard API is missing", async () => {
    setClipboard(undefined);
    await openReveal();
    fireEvent.click(screen.getByTestId("api-key-copy"));
    await waitFor(() =>
      expect(toastTexts("error")).toEqual(["Couldn't copy, select the key and copy it manually"]),
    );
    expect(toastTexts("info")).toEqual([]);
  });

  // Catches the failure path being lost for a clipboard that refuses the write.
  it("reports a failed copy when the clipboard refuses the write", async () => {
    setClipboard({ writeText: vi.fn().mockRejectedValue(new DOMException("denied", "NotAllowedError")) });
    await openReveal();
    fireEvent.click(screen.getByTestId("api-key-copy"));
    await waitFor(() =>
      expect(toastTexts("error")).toEqual(["Couldn't copy, select the key and copy it manually"]),
    );
    expect(toastTexts("info")).toEqual([]);
  });

  // Catches the failure toast firing on success, or the wrong text being copied.
  it("copies the key and confirms it", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    setClipboard({ writeText });
    await openReveal();
    fireEvent.click(screen.getByTestId("api-key-copy"));
    await waitFor(() => expect(toastTexts("info")).toEqual(["Copied key"]));
    expect(writeText).toHaveBeenCalledWith("knot_SECRET");
    expect(toastTexts("error")).toEqual([]);
  });

  it("says so when the key list can't be loaded, rather than looking empty", async () => {
    list.mockRejectedValue(new TypeError("Failed to fetch"));
    renderSection();
    expect(await screen.findByRole("alert")).toHaveTextContent("Couldn't load");
  });

  it("reports a revoke that fails on the network, and that the key still works", async () => {
    vi.spyOn(apiKeysApi, "revoke").mockRejectedValue(new TypeError("Failed to fetch"));
    renderSection();
    fireEvent.click(await screen.findByTestId("api-key-revoke"));
    fireEvent.click(screen.getByTestId("api-key-revoke-confirm"));
    await waitFor(() => {
      const errors = useUi.getState().toasts.filter((t) => t.kind === "error");
      expect(errors.map((t) => t.text)).toEqual([expect.stringContaining("still active")]);
    });
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

  it("mentions control characters when the server refuses a name", async () => {
    vi.spyOn(apiKeysApi, "create").mockResolvedValue({
      error: { code: "api_key.name_invalid", message: "", details: {}, status: 422 },
    });
    renderSection();
    fireEvent.change(await screen.findByTestId("api-key-name"), { target: { value: "deploy\tbot" } });
    fireEvent.click(screen.getByTestId("api-key-submit"));
    expect(await screen.findByTestId("api-key-error")).toHaveTextContent("control characters");
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
