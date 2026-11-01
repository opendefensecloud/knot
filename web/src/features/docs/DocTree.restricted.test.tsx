import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { Toast } from "../../components/Toast";
import { useUi } from "../../stores/ui";

import { DocTree } from "./DocTree";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  useUi.setState({ toasts: [] });
});

type Role = "owner" | "editor" | "viewer";

const INSTRUCTIONS = "Drag to move. Use the row menu or Alt with the arrow keys to move by keyboard.";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(status === 204 ? null : JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

type Restriction = {
  restricted?: boolean;
  within_restricted?: boolean;
  parent_hidden?: boolean;
  effective_role?: Role | null;
};

function doc(id: string, title: string, parent_id: string | null, sort_key: string, extra: Restriction = {}) {
  return {
    id,
    workspace_id: "w1",
    parent_id,
    title,
    sort_key,
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
    restricted: false,
    within_restricted: false,
    parent_hidden: false,
    effective_role: null,
    ...extra,
  };
}

/** Plain; Secret (restricted) > Secret child; Deep (shared: its restricted
 *  parent is hidden). */
function pages(roles: { plain: Role; secret: Role; deep: Role }) {
  return [
    doc("plain", "Plain", null, "a", { effective_role: roles.plain }),
    doc("secret", "Secret", null, "b", {
      restricted: true,
      within_restricted: true,
      effective_role: roles.secret,
    }),
    doc("secret-child", "Secret child", "secret", "a", {
      within_restricted: true,
      effective_role: roles.secret,
    }),
    doc("deep", "Deep", null, "c", {
      within_restricted: true,
      parent_hidden: true,
      effective_role: roles.deep,
    }),
  ];
}

function server(workspaceRole: Role, list: unknown[], archiveReply?: { status: number; code: string }) {
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role: workspaceRole });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role: workspaceRole, archive_retention_days: 14 });
    }
    if (url === "/api/docs" && (!init?.method || init.method === "GET")) return json(list);
    if (init?.method === "DELETE" && archiveReply) {
      return json({ error: { code: archiveReply.code, message: "", details: {} } }, archiveReply.status);
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderTree(fetchMock: ReturnType<typeof server>) {
  vi.stubGlobal("fetch", fetchMock);
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter>
          <DocTree />
          <Toast />
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

/** The text of every element `el`'s aria-describedby names. */
function descriptions(el: HTMLElement): string[] {
  return (el.getAttribute("aria-describedby") ?? "")
    .split(/\s+/)
    .filter(Boolean)
    .map((id) => document.getElementById(id)?.textContent ?? `(no element #${id})`);
}

function moreButton(id: string): HTMLElement | null {
  return within(screen.getByTestId(`tree-row-${id}`)).queryByRole("button", { name: "More" });
}

describe("DocTree — restricted pages", () => {
  it("lists pages whose parent is hidden under 'Shared with you'", async () => {
    renderTree(server("editor", pages({ plain: "editor", secret: "owner", deep: "viewer" })));

    const shared = await screen.findByTestId("tree-shared");
    expect(shared).toHaveTextContent("Shared with you");
    expect(within(shared).getByTestId("doc-row-deep")).toBeInTheDocument();
    expect(within(shared).queryByTestId("doc-row-plain")).toBeNull();
    expect(within(shared).queryByTestId("doc-row-secret")).toBeNull();
  });

  it("has no 'Shared with you' section when nothing is shared", async () => {
    renderTree(server("editor", [doc("plain", "Plain", null, "a", { effective_role: "editor" })]));

    await screen.findByTestId("doc-row-plain");
    expect(screen.queryByTestId("tree-shared")).toBeNull();
  });

  it("locks restricted pages, and mutes the lock on pages inside one", async () => {
    renderTree(server("editor", pages({ plain: "editor", secret: "owner", deep: "viewer" })));

    const secretRow = await screen.findByTestId("tree-row-secret");
    expect(within(secretRow).getByLabelText("Restricted")).toBeInTheDocument();
    const childRow = screen.getByTestId("tree-row-secret-child");
    expect(within(childRow).getByLabelText("In a restricted page")).toBeInTheDocument();
    expect(within(childRow).queryByLabelText("Restricted")).toBeNull();
    expect(within(screen.getByTestId("tree-row-plain")).queryByLabelText(/restricted/i)).toBeNull();
    expect(descriptions(screen.getByTestId("doc-row-secret-child"))).toContain("level 2, restricted");
  });

  it("gates each row's actions by the role on that page, not the workspace role", async () => {
    // A workspace viewer who owns a restricted page.
    renderTree(server("viewer", pages({ plain: "viewer", secret: "owner", deep: "viewer" })));

    await waitFor(() => expect(moreButton("secret")).not.toBeNull());
    expect(moreButton("plain")).toBeNull();
    expect(moreButton("deep")).toBeNull();
    // The top-level "New document" keeps the workspace role.
    expect(screen.queryByTestId("new-doc")).toBeNull();

    fireEvent.click(moreButton("secret")!);
    const menu = screen.getByTestId("context-menu");
    expect(within(menu).getByTestId("ctx-permissions")).toBeInTheDocument();
    expect(within(menu).getByTestId("ctx-rename")).toBeInTheDocument();
    expect(within(menu).getByTestId("ctx-delete")).toBeInTheDocument();
  });

  it("gives an editor of a page no owner actions on it", async () => {
    renderTree(server("owner", pages({ plain: "owner", secret: "editor", deep: "viewer" })));

    await waitFor(() => expect(moreButton("secret")).not.toBeNull());
    fireEvent.click(moreButton("secret")!);
    const menu = screen.getByTestId("context-menu");
    expect(within(menu).getByTestId("ctx-rename")).toBeInTheDocument();
    expect(within(menu).queryByTestId("ctx-permissions")).toBeNull();
    expect(within(menu).queryByTestId("ctx-template")).toBeNull();
    // A workspace owner who only views the shared page gets no menu there.
    expect(moreButton("deep")).toBeNull();
  });

  it("doesn't offer a shared page for dragging or moving", async () => {
    renderTree(server("editor", pages({ plain: "editor", secret: "owner", deep: "owner" })));

    await waitFor(() => expect(moreButton("deep")).not.toBeNull());
    expect(descriptions(screen.getByTestId("doc-row-deep"))).not.toContain(INSTRUCTIONS);
    expect(descriptions(screen.getByTestId("doc-row-plain"))).toContain(INSTRUCTIONS);
    fireEvent.click(moreButton("deep")!);
    const menu = screen.getByTestId("context-menu");
    for (const item of ["ctx-move-up", "ctx-move-down", "ctx-indent", "ctx-outdent"]) {
      expect(within(menu).getByTestId(item), item).toBeDisabled();
    }
  });

  it("offers no Duplicate on a shared page, whose copy would go under its hidden parent", async () => {
    renderTree(server("editor", pages({ plain: "editor", secret: "owner", deep: "owner" })));

    await waitFor(() => expect(moreButton("deep")).not.toBeNull());
    fireEvent.click(moreButton("deep")!);
    expect(within(screen.getByTestId("context-menu")).queryByTestId("ctx-duplicate")).toBeNull();
    fireEvent.keyDown(screen.getByTestId("context-menu"), { key: "Escape" });
    fireEvent.click(moreButton("plain")!);
    expect(within(screen.getByTestId("context-menu")).getByTestId("ctx-duplicate")).toBeInTheDocument();
  });

  it("has no menu move to where the user can't add pages", async () => {
    // A workspace viewer who owns a restricted page can't add pages at the
    // top level, so its subpage can't be outdented there.
    renderTree(server("viewer", pages({ plain: "viewer", secret: "owner", deep: "viewer" })));

    await waitFor(() => expect(moreButton("secret-child")).not.toBeNull());
    fireEvent.click(moreButton("secret-child")!);
    expect(within(screen.getByTestId("context-menu")).getByTestId("ctx-outdent")).toBeDisabled();
  });

  it("says why archiving a page with hidden subpages was refused", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    renderTree(
      server("editor", pages({ plain: "editor", secret: "owner", deep: "viewer" }), {
        status: 409,
        code: "doc.hidden_descendants",
      }),
    );

    await waitFor(() => expect(moreButton("secret")).not.toBeNull());
    fireEvent.click(moreButton("secret")!);
    fireEvent.click(screen.getByTestId("ctx-delete"));
    await waitFor(() =>
      expect(screen.getByTestId("toast-error")).toHaveTextContent(
        "This page has subpages you can't see. Ask their owners to move them first.",
      ),
    );
  });
});
