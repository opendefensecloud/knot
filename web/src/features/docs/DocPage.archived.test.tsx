import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { useEffect } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import type { ConnStatus } from "../../components/StatusDot";

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

import DocPage from "./DocPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  editorScript.reportUnauthorised = false;
  editorScript.mounts = 0;
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

type Role = "owner" | "editor";

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

function renderDoc(id: string) {
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter initialEntries={[`/doc/${id}`]}>
          <Routes>
            <Route path="/" element={<p>home</p>} />
            <Route path="/doc/:id" element={<DocPage />} />
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
