import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useEffect } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import type { ConnStatus } from "../../components/StatusDot";
import { useUi } from "../../stores/ui";

// The editor needs a WebSocket; a stand-in that shows what it was given and
// can report a revoked connection, as KnotProvider does on close 4403.
const editorScript = vi.hoisted(() => ({ reportUnauthorised: false, mounts: 0 }));
vi.mock("../editor/KnotEditor", () => ({
  KnotEditor: (props: { role: string; editMode: boolean; onStatus: (s: ConnStatus) => void }) => {
    const { onStatus } = props;
    useEffect(() => {
      editorScript.mounts += 1;
      // Once, like a single revoke.
      if (editorScript.reportUnauthorised) {
        editorScript.reportUnauthorised = false;
        onStatus("unauthorised");
      }
    }, [onStatus]);
    return <div data-testid="editor" data-role={props.role} data-edit-mode={String(props.editMode)} />;
  },
}));

import { archiveErrorText } from "./archive";
import DocPage from "./DocPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  editorScript.reportUnauthorised = false;
  editorScript.mounts = 0;
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

type Role = "owner" | "editor" | "viewer";

function docBody(id: string, title: string, role: Role, archive: Record<string, unknown> | null) {
  return {
    id,
    workspace_id: "w1",
    parent_id: null,
    title,
    sort_key: "a",
    icon: null,
    created_by: "u-anna",
    archived: archive !== null,
    is_template: false,
    effective_role: role,
    archived_at: null,
    archived_by: null,
    purge_at: null,
    archive_root_id: null,
    archive_root_title: null,
    ...archive,
  };
}

const archivedRoot = {
  archived_at: "2026-10-12T09:00:00Z",
  archived_by: { id: "u-anna", display_name: "Anna" },
  purge_at: "2026-11-11T09:00:00Z",
  archive_root_id: "d1",
  archive_root_title: "Runbook",
};

/** `docs` maps an id to the bodies GET /api/docs/{id} returns, in order. */
function server(role: Role, docs: Record<string, unknown[]>) {
  const served: Record<string, number> = {};
  return vi.fn((url: string) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace/archive") return json([]);
    const m = /^\/api\/docs\/([^/]+)(\/contributors)?$/.exec(url);
    if (m && m[2]) {
      return json({
        created_by: { id: "u-anna", display_name: "Anna" },
        created_at: "2026-03-03T10:00:00Z",
        contributors_since: "2026-03-03T10:00:00Z",
        contributors: [],
      });
    }
    if (m && docs[m[1]!]) {
      const bodies = docs[m[1]!]!;
      const n = served[m[1]!] ?? 0;
      served[m[1]!] = n + 1;
      return json(bodies[Math.min(n, bodies.length - 1)]);
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function renderDoc(id: string, client = new QueryClient({ defaultOptions: { queries: { retry: false } } })) {
  render(
    <QueryClientProvider client={client}>
      <SessionProvider>
        <MemoryRouter initialEntries={[`/doc/${id}`]}>
          <Routes>
            <Route path="/" element={<p>home</p>} />
            <Route path="/doc/:id" element={<DocPage />}>
              <Route path="permissions" element={<p>permissions</p>} />
            </Route>
          </Routes>
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

describe("DocPage — archived", () => {
  it("shows an editor the archive banner with Restore and a read-only page", async () => {
    vi.stubGlobal("fetch", server("editor", { d1: [docBody("d1", "Runbook", "editor", archivedRoot)] }));
    renderDoc("d1");

    const banner = await screen.findByTestId("archived-banner");
    expect(banner).toHaveTextContent("In the archive");
    expect(banner).toHaveTextContent("Deleted by Anna");
    expect(within(banner).getByRole("button", { name: "Restore" })).toBeInTheDocument();
    expect(within(banner).queryByRole("button", { name: "Delete permanently" })).toBeNull();
    expect(screen.getByTestId("doc-title")).toHaveAttribute("readonly");
    for (const control of ["toggle-edit-mode", "open-history", "open-comments", "doc-import-md", "doc-export"]) {
      expect(screen.queryByTestId(control), control).toBeNull();
    }
    expect(screen.getByTestId("toggle-markdown")).toBeInTheDocument();
    const editor = await screen.findByTestId("editor");
    expect(editor).toHaveAttribute("data-role", "viewer");
    expect(editor).toHaveAttribute("data-edit-mode", "false");
  });

  it("offers owners permanent deletion too, and no share or template controls", async () => {
    vi.stubGlobal("fetch", server("owner", { d1: [docBody("d1", "Runbook", "owner", archivedRoot)] }));
    renderDoc("d1");

    const banner = await screen.findByTestId("archived-banner");
    expect(within(banner).getByRole("button", { name: "Delete permanently" })).toBeInTheDocument();
    expect(screen.queryByTestId("open-permissions")).toBeNull();
    expect(screen.queryByTestId("toggle-template")).toBeNull();
  });

  it("points a subpage at the folder it went into the archive with", async () => {
    const subpage = docBody("d2", "Step 1", "editor", {
      ...archivedRoot,
      archive_root_id: "d1",
      archive_root_title: "Runbook",
    });
    vi.stubGlobal("fetch", server("editor", { d2: [subpage] }));
    renderDoc("d2");

    const banner = await screen.findByTestId("archived-banner");
    expect(banner).toHaveTextContent("Part of “Runbook”, which is in the archive");
    expect(within(banner).getByRole("link", { name: "Runbook" })).toHaveAttribute("href", "/doc/d1");
    expect(within(banner).queryByRole("button")).toBeNull();
  });

  it("names no folder it can't show when the archived root is hidden", async () => {
    // The server nulls the root's id and title when the user can't read it.
    const subpage = docBody("d2", "Step 1", "editor", {
      ...archivedRoot,
      archive_root_id: null,
      archive_root_title: null,
    });
    vi.stubGlobal("fetch", server("editor", { d2: [subpage] }));
    renderDoc("d2");

    const banner = await screen.findByTestId("archived-banner");
    expect(banner).toHaveTextContent("In the archive with a page above it");
    expect(banner).not.toHaveTextContent("a folder");
    expect(within(banner).queryByRole("link")).toBeNull();
    expect(within(banner).queryByRole("button")).toBeNull();
  });

  it("says why a permanent deletion was refused for hidden subpages", async () => {
    const notify = vi.spyOn(useUi.getState(), "notify");
    vi.spyOn(window, "confirm").mockReturnValue(true);
    const answer = server("owner", { d1: [docBody("d1", "Runbook", "owner", archivedRoot)] });
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string, init?: RequestInit) =>
        url === "/api/docs/d1/purge" && init?.method === "POST"
          ? json({ error: { code: "doc.hidden_descendants", message: "", details: {} } }, 409)
          : answer(url),
      ),
    );
    renderDoc("d1");

    const banner = await screen.findByTestId("archived-banner");
    fireEvent.click(within(banner).getByRole("button", { name: "Delete permanently" }));
    await waitFor(() =>
      expect(notify).toHaveBeenCalledWith("error", archiveErrorText("doc.hidden_descendants")),
    );
  });

  it("stays on a page whose connection was revoked because it was archived", async () => {
    editorScript.reportUnauthorised = true;
    vi.stubGlobal(
      "fetch",
      server("editor", {
        d1: [docBody("d1", "Runbook", "editor", null), docBody("d1", "Runbook", "editor", archivedRoot)],
      }),
    );
    renderDoc("d1");

    expect(await screen.findByTestId("archived-banner")).toBeInTheDocument();
    await waitFor(() => expect(screen.getByTestId("editor")).toHaveAttribute("data-role", "viewer"));
    expect(screen.queryByText("home")).toBeNull();
  });
});

describe("DocPage — revoked connection", () => {
  // The server closes every socket on a doc (4403) whenever its access
  // changes: archive, restore, a grant. A doc that is still readable stays
  // open and reconnects; restoring from the banner must not kick the user
  // out with "You no longer have access".
  it("reconnects and stays when the doc is still readable", async () => {
    editorScript.reportUnauthorised = true;
    vi.stubGlobal("fetch", server("editor", { d1: [docBody("d1", "Runbook", "editor", null)] }));
    renderDoc("d1");

    await waitFor(() => expect(editorScript.mounts).toBe(2));
    expect(screen.getByTestId("doc-page")).toBeInTheDocument();
    expect(screen.queryByText("home")).toBeNull();
  });

  it("leaves when the doc can no longer be read", async () => {
    editorScript.reportUnauthorised = true;
    let served = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/api/docs/d1") {
          served += 1;
          return served === 1
            ? json(docBody("d1", "Runbook", "editor", null))
            : json({ error: { code: "acl.no_grant", message: "", details: {} } }, 403);
        }
        return server("editor", {})(url);
      }),
    );
    renderDoc("d1");

    expect(await screen.findByText("home")).toBeInTheDocument();
  });
});

describe("DocPage — export menu", () => {
  it("readers see the export menu", async () => {
    vi.stubGlobal("fetch", server("viewer", { d1: [docBody("d1", "Runbook", "viewer", null)] }));
    renderDoc("d1");

    expect(await screen.findByTestId("doc-export")).toHaveAttribute("aria-haspopup", "menu");
  });
});

describe("DocPage — restricted", () => {
  const restrictedBoundary = { restricted: true, within_restricted: true, restriction_root_id: "d1" };

  it("shows owners a Restricted chip that opens the permissions", async () => {
    vi.stubGlobal(
      "fetch",
      server("viewer", { d1: [{ ...docBody("d1", "Payroll", "owner", null), ...restrictedBoundary }] }),
    );
    renderDoc("d1");

    const chip = await screen.findByTestId("restricted-chip");
    expect(chip).toHaveTextContent("Restricted");
    expect(chip).toHaveAttribute("href", "/doc/d1/permissions");
  });

  it("shows other readers the chip without a link", async () => {
    vi.stubGlobal(
      "fetch",
      server("owner", {
        d2: [{ ...docBody("d2", "Salaries", "viewer", null), within_restricted: true, restriction_root_id: null }],
      }),
    );
    renderDoc("d2");

    const chip = await screen.findByTestId("restricted-chip");
    expect(chip).toHaveTextContent("Restricted");
    expect(chip).not.toHaveAttribute("href");
  });

  it("shows no chip on a page outside any restricted page", async () => {
    vi.stubGlobal("fetch", server("owner", { d1: [docBody("d1", "Runbook", "owner", null)] }));
    renderDoc("d1");

    await screen.findByTestId("doc-title");
    expect(screen.queryByTestId("restricted-chip")).toBeNull();
  });

  it("says the same thing for a page that doesn't exist and one the user can't read", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) =>
        url === "/api/docs/d1"
          ? json({ error: { code: "acl.no_grant", message: "", details: {} } }, 403)
          : server("editor", {})(url),
      ),
    );
    renderDoc("d1");

    expect(
      await screen.findByText("This page doesn't exist or you don't have access to it."),
    ).toBeInTheDocument();
  });

  it("refreshes the sidebar and forgets the page before leaving one it lost access to", async () => {
    editorScript.reportUnauthorised = true;
    let served = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/api/docs/d1") {
          served += 1;
          return served === 1
            ? json(docBody("d1", "Runbook", "editor", null))
            : json({ error: { code: "acl.no_grant", message: "", details: {} } }, 403);
        }
        return server("editor", {})(url);
      }),
    );
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const invalidate = vi.spyOn(client, "invalidateQueries");
    const remove = vi.spyOn(client, "removeQueries");
    renderDoc("d1", client);

    expect(await screen.findByText("home")).toBeInTheDocument();
    expect(invalidate).toHaveBeenCalledWith({ queryKey: ["docs"] });
    expect(remove).toHaveBeenCalledWith({ queryKey: ["doc", "d1"] });
  });

  it("leaves with a sidebar list that no longer opens the lost page or its subpages", async () => {
    // The landing page opens the list's first page: a stale list would send
    // the user straight back to the page they lost.
    editorScript.reportUnauthorised = true;
    let served = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url === "/api/docs/d1") {
          served += 1;
          return served === 1
            ? json(docBody("d1", "Runbook", "editor", null))
            : json({ error: { code: "acl.no_grant", message: "", details: {} } }, 403);
        }
        return server("editor", {})(url);
      }),
    );
    const client = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    client.setQueryData(["docs"], {
      ok: [
        { ...docBody("d1", "Runbook", "editor", null), restricted: true, within_restricted: true },
        { ...docBody("d3", "Step", "editor", null), parent_id: "d1", within_restricted: true },
        docBody("d2", "Handbook", "editor", null),
      ],
    });
    let listedWhenHome: string[] | null = null;
    function Home() {
      const cached = client.getQueryData<{ ok: { id: string }[] }>(["docs"]);
      listedWhenHome ??= cached ? cached.ok.map((d) => d.id) : [];
      return <p>home</p>;
    }
    render(
      <QueryClientProvider client={client}>
        <SessionProvider>
          <MemoryRouter initialEntries={["/doc/d1"]}>
            <Routes>
              <Route path="/" element={<Home />} />
              <Route path="/doc/:id" element={<DocPage />} />
            </Routes>
          </MemoryRouter>
        </SessionProvider>
      </QueryClientProvider>,
    );

    expect(await screen.findByText("home")).toBeInTheDocument();
    expect(listedWhenHome).toEqual(["d2"]);
  });
});
