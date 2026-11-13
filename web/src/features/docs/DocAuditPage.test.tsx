import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { ApiResult } from "../../lib/api";
import type { DocAuditEvent, DocAuditPage as DocAuditPageBody, DocAuditStats } from "../../lib/validators";

const auditMock = vi.hoisted(() => ({ list: vi.fn() }));
vi.mock("./audit.api", () => ({ auditApi: { list: auditMock.list } }));

import { SessionProvider } from "../../auth/SessionContext";
import type { AuditListOptions } from "./audit.api";
import DocAuditPage from "./DocAuditPage";

type Role = "owner" | "editor" | "viewer";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } }),
  );
}

function docBody(role: Role, archivedAt: string | null = null) {
  return {
    id: "d1",
    workspace_id: "w1",
    parent_id: null,
    title: "Runbook",
    sort_key: "a",
    icon: null,
    created_by: "u-anna",
    archived: archivedAt !== null,
    is_template: false,
    effective_role: role,
    archived_at: archivedAt,
    archived_by: null,
    purge_at: null,
    archive_root_id: null,
    archive_root_title: null,
  };
}

function stubServer(role: Role, archivedAt: string | null = null) {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url === "/auth/session") {
        return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role: "owner" });
      }
      if (url === "/api/docs/d1") return json(docBody(role, archivedAt));
      return json({ error: { code: "not_found", message: url, details: {} } }, 404);
    }),
  );
}

const HOUR_AGO = new Date(Date.now() - 2 * 3_600_000).toISOString();

const ADA = { key: "u-ada", name: "Ada", former: false };
const BO = { key: "u-bo", name: "Bo", former: false };
const FORMER = { key: null, name: null, former: true };

const EXPORT_EVENT: DocAuditEvent = {
  id: "5c1e9a07d3b4f268",
  at: HOUR_AGO,
  action: "doc.export",
  category: "exports",
  actor_kind: "user",
  actor: ADA,
  details: { format: "docx", scope: "page", diagram_pictures: 2 },
};
const SUBTREE_EVENT: DocAuditEvent = {
  id: "e07b42c9a1d65f30",
  at: HOUR_AGO,
  action: "doc.export",
  category: "exports",
  actor_kind: "user",
  actor: BO,
  details: { format: "markdown_zip", scope: "subtree", root: { id: "d-root", title: "Handbook" }, root_hidden: false },
};
const GRANT_EVENT: DocAuditEvent = {
  id: "9d2f6a18c4e07b35",
  at: HOUR_AGO,
  action: "doc.grant",
  category: "access",
  actor_kind: "user",
  actor: BO,
  details: { principal: { kind: "user", person: { key: "u-cy", name: "Cy", former: false } }, role: "editor", inherit: true },
};
const BREAK_GLASS_EVENT: DocAuditEvent = {
  id: "26b8e0f4a9c13d57",
  at: HOUR_AGO,
  action: "admin.grant",
  category: "access",
  actor_kind: "admin_cli",
  actor: null,
  details: { principal: { kind: "group", name: "ops" }, role: "owner", inherit: false, break_glass: true },
};
const OLDER_EVENT: DocAuditEvent = {
  id: "f1a7c3e95b20d864",
  at: HOUR_AGO,
  action: "doc.create",
  category: "changes",
  actor_kind: "user",
  actor: FORMER,
  details: {},
};

const STATS: DocAuditStats = {
  total: 37,
  by_action: [
    { action: "doc.export", count: 9, last_at: HOUR_AGO },
    { action: "doc.grant", count: 3, last_at: HOUR_AGO },
    { action: "doc.rename", count: 25, last_at: HOUR_AGO },
  ],
  exports: {
    total: 9,
    people: 3,
    last_30_days: 4,
    last: { at: HOUR_AGO, actor_kind: "user", actor: ADA, format: "docx", scope: "page" },
    by_format: { docx: 5, odt: 1, markdown_zip: 3 },
    by_scope: { page: 7, subtree: 1, workspace: 1 },
    by_person: [
      { actor: ADA, count: 6, last_at: HOUR_AGO },
      { actor: FORMER, count: 3, last_at: HOUR_AGO },
    ],
    by_person_truncated: false,
  },
};

const EMPTY_STATS: DocAuditStats = {
  total: 0,
  by_action: [],
  exports: {
    total: 0,
    people: 0,
    last_30_days: 0,
    last: null,
    by_format: {},
    by_scope: {},
    by_person: [],
    by_person_truncated: false,
  },
};

function page(events: DocAuditEvent[], nextCursor: string | null, stats: DocAuditStats | null): ApiResult<DocAuditPageBody> {
  return { ok: { events, next_cursor: nextCursor, stats } };
}

function apiError(status: number, code: string): ApiResult<DocAuditPageBody> {
  return { error: { code, message: code, details: {}, status } };
}

/** The log the server would send for each category and cursor. */
function serveLog(handler: (options: AuditListOptions) => ApiResult<DocAuditPageBody>) {
  auditMock.list.mockImplementation((_docId: string, options: AuditListOptions = {}) =>
    Promise.resolve(handler(options)),
  );
}

function renderPage() {
  const queryClient = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={queryClient}>
      <SessionProvider>
        <MemoryRouter initialEntries={["/doc/d1/audit"]}>
          <Routes>
            <Route path="/doc/:id" element={<p>the page</p>} />
            <Route path="/doc/:id/audit" element={<DocAuditPage />} />
          </Routes>
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  auditMock.list.mockReset();
});
afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe("DocAuditPage", () => {
  it("shows the export stats first", async () => {
    stubServer("editor");
    serveLog(() => page([EXPORT_EVENT], null, STATS));
    renderPage();

    const stats = await screen.findByTestId("audit-stats");
    expect(within(stats).getByTestId("audit-stat-exports")).toHaveTextContent("9");
    expect(within(stats).getByTestId("audit-stat-people")).toHaveTextContent("3");
    expect(within(stats).getByTestId("audit-stat-30d")).toHaveTextContent("4");
    const lastExport = within(stats).getByTestId("audit-stat-last");
    expect(lastExport).toHaveTextContent("2h ago");
    expect(lastExport).toHaveTextContent("Word · Ada");
    expect(within(lastExport).getByText("2h ago").closest("time")).toHaveAttribute("dateTime", HOUR_AGO);
    expect(within(stats).getByTestId("audit-formats")).toHaveTextContent(
      "Word 5OpenDocument 1Markdown (zip) 3",
    );
    expect(stats).toHaveTextContent("1 as part of a subpage export · 1 in a workspace export");
    expect(screen.getByTestId("audit-other-activity")).toHaveTextContent("Access changes 3 · Changes 25");
    expect(screen.getByTestId("audit-other-activity")).toHaveTextContent("37 events in total");
    expect(auditMock.list).toHaveBeenCalledWith("d1", { category: "all", cursor: undefined });
  });

  it("lists the exporters, a former member as such", async () => {
    stubServer("owner");
    serveLog(() => page([], null, { ...STATS, exports: { ...STATS.exports, by_person_truncated: true } }));
    renderPage();

    const exporters = await screen.findByTestId("audit-exporters");
    const rows = within(exporters).getAllByRole("listitem");
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent("Ada");
    expect(rows[0]).toHaveTextContent("6 exports");
    expect(rows[0]).toHaveTextContent("last 2h ago");
    expect(rows[1]).toHaveTextContent("Former member");
    expect(rows[1]).toHaveTextContent("3 exports");
    expect(screen.getByText("Showing the 50 most recent exporters.")).toBeInTheDocument();
  });

  it("describes each event in a sentence with its time", async () => {
    stubServer("editor");
    serveLog(() => page([EXPORT_EVENT, SUBTREE_EVENT, GRANT_EVENT, BREAK_GLASS_EVENT], null, STATS));
    renderPage();

    const events = await screen.findAllByTestId("audit-event");
    expect(events.map((row) => row.getAttribute("data-action"))).toEqual([
      "doc.export",
      "doc.export",
      "doc.grant",
      "admin.grant",
    ]);
    expect(events[0]).toHaveTextContent("Ada exported this page as Word (.docx), with 2 diagrams");
    expect(events[1]).toHaveTextContent("Bo exported this page as part of Handbook and its subpages");
    expect(within(events[1]!).getByRole("link", { name: "Handbook" })).toHaveAttribute("href", "/doc/d-root");
    expect(events[2]).toHaveTextContent("Bo gave Cy editor access");
    expect(events[3]).toHaveTextContent(
      "An administrator (command line) granted group ops owner access (break-glass)",
    );
    const time = events[0]!.querySelector("time");
    expect(time).toHaveAttribute("dateTime", HOUR_AGO);
    expect(time).toHaveAttribute("title", new Date(HOUR_AGO).toLocaleString());
    expect(time).toHaveTextContent("2h ago");
  });

  it("filters by category", async () => {
    stubServer("editor");
    serveLog((options) =>
      options.category === "exports" ? page([EXPORT_EVENT], null, STATS) : page([EXPORT_EVENT, GRANT_EVENT], null, STATS),
    );
    renderPage();

    await waitFor(() => expect(screen.getAllByTestId("audit-event")).toHaveLength(2));
    const filters = screen.getByRole("group", { name: "Show events" });
    expect(within(filters).getByTestId("audit-filter-all")).toHaveAttribute("aria-pressed", "true");
    fireEvent.click(within(filters).getByTestId("audit-filter-exports"));

    await waitFor(() => expect(screen.getAllByTestId("audit-event")).toHaveLength(1));
    expect(auditMock.list).toHaveBeenLastCalledWith("d1", { category: "exports", cursor: undefined });
    expect(screen.getByTestId("audit-filter-exports")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("audit-filter-all")).toHaveAttribute("aria-pressed", "false");
  });

  it("loads older events from the cursor", async () => {
    stubServer("editor");
    serveLog((options) =>
      options.cursor === "26b8e0f4a9c13d57"
        ? page([OLDER_EVENT], null, null)
        : page([EXPORT_EVENT, BREAK_GLASS_EVENT], "26b8e0f4a9c13d57", STATS),
    );
    renderPage();

    fireEvent.click(await screen.findByTestId("audit-load-more"));

    await waitFor(() => expect(screen.getAllByTestId("audit-event")).toHaveLength(3));
    expect(auditMock.list).toHaveBeenLastCalledWith("d1", { category: "all", cursor: "26b8e0f4a9c13d57" });
    expect(screen.getAllByTestId("audit-event")[2]).toHaveTextContent("Former member created this page");
    expect(screen.queryByTestId("audit-load-more")).toBeNull();
    // The stats stay those of the first page.
    expect(screen.getByTestId("audit-stat-exports")).toHaveTextContent("9");
  });

  it("refuses a viewer the log the server refused", async () => {
    stubServer("editor");
    serveLog(() => apiError(403, "acl.editor_required"));
    renderPage();

    const forbidden = await screen.findByTestId("audit-forbidden");
    expect(forbidden).toHaveTextContent("Only editors and owners of this page can see its audit log.");
    expect(within(forbidden).getByRole("link", { name: "Back to page" })).toHaveAttribute("href", "/doc/d1");
  });

  it("doesn't ask the server for a viewer's log", async () => {
    stubServer("viewer");
    serveLog(() => page([], null, EMPTY_STATS));
    renderPage();

    expect(await screen.findByTestId("audit-forbidden")).toBeInTheDocument();
    expect(auditMock.list).not.toHaveBeenCalled();
    expect(screen.queryByTestId("audit-stats")).toBeNull();
  });

  it.each(["acl.no_grant", "doc.not_found"])("says the same for %s", async (code) => {
    stubServer("editor");
    serveLog(() => apiError(code === "doc.not_found" ? 404 : 403, code));
    renderPage();

    expect(await screen.findByTestId("audit-no-access")).toHaveTextContent(
      "This page doesn't exist or you don't have access to it.",
    );
  });

  it("offers to try again after any other failure", async () => {
    stubServer("editor");
    let failNext = true;
    serveLog(() => {
      if (failNext) {
        failNext = false;
        return apiError(500, "internal");
      }
      return page([EXPORT_EVENT], null, STATS);
    });
    renderPage();

    expect(await screen.findByText("Couldn't load the audit log.")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("audit-retry"));

    expect(await screen.findByTestId("audit-stats")).toBeInTheDocument();
    expect(screen.queryByText("Couldn't load the audit log.")).toBeNull();
  });

  it("keeps the log and its filters when switching the filter fails", async () => {
    stubServer("editor");
    let exportsFail = true;
    serveLog((options) => {
      if (options.category === "exports") {
        return exportsFail ? apiError(500, "internal") : page([EXPORT_EVENT], null, STATS);
      }
      return page([EXPORT_EVENT, GRANT_EVENT], null, STATS);
    });
    renderPage();

    await waitFor(() => expect(screen.getAllByTestId("audit-event")).toHaveLength(2));
    fireEvent.click(screen.getByTestId("audit-filter-exports"));

    const failure = await screen.findByTestId("audit-events-failed");
    expect(failure).toHaveTextContent("Couldn't load these events.");
    expect(screen.queryByTestId("audit-retry")).toBeNull();
    expect(screen.queryByText("Couldn't load the audit log.")).toBeNull();
    // The stats and the filters stay; no other filter's events stand in.
    expect(screen.getByTestId("audit-stat-exports")).toHaveTextContent("9");
    expect(screen.getByTestId("audit-filter-exports")).toHaveAttribute("aria-pressed", "true");
    expect(screen.queryAllByTestId("audit-event")).toHaveLength(0);

    // Back to All, its list is there again...
    fireEvent.click(screen.getByTestId("audit-filter-all"));
    await waitFor(() => expect(screen.getAllByTestId("audit-event")).toHaveLength(2));
    expect(screen.queryByTestId("audit-events-failed")).toBeNull();

    // ...and trying Exports again loads it once the server answers.
    fireEvent.click(screen.getByTestId("audit-filter-exports"));
    exportsFail = false;
    fireEvent.click(await screen.findByTestId("audit-events-retry"));
    await waitFor(() => expect(screen.getAllByTestId("audit-event")).toHaveLength(1));
    expect(screen.queryByTestId("audit-events-failed")).toBeNull();
  });

  it("says when nothing was exported or recorded", async () => {
    stubServer("editor");
    serveLog(() => page([], null, EMPTY_STATS));
    renderPage();

    expect(await screen.findByTestId("audit-exports-empty")).toHaveTextContent(
      "Nobody has exported this page since exports started being recorded.",
    );
    expect(screen.getByTestId("audit-stat-last")).toHaveTextContent("Never");
    expect(screen.getByTestId("audit-events")).toHaveTextContent("Nothing recorded yet.");
    expect(screen.queryByTestId("audit-exporters")).toBeNull();
  });

  it.each([
    ["exports", "No exports recorded."],
    ["access", "No access changes recorded."],
    ["changes", "No changes recorded."],
  ])("says when the %s filter has nothing", async (category, emptyText) => {
    stubServer("editor");
    serveLog((options) => (options.category === category ? page([], null, STATS) : page([GRANT_EVENT], null, STATS)));
    renderPage();

    fireEvent.click(await screen.findByTestId(`audit-filter-${category}`));

    await waitFor(() => expect(screen.getByTestId("audit-events")).toHaveTextContent(emptyText));
  });

  it("states what is and isn't recorded, and names the page", async () => {
    stubServer("editor", "2026-11-01T09:00:00Z");
    serveLog(() => page([], null, EMPTY_STATS));
    renderPage();

    const scopeNote = await screen.findByTestId("audit-scope-note");
    expect(scopeNote).toHaveTextContent("Not recorded: opening or reading the page");
    expect(scopeNote).toHaveTextContent("Exports made before this version of knot were not recorded.");
    expect(screen.getByRole("heading", { level: 1, name: "Audit log" })).toBeInTheDocument();
    const breadcrumb = screen.getByRole("navigation", { name: "Breadcrumb" });
    await waitFor(() =>
      expect(within(breadcrumb).getByRole("link", { name: "Runbook" })).toHaveAttribute("href", "/doc/d1"),
    );
    expect(screen.getByTestId("audit-archived")).toHaveTextContent("Archived");
  });
});
