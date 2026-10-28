import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useEffect } from "react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";
import { Toast } from "../../components/Toast";
import { useUi } from "../../stores/ui";

import { DocTree } from "./DocTree";
import { editModeKey } from "./editMode";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  useUi.setState({ toasts: [] });
  window.sessionStorage.clear();
});

const TOO_LARGE = "This page is too large to duplicate (over 200 pages or 200 MB of files).";
const FAILED = "Couldn't duplicate this page.";

type Role = "owner" | "editor" | "viewer";
type Page = { id: string; title: string; parent_id: string | null; sort_key: string };

/** Q4 planning > (Milestones; Budget > Retro notes); Meeting notes. Q4
 *  planning has two children and three subpages. */
const PAGES: Page[] = [
  { id: "plan", title: "Q4 planning", parent_id: null, sort_key: "a" },
  { id: "miles", title: "Milestones", parent_id: "plan", sort_key: "a" },
  { id: "budget", title: "Budget", parent_id: "plan", sort_key: "b" },
  { id: "retro", title: "Retro notes", parent_id: "budget", sort_key: "a" },
  { id: "notes", title: "Meeting notes", parent_id: null, sort_key: "b" },
];

function doc(p: Page) {
  return {
    ...p,
    workspace_id: "w1",
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
  };
}

function response(body: unknown, status = 200) {
  return new Response(JSON.stringify(body), { status, headers: { "Content-Type": "application/json" } });
}

function json(body: unknown, status = 200) {
  return Promise.resolve(response(body, status));
}

function deferred() {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

/** How the server answers a duplicate: an error, a failed request, or (by
 *  default) a copy with the id "copy" right after the source. */
type Reply = { status: number; code: string } | "network";

type ServerOpts = {
  role?: Role;
  reply?: Reply;
  /** Duplicates are answered once this resolves. */
  gate?: Promise<void>;
  /** Every GET /api/docs after the first is answered once this resolves. */
  relistGate?: Promise<void>;
};

function server({ role = "editor", reply, gate, relistGate }: ServerOpts = {}) {
  let pages = PAGES;
  let listed = 0;
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role, archive_retention_days: 14 });
    }
    if (url === "/api/docs" && (!init?.method || init.method === "GET")) {
      listed += 1;
      if (listed === 1 || !relistGate) return json(pages.map(doc));
      return relistGate.then(() => response(pages.map(doc)));
    }
    const dup = /^\/api\/docs\/([^/]+)\/duplicate$/.exec(url);
    if (dup && init?.method === "POST") {
      return (gate ?? Promise.resolve()).then(() => {
        if (reply === "network") throw new TypeError("Failed to fetch");
        if (reply) return response({ error: { code: reply.code, message: reply.code, details: {} } }, reply.status);
        const src = pages.find((p) => p.id === dup[1])!;
        const copy = { id: "copy", title: `${src.title} (copy)`, parent_id: src.parent_id, sort_key: `${src.sort_key}m` };
        pages = [...pages, copy];
        return response(doc(copy), 201);
      });
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

type FetchMock = ReturnType<typeof server>;

/** The duplicates the tree asked for: page id and request body. */
function duplicates(fetchMock: FetchMock) {
  return fetchMock.mock.calls
    .filter(([url, init]) => url.endsWith("/duplicate") && init?.method === "POST")
    .map(([url, init]) => ({ id: url.split("/")[3], body: JSON.parse(init?.body as string) as unknown }));
}

/** How many times the page list was fetched. */
function lists(fetchMock: FetchMock): number {
  return fetchMock.mock.calls.filter(([url, init]) => url === "/api/docs" && (!init?.method || init.method === "GET"))
    .length;
}

/** Reports each path the app arrives at. */
function Location({ onPath }: { onPath: (path: string) => void }) {
  const { pathname } = useLocation();
  useEffect(() => onPath(pathname), [pathname, onPath]);
  return <output data-testid="location">{pathname}</output>;
}

/** Each path the app arrived at, and whether the copy was set to open in
 *  edit mode by then. */
type Arrival = { path: string; editMode: string | null };

function renderTree(fetchMock: FetchMock): Arrival[] {
  vi.stubGlobal("fetch", fetchMock);
  const arrivals: Arrival[] = [];
  const onPath = (path: string) => {
    arrivals.push({ path, editMode: window.sessionStorage.getItem(editModeKey("copy")) });
  };
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter>
          <DocTree />
          <Toast />
          <Location onPath={onPath} />
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
  return arrivals;
}

/** Rows for editors: the session has loaded once every row has its "More" button. */
async function editorRows() {
  await waitFor(() => expect(screen.getAllByRole("button", { name: "More" })).toHaveLength(PAGES.length));
}

/** Let requests run and their results render. */
async function settle() {
  await act(() => new Promise((r) => setTimeout(r, 30)));
}

/** Open a row's menu with its "More" button. */
function openMenu(id: string): HTMLElement {
  fireEvent.click(within(screen.getByTestId(`tree-row-${id}`)).getByRole("button", { name: "More" }));
  return screen.getByTestId("context-menu");
}

/** Choose "Duplicate…" in a row's menu. */
function chooseDuplicate(id: string) {
  fireEvent.click(within(openMenu(id)).getByTestId("ctx-duplicate"));
}

describe("Duplicate… in the row menu", () => {
  it.each<Role>(["owner", "editor"])("is offered to an %s", async (role) => {
    renderTree(server({ role }));
    await editorRows();
    expect(within(openMenu("notes")).getByTestId("ctx-duplicate")).toHaveTextContent("Duplicate…");
  });

  it("is not offered to a viewer", async () => {
    renderTree(server({ role: "viewer" }));
    await screen.findByText("a@x.test");
    fireEvent.contextMenu(screen.getByTestId("doc-row-notes"));
    expect(screen.queryByTestId("ctx-duplicate")).toBeNull();
  });
});

describe("Duplicate without subpages", () => {
  it("copies at once: no dialog, and one request without subpages", async () => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("notes");
    expect(screen.queryByRole("dialog")).toBeNull();

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/doc/copy"));
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(duplicates(fetchMock)).toEqual([{ id: "notes", body: { include_subpages: false } }]);
  });

  it("says Duplicating… on the row, and Duplicate… is off until the copy is made", async () => {
    const gate = deferred();
    const fetchMock = server({ gate: gate.promise });
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("notes");
    const row = screen.getByTestId("tree-row-notes");
    expect(await within(row).findByText("Duplicating…")).toBeInTheDocument();
    expect(screen.getByTestId("tree-live")).toHaveTextContent("Duplicating “Meeting notes”…");
    // Picking it again makes no second copy.
    const again = within(openMenu("notes")).getByTestId("ctx-duplicate");
    expect(again).toBeDisabled();
    fireEvent.click(again);
    // Another page's copy can still be asked for.
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(within(openMenu("plan")).getByTestId("ctx-duplicate")).toBeEnabled();
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    gate.resolve();

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/doc/copy"));
    expect(within(row).queryByText("Duplicating…")).toBeNull();
    expect(duplicates(fetchMock)).toEqual([{ id: "notes", body: { include_subpages: false } }]);
  });
});

describe("the Duplicate dialog", () => {
  it("asks first when the page has subpages, naming how many, with the box unticked", async () => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("plan");
    const dialog = screen.getByRole("dialog", { name: "Duplicate “Q4 planning”" });
    expect(dialog).toHaveAttribute("aria-modal", "true");
    // Three subpages: its two children and a grandchild.
    const box = within(dialog).getByRole("checkbox", { name: "Include its 3 subpages" });
    expect(box).not.toBeChecked();
    expect(box).toHaveAccessibleDescription("Every subpage is copied with all its content, drawings and files.");
    expect(within(dialog).getByRole("button", { name: "Cancel" })).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Duplicate" })).toBeInTheDocument();

    await settle();
    expect(duplicates(fetchMock)).toEqual([]);
  });

  it("says subpage for a page with one", async () => {
    renderTree(server());
    await editorRows();

    chooseDuplicate("budget");
    expect(within(screen.getByRole("dialog")).getByRole("checkbox", { name: "Include its 1 subpage" })).not.toBeChecked();
  });

  it.each([
    [false, "leaves the box unticked"],
    [true, "ticks the box"],
  ])("sends include_subpages: %s when the user %s", async (tick) => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("plan");
    const dialog = screen.getByRole("dialog");
    if (tick) fireEvent.click(within(dialog).getByRole("checkbox"));
    fireEvent.click(within(dialog).getByRole("button", { name: "Duplicate" }));

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/doc/copy"));
    expect(duplicates(fetchMock)).toEqual([{ id: "plan", body: { include_subpages: tick } }]);
  });

  it.each<[string, (dialog: HTMLElement) => void]>([
    [
      "Escape",
      () => {
        fireEvent.keyDown(document.activeElement!, { key: "Escape" });
      },
    ],
    [
      "Cancel",
      (dialog) => {
        fireEvent.click(within(dialog).getByRole("button", { name: "Cancel" }));
      },
    ],
    [
      "a click beside it",
      (dialog) => {
        fireEvent.click(dialog.parentElement!);
      },
    ],
  ])("closes on %s and gives focus back to the row link", async (_how, close) => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("plan");
    const dialog = screen.getByRole("dialog");
    expect(dialog).toContainElement(document.activeElement as HTMLElement);
    close(dialog);

    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByTestId("doc-row-plan")).toHaveFocus();
    await settle();
    expect(duplicates(fetchMock)).toEqual([]);
  });

  it("keeps focus inside: Tab and Shift+Tab wrap around its controls", async () => {
    renderTree(server());
    await editorRows();

    chooseDuplicate("plan");
    const dialog = screen.getByRole("dialog");
    const box = within(dialog).getByRole("checkbox");
    const cancel = within(dialog).getByRole("button", { name: "Cancel" });
    const ok = within(dialog).getByRole("button", { name: "Duplicate" });
    expect(box).toHaveFocus();

    const tab = (shiftKey = false) => fireEvent.keyDown(document.activeElement!, { key: "Tab", shiftKey });
    tab(true);
    expect(ok).toHaveFocus();
    tab();
    expect(box).toHaveFocus();
    tab();
    expect(cancel).toHaveFocus();
    tab();
    expect(ok).toHaveFocus();
    tab();
    expect(box).toHaveFocus();
  });

  // A click on its text puts focus on the dialog itself, not on <body>, where
  // its keys would no longer reach it.
  it("keeps Tab and Escape when focus is on the dialog itself", async () => {
    renderTree(server());
    await editorRows();

    chooseDuplicate("plan");
    const dialog = screen.getByRole("dialog");
    dialog.focus();
    expect(dialog).toHaveFocus();
    fireEvent.keyDown(dialog, { key: "Tab" });
    expect(within(dialog).getByRole("checkbox")).toHaveFocus();
    dialog.focus();
    fireEvent.keyDown(dialog, { key: "Tab", shiftKey: true });
    expect(within(dialog).getByRole("button", { name: "Duplicate" })).toHaveFocus();

    dialog.focus();
    fireEvent.keyDown(dialog, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByTestId("doc-row-plan")).toHaveFocus();
  });

  it("sends one request however often Duplicate is pressed while the copy is made", async () => {
    const gate = deferred();
    const fetchMock = server({ gate: gate.promise });
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("plan");
    const ok = within(screen.getByRole("dialog")).getByRole("button", { name: "Duplicate" });
    // A double click: the second press comes before the pending state renders.
    fireEvent.click(ok);
    fireEvent.click(ok);
    await settle();
    // Once it has rendered, the button says so and keeps focus.
    expect(ok).toHaveAttribute("aria-disabled", "true");
    expect(ok).toBeEnabled();
    fireEvent.click(ok);
    gate.resolve();

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/doc/copy"));
    expect(duplicates(fetchMock)).toHaveLength(1);
  });
});

describe("after a successful Duplicate", () => {
  it.each([
    ["notes", "without a dialog"],
    ["plan", "from the dialog"],
  ])("refreshes the list, then opens the copy of %s in edit mode (%s)", async (id) => {
    const relist = deferred();
    const fetchMock = server({ relistGate: relist.promise });
    const arrivals = renderTree(fetchMock);
    await editorRows();

    chooseDuplicate(id);
    const dialog = screen.queryByRole("dialog");
    if (dialog) fireEvent.click(within(dialog).getByRole("button", { name: "Duplicate" }));

    // The copy opens only once the list that shows it is in.
    await waitFor(() => expect(lists(fetchMock)).toBe(2));
    await settle();
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
    relist.resolve();

    await waitFor(() => expect(screen.getByTestId("location")).toHaveTextContent("/doc/copy"));
    // Set to open in edit mode by the time the app got there.
    expect(arrivals.at(-1)).toEqual({ path: "/doc/copy", editMode: "1" });
    expect(screen.getByTestId("doc-row-copy")).toBeInTheDocument();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.queryByTestId("toast-error")).toBeNull();
  });
});

describe("when Duplicate fails", () => {
  const ERRORS: [string, Reply, string][] = [
    ["a 413", { status: 413, code: "doc.duplicate_too_large" }, TOO_LARGE],
    ["a 409", { status: 409, code: "doc.duplicate_conflict" }, FAILED],
    ["a 422", { status: 422, code: "doc.is_template" }, FAILED],
    ["a 500", { status: 500, code: "internal" }, FAILED],
    ["a failed request", "network", FAILED],
  ];

  it.each(ERRORS)("%s shows its message in the open dialog", async (_what, reply, text) => {
    const fetchMock = server({ reply });
    renderTree(fetchMock);
    await editorRows();

    chooseDuplicate("plan");
    const dialog = screen.getByRole("dialog");
    fireEvent.click(within(dialog).getByRole("button", { name: "Duplicate" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(text);
    expect(screen.getByRole("dialog")).toBe(dialog);
    expect(screen.queryByTestId("toast-error")).toBeNull();
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  it.each(ERRORS)("%s without a dialog shows its message as a toast", async (_what, reply, text) => {
    renderTree(server({ reply }));
    await editorRows();

    chooseDuplicate("notes");

    expect(await screen.findByTestId("toast-error")).toHaveTextContent(text);
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  it("shows the message as a toast when the dialog was closed before the answer came", async () => {
    const gate = deferred();
    renderTree(server({ reply: { status: 413, code: "doc.duplicate_too_large" }, gate: gate.promise }));
    await editorRows();

    chooseDuplicate("plan");
    fireEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Duplicate" }));
    await settle();
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(screen.queryByRole("dialog")).toBeNull();
    gate.resolve();

    expect(await screen.findByTestId("toast-error")).toHaveTextContent(TOO_LARGE);
  });
});
