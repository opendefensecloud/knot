import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { Member } from "../../lib/validators";

import MembersPage, { filterMembers } from "./MembersPage";
import { workspaceApi } from "./workspace.api";

vi.mock("../../auth/useEffectiveRole", () => ({
  useEffectiveRole: () => ({ workspace: "owner", doc: null }),
}));

const MEMBERS: Member[] = [
  { user_id: "u1", email: "anna@example.org", display_name: "Anna Schmidt", role: "owner" },
  { user_id: "u2", email: "bob@example.org", display_name: "Bob Meyer", role: "editor" },
  { user_id: "u3", email: "carla@other.net", display_name: "Carla Jansen", role: "viewer" },
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
