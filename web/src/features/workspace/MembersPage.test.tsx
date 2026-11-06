import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Member } from "../../lib/validators";

import { apiKeysApi } from "../apiKeys/apiKeys.api";
import MembersPage, { filterMembers } from "./MembersPage";
import { workspaceApi } from "./workspace.api";

vi.mock("../../auth/SessionContext", () => ({
  useSession: () => ({ data: undefined }),
}));

vi.mock("../../auth/useEffectiveRole", () => ({
  useEffectiveRole: () => ({ workspace: "owner", doc: null }),
}));

const MEMBERS: Member[] = [
  { user_id: "u1", email: "anna@example.org", display_name: "Anna Schmidt", role: "owner", kind: "human" },
  { user_id: "u2", email: "bob@example.org", display_name: "Bob Meyer", role: "editor", kind: "human" },
  { user_id: "u3", email: "carla@other.net", display_name: "Carla Jansen", role: "viewer", kind: "human" },
];

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("filterMembers", () => {
  it("returns everyone for an empty or blank query", () => {
    expect(filterMembers(MEMBERS, "")).toEqual(MEMBERS);
    expect(filterMembers(MEMBERS, "   ")).toEqual(MEMBERS);
  });

  it("matches display name and email case-insensitively", () => {
    expect(filterMembers(MEMBERS, "SCHMIDT").map((m) => m.user_id)).toEqual(["u1"]);
    expect(filterMembers(MEMBERS, "example.org").map((m) => m.user_id)).toEqual(["u1", "u2"]);
  });

  it("matches role", () => {
    expect(filterMembers(MEMBERS, "viewer").map((m) => m.user_id)).toEqual(["u3"]);
  });
});

describe("MembersPage search", () => {
  beforeEach(() => {
    vi.spyOn(workspaceApi, "listMembers").mockResolvedValue({ ok: MEMBERS });
  });

  function renderPage() {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <QueryClientProvider client={qc}>
        <MembersPage />
      </QueryClientProvider>,
    );
  }

  it("narrows the table as you type and shows the match count", async () => {
    renderPage();
    const search = await screen.findByTestId("members-search");
    expect(screen.getByTestId("members-count")).toHaveTextContent("3 members");

    fireEvent.change(search, { target: { value: "bob" } });
    expect(screen.getByTestId("member-u2")).toBeInTheDocument();
    expect(screen.queryByTestId("member-u1")).not.toBeInTheDocument();
    expect(screen.queryByTestId("member-u3")).not.toBeInTheDocument();
    expect(screen.getByTestId("members-count")).toHaveTextContent("1 of 3");
  });

  it("shows an empty state when nothing matches, and Escape clears the query", async () => {
    renderPage();
    const search = await screen.findByTestId("members-search");

    fireEvent.change(search, { target: { value: "nobody" } });
    expect(screen.getByTestId("members-empty")).toHaveTextContent("No members match “nobody”.");

    fireEvent.keyDown(search, { key: "Escape" });
    expect(search).toHaveValue("");
    expect(screen.queryByTestId("members-empty")).not.toBeInTheDocument();
    expect(screen.getByTestId("member-u1")).toBeInTheDocument();
  });
});

describe("MembersPage remove", () => {
  beforeEach(() => {
    vi.spyOn(workspaceApi, "listMembers").mockResolvedValue({ ok: MEMBERS });
  });

  it("warns that a removed member loses their page grants too", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    const remove = vi.spyOn(workspaceApi, "remove");
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MembersPage />
      </QueryClientProvider>,
    );

    const row = await screen.findByTestId("member-u2");
    fireEvent.click(within(row).getByRole("button", { name: "Remove" }));
    expect(confirm).toHaveBeenCalledWith(
      "Remove bob@example.org? They also lose every page-level grant, including access to restricted pages.",
    );
    expect(remove).not.toHaveBeenCalled();
  });
});

describe("MembersPage bots", () => {
  const BOT: Member = { user_id: "b1", email: "ci@bots.example.org", display_name: "CI", role: "editor", kind: "bot" };

  function renderPage() {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    return render(
      <QueryClientProvider client={qc}>
        <MembersPage />
      </QueryClientProvider>,
    );
  }

  beforeEach(() => {
    vi.spyOn(workspaceApi, "listMembers").mockResolvedValue({ ok: [...MEMBERS, BOT] });
  });

  it("a bot invite sends kind bot, no password, and never the owner role", async () => {
    const invite = vi.spyOn(workspaceApi, "invite").mockResolvedValue({ ok: undefined });
    renderPage();
    const form = await screen.findByTestId("invite-form");
    fireEvent.change(screen.getByTestId("invite-email"), { target: { value: "ci@bots.example.org" } });
    fireEvent.change(screen.getByTestId("invite-role"), { target: { value: "owner" } });
    fireEvent.change(screen.getByTestId("invite-password"), { target: { value: "secret123" } });

    fireEvent.click(screen.getByTestId("invite-kind-bot"));
    expect(screen.queryByTestId("invite-password")).not.toBeInTheDocument();
    expect(screen.getByTestId("invite-bot-hint")).toHaveTextContent("API key");
    const roleSelect = screen.getByTestId("invite-role");
    expect(within(roleSelect).queryByRole("option", { name: "Owner" })).not.toBeInTheDocument();
    expect(roleSelect).toHaveValue("editor");

    fireEvent.submit(form);
    await waitFor(() => expect(invite).toHaveBeenCalledTimes(1));
    expect(invite).toHaveBeenCalledWith("ci@bots.example.org", "editor", undefined, undefined, "bot");
  });

  it("renders the bot badge and offers no owner role on a bot row", async () => {
    renderPage();
    const row = await screen.findByTestId("member-b1");
    expect(within(row).getByTestId("member-bot-badge")).toHaveTextContent("Bot");
    expect(within(row).queryByRole("option", { name: "Owner" })).not.toBeInTheDocument();
    expect(within(screen.getByTestId("member-u2")).queryByTestId("member-bot-badge")).not.toBeInTheDocument();
    expect(within(screen.getByTestId("member-u2")).getByRole("option", { name: "Owner" })).toBeInTheDocument();
  });

  it("opens the API keys section for a bot row", async () => {
    const list = vi.spyOn(apiKeysApi, "list").mockResolvedValue({ ok: [] });
    renderPage();
    const row = await screen.findByTestId("member-b1");
    expect(within(screen.getByTestId("member-u2")).queryByTestId("member-api-keys")).not.toBeInTheDocument();
    fireEvent.click(within(row).getByTestId("member-api-keys"));
    expect(await screen.findByTestId("member-api-keys-panel-b1")).toBeInTheDocument();
    await waitFor(() => expect(list).toHaveBeenCalledWith("b1"));
  });
});
