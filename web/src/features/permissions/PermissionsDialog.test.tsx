import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { Toast } from "../../components/Toast";
import { useUi } from "../../stores/ui";

import type { DocAccessInfo } from "../../lib/validators";

import PermissionsDialog from "./PermissionsDialog";

beforeEach(() => {
  useUi.setState({ toasts: [] });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

const ADA = "11111111-1111-1111-1111-111111111111";
const GONE = "22222222-2222-2222-2222-222222222222";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(status === 204 ? null : JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

/** `GET /api/docs/d1` for an owner, outside any restricted subtree. */
const DOC = {
  id: "d1",
  workspace_id: "w1",
  parent_id: null,
  title: "Plan",
  sort_key: "n",
  icon: null,
  created_by: ADA,
  archived: false,
  effective_role: "owner",
  restricted: false,
  within_restricted: false,
  restriction_root_id: null as string | null,
};

/** `GET /api/docs/d1/access` for DOC. */
const ACCESS: DocAccessInfo = {
  restricted: false,
  restriction_root_id: null,
  restriction_root_title: null,
  people: [],
};

type Call = { url: string; method: string; body: unknown };

type Stub = {
  doc?: Partial<typeof DOC>;
  access?: Partial<DocAccessInfo>;
  /** Answer to any PUT or DELETE; defaults to 204. */
  write?: { status: number; body?: unknown };
};

function renderDialog(stub: Stub = {}) {
  const calls: Call[] = [];
  let currentDoc = { ...DOC, ...stub.doc };
  let currentAccess = { ...ACCESS, ...stub.access };
  vi.stubGlobal("fetch", (url: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? (JSON.parse(init.body) as unknown) : undefined;
    calls.push({ url, method, body });
    if (method === "PUT" && url.endsWith("/restricted")) {
      const restricted = (body as { restricted: boolean }).restricted;
      currentDoc = { ...currentDoc, restricted, within_restricted: restricted };
      currentAccess = {
        ...currentAccess,
        restricted,
        restriction_root_id: restricted ? "d1" : null,
        restriction_root_title: restricted ? "Plan" : null,
      };
      return json(currentDoc);
    }
    if (method !== "GET") {
      const write = stub.write ?? { status: 204 };
      return json(write.body ?? null, write.status);
    }
    if (url.endsWith("/grants")) {
      return json([
        { principal: `user:${ADA}`, role: "editor", inherit: true },
        { principal: `user:${GONE}`, role: "viewer", inherit: false },
      ]);
    }
    if (url.endsWith("/api/workspace/members")) {
      return json([{ user_id: ADA, email: "ada@example.com", display_name: "Ada Lovelace", role: "editor" }]);
    }
    if (url.endsWith("/shares")) return json([]);
    if (url.endsWith("/api/docs/d1/access")) return json(currentAccess);
    if (url.endsWith("/api/docs/d1")) return json(currentDoc);
    return json(null);
  });
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={["/doc/d1/permissions"]}>
        <Routes>
          <Route path="/doc/:id/permissions" element={<><PermissionsDialog /><Toast /></>} />
        </Routes>
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return calls;
}

describe("PermissionsDialog grants", () => {
  it("shows a member's name and email instead of the user id", async () => {
    renderDialog();
    const row = await screen.findByTestId(`grant-user:${ADA}`);
    expect(await within(row).findByText("Ada Lovelace")).toBeTruthy();
    expect(within(row).getByText("ada@example.com")).toBeTruthy();
    expect(row.textContent).not.toContain(ADA);
  });

  it("falls back to the id for someone who is no longer a member", async () => {
    renderDialog();
    const row = await screen.findByTestId(`grant-user:${GONE}`);
    expect(await within(row).findByText("Former member")).toBeTruthy();
    expect(row.textContent).toContain(GONE);
  });
});

describe("PermissionsDialog access", () => {
  it("shows restrict toggle off and enables with confirm", async () => {
    const confirm = vi.fn(() => true);
    vi.stubGlobal("confirm", confirm);
    const calls = renderDialog();

    const toggle = await screen.findByTestId("restricted-toggle");
    await waitFor(() => expect(toggle).toHaveProperty("disabled", false));
    expect(toggle.getAttribute("role")).toBe("switch");
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    expect(
      screen.getByText("Everyone in the workspace can see this page according to their workspace role."),
    ).toBeTruthy();

    fireEvent.click(toggle);

    expect(confirm).toHaveBeenCalledWith(
      "Only people you add will keep access to this page and its subpages. You will be added as an owner.",
    );
    await waitFor(() => expect(toggle.getAttribute("aria-checked")).toBe("true"));
    const put = calls.find((call) => call.method === "PUT");
    expect(put?.url).toBe("/api/docs/d1/restricted");
    expect(put?.body).toEqual({ restricted: true });
    // The dialog read the doc and its access again after the toggle.
    const putIndex = calls.indexOf(put!);
    const refetched = calls.slice(putIndex + 1).map((call) => call.url);
    expect(refetched).toContain("/api/docs/d1");
    expect(refetched).toContain("/api/docs/d1/access");
    expect(refetched).toContain("/api/docs/d1/grants");
    expect(refetched).toContain("/api/docs/d1/shares");
    expect(
      await screen.findByText(
        "Only the people listed below can see this page and its subpages. Workspace roles no longer apply.",
      ),
    ).toBeTruthy();
    expect(screen.getByText("People with access")).toBeTruthy();
  });

  it("leaves the page unrestricted when the confirm is cancelled", async () => {
    vi.stubGlobal("confirm", vi.fn(() => false));
    const calls = renderDialog();

    const toggle = await screen.findByTestId("restricted-toggle");
    await waitFor(() => expect(toggle).toHaveProperty("disabled", false));
    fireEvent.click(toggle);

    expect(calls.some((call) => call.method === "PUT")).toBe(false);
    expect(toggle.getAttribute("aria-checked")).toBe("false");
  });

  it("inherited restriction shows disabled switch with root title", async () => {
    renderDialog({
      doc: { within_restricted: true, restriction_root_id: "root" },
      access: {
        restriction_root_id: "root",
        restriction_root_title: "Board minutes",
        people: [
          { principal: `user:${ADA}`, role: "owner", inherit: true, source_doc_id: "root", source_title: "Board minutes" },
        ],
      },
    });

    expect(await screen.findByText('Restricted by "Board minutes"')).toBeTruthy();
    const toggle = screen.getByTestId("restricted-toggle");
    expect(toggle).toHaveProperty("disabled", true);
    expect(toggle.getAttribute("aria-checked")).toBe("true");
    // The people list comes from /access, with where each grant comes from.
    expect(screen.getByText("People with access")).toBeTruthy();
    const row = await screen.findByTestId(`access-user:${ADA}`);
    expect(await within(row).findByText("Ada Lovelace")).toBeTruthy();
    expect(within(row).getByText(/Board minutes/)).toBeTruthy();
  });

  it("names no parent when the restriction root is hidden", async () => {
    renderDialog({
      doc: { within_restricted: true },
      access: { restriction_root_id: null, restriction_root_title: null },
    });

    expect(await screen.findByText("Restricted by a parent page")).toBeTruthy();
  });

  it("public link disabled inside restricted subtree", async () => {
    renderDialog({ doc: { within_restricted: true }, access: { restriction_root_title: "Board minutes" } });

    expect(await screen.findByText("Public links are disabled for restricted pages.")).toBeTruthy();
    expect(screen.queryByTestId("share-enable")).toBeNull();
  });

  it("explains the empty grant list outside a restricted subtree", async () => {
    vi.stubGlobal("fetch", (url: string) => {
      if (url.endsWith("/api/docs/d1")) return json(DOC);
      if (url.endsWith("/access")) return json(ACCESS);
      return json([]);
    });
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/doc/d1/permissions"]}>
          <Routes>
            <Route path="/doc/:id/permissions" element={<PermissionsDialog />} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    expect(
      await screen.findByText(
        "No explicit grants. Everyone in the workspace has access through their workspace role.",
      ),
    ).toBeTruthy();
    expect(screen.getByText("Explicit grants on this document.")).toBeTruthy();
  });

  it("last owner error shows toast", async () => {
    renderDialog({
      doc: { restricted: true, within_restricted: true, restriction_root_id: "d1" },
      access: {
        restricted: true,
        restriction_root_id: "d1",
        restriction_root_title: "Plan",
        people: [
          { principal: `user:${ADA}`, role: "owner", inherit: true, source_doc_id: "d1", source_title: "Plan" },
        ],
      },
      write: {
        status: 409,
        body: { error: { code: "grant.last_owner", message: "", details: {} } },
      },
    });

    const row = await screen.findByTestId(`access-user:${ADA}`);
    fireEvent.click(within(row).getByRole("button", { name: "Remove" }));

    expect(await screen.findByText("A restricted page needs at least one owner.")).toBeTruthy();
  });

  it("not member error shows toast", async () => {
    renderDialog({
      write: {
        status: 422,
        body: { error: { code: "grant.not_member", message: "", details: {} } },
      },
    });

    const select = await screen.findByTestId("grant-user");
    await within(select).findByText("Ada Lovelace (ada@example.com)");
    fireEvent.change(select, { target: { value: ADA } });
    fireEvent.click(screen.getByTestId("grant-add"));

    expect(await screen.findByText("Only workspace members can be added.")).toBeTruthy();
  });

  it("hints that inherit shares subpages on a restricted page", async () => {
    renderDialog({ doc: { restricted: true, within_restricted: true, restriction_root_id: "d1" } });

    expect(
      await screen.findByText('Untick "Inherit" to share only this page, not its subpages.'),
    ).toBeTruthy();
  });
});

describe("PermissionsDialog while the page loads", () => {
  it("shows no public link or grants until it knows whether the page is restricted", async () => {
    // GET /api/docs/d1 never answers: the page may be restricted.
    vi.stubGlobal("fetch", (url: string) =>
      url.endsWith("/api/docs/d1") ? new Promise<Response>(() => {}) : json(url.endsWith("/access") ? ACCESS : []),
    );
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/doc/d1/permissions"]}>
          <Routes>
            <Route path="/doc/:id/permissions" element={<PermissionsDialog />} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    expect(await screen.findByTestId("permissions-loading")).toHaveTextContent("Loading…");
    expect(screen.queryByTestId("share-enable")).toBeNull();
    expect(screen.queryByText("Explicit grants on this document.")).toBeNull();
    expect(screen.queryByTestId("grant-add")).toBeNull();
  });

  it("says so when the page can't be loaded", async () => {
    vi.stubGlobal("fetch", (url: string) =>
      url.endsWith("/api/docs/d1")
        ? json({ error: { code: "acl.no_grant", message: "", details: {} } }, 403)
        : json(url.endsWith("/access") ? ACCESS : []),
    );
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    render(
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/doc/d1/permissions"]}>
          <Routes>
            <Route path="/doc/:id/permissions" element={<PermissionsDialog />} />
          </Routes>
        </MemoryRouter>
      </QueryClientProvider>,
    );

    await waitFor(() => expect(screen.getByTestId("permissions-loading")).toHaveTextContent("Couldn't load"));
    expect(screen.queryByTestId("share-enable")).toBeNull();
  });
});

describe("PermissionsDialog people list", () => {
  it("lists one person's grants from two hidden parent pages as two rows", async () => {
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    renderDialog({
      doc: { within_restricted: true },
      access: {
        people: [
          { principal: `user:${ADA}`, role: "editor", inherit: true, source_doc_id: null, source_title: null },
          { principal: `user:${ADA}`, role: "owner", inherit: true, source_doc_id: null, source_title: null },
        ],
      },
    });

    await waitFor(() => expect(screen.getAllByTestId(`access-user:${ADA}`)).toHaveLength(2));
    const keyWarnings = consoleError.mock.calls.filter((call) => String(call[0]).includes("same key"));
    expect(keyWarnings).toEqual([]);
    consoleError.mockRestore();
  });
});
