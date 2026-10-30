import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, createEvent, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
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
type Page = { id: string; title: string; parent_id: string | null; sort_key: string };
type MoveBody = { parent_id: string | null; before_id?: string; after_id?: string };

/** Alpha; Beta > Beta one; Gamma. */
const PAGES: Page[] = [
  { id: "alpha", title: "Alpha", parent_id: null, sort_key: "a" },
  { id: "beta", title: "Beta", parent_id: null, sort_key: "b" },
  { id: "beta1", title: "Beta one", parent_id: "beta", sort_key: "m" },
  { id: "gamma", title: "Gamma", parent_id: null, sort_key: "c" },
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

function deferred<T>() {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

/** What the server does with a move: the page goes to its place among its
 *  new siblings, and they get fresh keys in that order. */
function serverMove(pages: Page[], id: string, b: MoveBody): Page[] {
  const page = pages.find((p) => p.id === id)!;
  const rest = pages.filter((p) => p.id !== id);
  const sibs = rest
    .filter((p) => p.parent_id === b.parent_id)
    .sort((x, y) => (x.sort_key < y.sort_key ? -1 : 1));
  const at =
    b.after_id !== undefined ? sibs.findIndex((s) => s.id === b.after_id) + 1
    : b.before_id !== undefined ? sibs.findIndex((s) => s.id === b.before_id)
    : sibs.length;
  sibs.splice(at, 0, { ...page, parent_id: b.parent_id });
  const keyed = sibs.map((s, i) => ({ ...s, sort_key: String.fromCharCode(97 + i) }));
  return [...rest.filter((p) => !keyed.some((k) => k.id === p.id)), ...keyed];
}

/** How the server answers one move. */
type MoveReply = {
  /** The move is answered once this resolves. */
  gate?: Promise<unknown>;
  /** The move is refused with this error. */
  refuse?: { status: number; code: string };
};

type ServerOpts = {
  role?: Role;
  /** Moves are answered once this resolves. */
  gate?: Promise<unknown>;
  /** Moves are refused with this error. */
  refuse?: { status: number; code: string };
  /** One reply per move, in the order the moves arrive, in place of `gate`
   *  and `refuse`. */
  replies?: MoveReply[];
  /** Where the server puts a moved page. By default where the move asked. */
  place?: (pages: Page[], id: string, b: MoveBody) => Page[];
  /** Every GET /api/docs after the first is answered once this resolves. */
  relistGate?: Promise<unknown>;
};

function server({ role = "editor", gate, refuse, replies, place = serverMove, relistGate }: ServerOpts = {}) {
  let pages = PAGES;
  let listed = 0;
  let movesSeen = 0;
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
    const moved = /^\/api\/docs\/([^/]+)\/move$/.exec(url);
    if (moved && init?.method === "POST") {
      const body = JSON.parse(init.body as string) as MoveBody;
      const reply = replies ? (replies[movesSeen] ?? {}) : { gate, refuse };
      movesSeen += 1;
      return (reply.gate ?? Promise.resolve()).then(() => {
        const refused = reply.refuse;
        if (refused) return response({ error: { code: refused.code, message: refused.code, details: {} } }, refused.status);
        pages = place(pages, moved[1]!, body);
        return response({});
      });
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

type FetchMock = ReturnType<typeof server>;

/** The moves the tree sent: page id and request body. */
function moves(fetchMock: FetchMock) {
  return fetchMock.mock.calls
    .filter(([url, init]) => url.endsWith("/move") && init?.method === "POST")
    .map(([url, init]) => ({ id: url.split("/")[3], body: JSON.parse(init?.body as string) as unknown }));
}

function renderTree(fetchMock: FetchMock) {
  vi.stubGlobal("fetch", fetchMock);
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <SessionProvider>
        <MemoryRouter>
          <DocTree />
          <Toast />
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
  return qc;
}

/** Rows for editors: the session has loaded once every row has its "More" button. */
async function editorRows() {
  await waitFor(() => expect(screen.getAllByRole("button", { name: "More" })).toHaveLength(PAGES.length));
}

/** Let a move run (it starts after an awaited cancelQueries) and its results render. */
async function settle() {
  await act(() => new Promise((r) => setTimeout(r, 30)));
}

/** The pages in sidebar order. */
function order(): string[] {
  return Array.from(document.querySelectorAll<HTMLElement>("[data-testid^='doc-row-']")).map((el) =>
    el.dataset.testid!.slice("doc-row-".length),
  );
}

/** The inline left padding of the nearest ancestor that has one: the row's depth. */
function indentOf(el: HTMLElement): number {
  for (let cur: HTMLElement | null = el; cur; cur = cur.parentElement) {
    const pl = cur.style.paddingLeft;
    if (pl && pl !== "0px") return parseFloat(pl);
  }
  return 0;
}

/** Open a row's menu with its "More" button. */
function openMenu(id: string): HTMLElement {
  fireEvent.click(within(screen.getByTestId(`tree-row-${id}`)).getByRole("button", { name: "More" }));
  return screen.getByTestId("context-menu");
}

/** Press a key on `el`; returns whether its default was cancelled. */
function press(el: HTMLElement, init: KeyboardEventInit): boolean {
  const ev = createEvent.keyDown(el, init);
  fireEvent(el, ev);
  return ev.defaultPrevented;
}

const ITEM = { up: "ctx-move-up", down: "ctx-move-down", indent: "ctx-indent", outdent: "ctx-outdent" } as const;
type Cmd = keyof typeof ITEM;

/** Where each command moves each page of PAGES, or null where it can't. */
const CASES: [string, Cmd, MoveBody | null][] = [
  ["alpha", "up", null],
  ["alpha", "down", { parent_id: null, after_id: "beta" }],
  ["alpha", "indent", null],
  ["alpha", "outdent", null],
  ["beta", "up", { parent_id: null, before_id: "alpha" }],
  ["beta", "down", { parent_id: null, after_id: "gamma" }],
  ["beta", "indent", { parent_id: "alpha" }],
  ["beta", "outdent", null],
  ["beta1", "up", null],
  ["beta1", "down", null],
  ["beta1", "indent", null],
  ["beta1", "outdent", { parent_id: null, after_id: "beta" }],
  // Between Alpha and Beta, anchored on the sibling before the gap.
  ["gamma", "up", { parent_id: null, after_id: "alpha" }],
  ["gamma", "down", null],
  ["gamma", "indent", { parent_id: "beta", after_id: "beta1" }],
  ["gamma", "outdent", null],
];

describe("DocTree — moves from the row menu", () => {
  it("lists Move up, Move down, Indent and Outdent after Rename", async () => {
    renderTree(server());
    await editorRows();
    const items = within(openMenu("beta")).getAllByRole("menuitem").map((b) => b.textContent);
    expect(items).toEqual(["Rename", "Duplicate…", "Move up", "Move down", "Indent", "Outdent", "Move to archive"]);
  });

  it("disables the moves a page can't make", async () => {
    renderTree(server());
    await editorRows();
    for (const [id, cmd, body] of CASES) {
      const item = within(openMenu(id)).getByTestId(ITEM[cmd]);
      if (body === null) expect(item, `${id} ${cmd}`).toBeDisabled();
      else expect(item, `${id} ${cmd}`).toBeEnabled();
      fireEvent.keyDown(item, { key: "Escape" });
      expect(screen.queryByTestId("context-menu")).toBeNull();
    }
  });

  it.each(CASES.filter(([, , body]) => body !== null))("%s: %s sends %j", async (id, cmd, body) => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();
    fireEvent.click(within(openMenu(id)).getByTestId(ITEM[cmd]));
    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id, body }]));
  });
});

describe("DocTree — Alt+arrow moves", () => {
  it.each([
    ["ArrowUp", "gamma", { parent_id: null, after_id: "alpha" }],
    ["ArrowDown", "alpha", { parent_id: null, after_id: "beta" }],
    ["ArrowRight", "beta", { parent_id: "alpha" }],
    ["ArrowLeft", "beta1", { parent_id: null, after_id: "beta" }],
  ])("Alt+%s moves %s and cancels the key's default", async (key, id, body) => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();
    expect(press(screen.getByTestId(`doc-row-${id}`), { key, altKey: true })).toBe(true);
    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id, body }]));
  });

  // Alt+← and Alt+→ are Back and Forward in Windows and Linux browsers: a
  // move that can't happen must not leave the page instead.
  it("cancels Alt+arrows that can't move the page, and sends nothing", async () => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();
    const alpha = screen.getByTestId("doc-row-alpha");
    expect(press(alpha, { key: "ArrowLeft", altKey: true })).toBe(true);
    expect(press(alpha, { key: "ArrowUp", altKey: true })).toBe(true);
    expect(press(screen.getByTestId("doc-row-beta1"), { key: "ArrowRight", altKey: true })).toBe(true);
    await settle();
    expect(moves(fetchMock)).toEqual([]);
  });

  it("leaves other keys to the browser", async () => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();
    const beta = screen.getByTestId("doc-row-beta");
    expect(press(beta, { key: "ArrowRight" })).toBe(false);
    expect(press(beta, { key: "ArrowRight", altKey: true, ctrlKey: true })).toBe(false);
    expect(press(beta, { key: "ArrowRight", altKey: true, metaKey: true })).toBe(false);
    expect(press(beta, { key: "Enter" })).toBe(false);
    expect(press(beta, { key: "F10" })).toBe(false);
    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.queryByTestId("context-menu")).toBeNull();
  });

  // A held key repeats its keydown about 30 times a second. Each repeat would
  // be another move, planned before the last one has even rendered.
  it("held_alt_arrow_repeat_sends_one_move: a held Alt+← moves the page once and never navigates", async () => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();
    const left = { key: "ArrowLeft", altKey: true };
    const betaOne = () => screen.getByTestId("doc-row-beta1");

    expect(press(betaOne(), { ...left, repeat: true })).toBe(true);
    await settle();
    expect(moves(fetchMock)).toEqual([]);

    expect(press(betaOne(), left)).toBe(true);
    for (let i = 0; i < 3; i++) expect(press(betaOne(), { ...left, repeat: true })).toBe(true);
    await settle();
    expect(moves(fetchMock)).toEqual([{ id: "beta1", body: { parent_id: null, after_id: "beta" } }]);
  });

  it("leaves Alt+arrows to the browser for viewers, who can't move pages", async () => {
    const fetchMock = server({ role: "viewer" });
    renderTree(fetchMock);
    await screen.findByText("a@x.test");
    expect(press(screen.getByTestId("doc-row-beta"), { key: "ArrowRight", altKey: true })).toBe(false);
    await settle();
    expect(moves(fetchMock)).toEqual([]);
  });
});

describe("DocTree — the row menu by keyboard", () => {
  it.each([
    ["Shift+F10", { key: "F10", shiftKey: true }],
    ["the ContextMenu key", { key: "ContextMenu" }],
  ])("opens the row menu at the link with %s", async (_name, key) => {
    renderTree(server());
    await editorRows();
    const link = screen.getByTestId("doc-row-beta");
    const box = { left: 24, top: 28, width: 200, height: 20 };
    vi.spyOn(link, "getBoundingClientRect").mockReturnValue({
      ...box,
      x: box.left,
      y: box.top,
      right: box.left + box.width,
      bottom: box.top + box.height,
      toJSON: () => box,
    });
    link.focus();

    expect(press(link, key)).toBe(true);
    const menu = screen.getByTestId("context-menu");
    // Just below the link, at its left edge.
    expect(menu).toHaveStyle({ left: "24px", top: "48px" });
    expect(within(menu).getByTestId("ctx-rename")).toHaveFocus();

    // Closing it puts the keyboard user back where they were.
    fireEvent.keyDown(document.activeElement!, { key: "Escape" });
    expect(screen.queryByTestId("context-menu")).toBeNull();
    expect(link).toHaveFocus();
  });

  // On Windows, Chrome and Edge send the Menu key's contextmenu on keyup, after
  // the keydown has opened the row menu and moved focus to its first item:
  // the event lands on that item, and the browser's own menu must not open
  // over ours.
  it("keeps the browser's own menu off the row menu", async () => {
    renderTree(server());
    await editorRows();
    const link = screen.getByTestId("doc-row-beta");
    link.focus();
    expect(press(link, { key: "ContextMenu" })).toBe(true);
    const rename = within(screen.getByTestId("context-menu")).getByTestId("ctx-rename");
    expect(rename).toHaveFocus();

    const onItem = createEvent.contextMenu(rename);
    fireEvent(rename, onItem);
    expect(onItem.defaultPrevented).toBe(true);
    expect(screen.getByTestId("context-menu")).toBeInTheDocument();
    expect(rename).toHaveFocus();

    // Elsewhere in the row, where no menu of ours opens, the browser's stays.
    fireEvent.keyDown(rename, { key: "Escape" });
    const row = screen.getByTestId("tree-row-beta");
    const onRow = createEvent.contextMenu(row);
    fireEvent(row, onRow);
    expect(onRow.defaultPrevented).toBe(false);
    expect(screen.queryByTestId("context-menu")).toBeNull();
  });
});

describe("DocTree — the browser's own menu", () => {
  // Viewers have no row menu: a right-click on a page must still open the
  // browser's (open in a new tab, copy the link).
  it("leaves the browser's menu on a page's link to a user with no row menu items", async () => {
    renderTree(server({ role: "viewer" }));
    await screen.findByText("a@x.test");
    const link = screen.getByTestId("doc-row-beta");

    const ev = createEvent.contextMenu(link);
    fireEvent(link, ev);
    expect(ev.defaultPrevented).toBe(false);
    expect(screen.queryByTestId("context-menu")).toBeNull();
  });

  it("leaves Shift+F10 and the ContextMenu key to the browser for a user with no row menu items", async () => {
    renderTree(server({ role: "viewer" }));
    await screen.findByText("a@x.test");
    const link = screen.getByTestId("doc-row-beta");

    expect(press(link, { key: "F10", shiftKey: true })).toBe(false);
    expect(press(link, { key: "ContextMenu" })).toBe(false);
    expect(screen.queryByTestId("context-menu")).toBeNull();
  });
});

describe("DocTree — moves in quick succession", () => {
  it("moves_are_sent_one_at_a_time: a move is sent once the one before it is answered, in press order", async () => {
    const first = deferred<void>();
    const second = deferred<void>();
    const fetchMock = server({ replies: [{ gate: first.promise }, { gate: second.promise }] });
    const qc = renderTree(fetchMock);
    await editorRows();

    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    await waitFor(() => expect(order()).toEqual(["beta", "beta1", "alpha", "gamma"]));
    // Planned from the list the first move shows.
    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    await waitFor(() => expect(order()).toEqual(["beta", "beta1", "gamma", "alpha"]));
    await settle();
    expect(moves(fetchMock)).toEqual([{ id: "alpha", body: { parent_id: null, after_id: "beta" } }]);

    first.resolve();
    await waitFor(() => expect(moves(fetchMock)).toHaveLength(2));
    expect(moves(fetchMock)).toEqual([
      { id: "alpha", body: { parent_id: null, after_id: "beta" } },
      { id: "alpha", body: { parent_id: null, after_id: "gamma" } },
    ]);

    second.resolve();
    await waitFor(() => {
      expect(qc.isMutating()).toBe(0);
      expect(qc.isFetching()).toBe(0);
    });
    await settle();
    // The server applied them in that order: the other order ends with Gamma last.
    expect(order()).toEqual(["beta", "beta1", "gamma", "alpha"]);
  });

  it("failed_move_does_not_undo_a_later_one: a refused move leaves the next one shown, then the server's list", async () => {
    const first = deferred<void>();
    const second = deferred<void>();
    const fetchMock = server({
      replies: [
        { gate: first.promise, refuse: { status: 422, code: "doc.bad_anchor" } },
        { gate: second.promise },
      ],
    });
    const qc = renderTree(fetchMock);
    await editorRows();

    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    await waitFor(() => expect(order()).toEqual(["beta", "beta1", "alpha", "gamma"]));
    fireEvent.click(within(openMenu("beta1")).getByTestId("ctx-outdent"));
    await waitFor(() => expect(indentOf(screen.getByTestId("doc-row-beta1"))).toBe(4));

    first.resolve();
    await waitFor(() => expect(screen.getByTestId("toast-error")).toHaveTextContent("Couldn't move"));
    await settle();
    // The second move is still on its way, and still shown at the top level.
    expect(indentOf(screen.getByTestId("doc-row-beta1"))).toBe(4);

    second.resolve();
    await waitFor(() => {
      expect(qc.isMutating()).toBe(0);
      expect(qc.isFetching()).toBe(0);
    });
    await settle();
    // The server's list: Alpha never moved, Beta one is a top-level page after Beta.
    expect(order()).toEqual(["alpha", "beta", "beta1", "gamma"]);
    expect(indentOf(screen.getByTestId("doc-row-beta1"))).toBe(4);
    expect(screen.getAllByTestId("toast-error")).toHaveLength(1);
  });
});

describe("DocTree — announcements and focus", () => {
  it("announces each move by title in a polite live region", async () => {
    renderTree(server());
    await editorRows();
    const live = screen.getByTestId("tree-live");
    expect(live).toHaveAttribute("aria-live", "polite");
    expect(live).toBeEmptyDOMElement();

    fireEvent.keyDown(screen.getByTestId("doc-row-beta"), { key: "ArrowRight", altKey: true });
    expect(live).toHaveTextContent(/^Moved “Beta” into “Alpha”, position 1 of 1$/);

    // Now Alpha > Beta > Beta one; Gamma.
    await waitFor(() => expect(indentOf(screen.getByTestId("doc-row-beta"))).toBe(16));
    fireEvent.click(within(openMenu("gamma")).getByTestId("ctx-move-up"));
    expect(screen.getByTestId("tree-live")).toBe(live);
    expect(live).toHaveTextContent(/^Moved “Gamma” to the top level, position 1 of 2$/);
  });

  it("keeps focus on a page moved with Alt+arrow, through the refetch", async () => {
    const fetchMock = server();
    const qc = renderTree(fetchMock);
    await editorRows();
    const before = screen.getByTestId("doc-row-beta");
    before.focus();

    fireEvent.keyDown(before, { key: "ArrowRight", altKey: true });
    await waitFor(() => {
      expect(moves(fetchMock)).toHaveLength(1);
      expect(qc.isMutating()).toBe(0);
      expect(qc.isFetching()).toBe(0);
    });
    await settle();

    const after = screen.getByTestId("doc-row-beta");
    expect(indentOf(after)).toBe(16);
    // Under its new parent the row is a new element, found again by id.
    expect(after).not.toBe(before);
    expect(after).toHaveFocus();
  });

  it("puts focus on a page moved from its menu", async () => {
    const fetchMock = server();
    const qc = renderTree(fetchMock);
    await editorRows();

    fireEvent.click(within(openMenu("gamma")).getByTestId("ctx-indent"));
    await waitFor(() => {
      expect(moves(fetchMock)).toHaveLength(1);
      expect(qc.isMutating()).toBe(0);
      expect(qc.isFetching()).toBe(0);
    });
    await settle();

    const gamma = screen.getByTestId("doc-row-gamma");
    expect(indentOf(gamma)).toBe(16);
    expect(gamma).toHaveFocus();
  });

  it("expands a collapsed page that a page is indented into, so it keeps focus", async () => {
    const fetchMock = server();
    renderTree(fetchMock);
    await editorRows();
    fireEvent.click(within(screen.getByTestId("tree-row-beta")).getByRole("button", { name: "Collapse" }));
    expect(screen.queryByTestId("doc-row-beta1")).toBeNull();
    const gamma = screen.getByTestId("doc-row-gamma");
    gamma.focus();

    fireEvent.keyDown(gamma, { key: "ArrowRight", altKey: true });
    // Beta's children were hidden, so the position can't be told.
    expect(screen.getByTestId("tree-live")).toHaveTextContent(/^Moved “Gamma” into “Beta”$/);
    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id: "gamma", body: { parent_id: "beta" } }]));
    await waitFor(() => expect(screen.getByTestId("doc-row-gamma")).toHaveFocus());
    expect(order()).toEqual(["alpha", "beta", "beta1", "gamma"]);
    expect(indentOf(screen.getByTestId("doc-row-gamma"))).toBe(16);
  });

  it("keeps focus on the page when the refetch shows it under another parent", async () => {
    const relist = deferred<void>();
    // Someone else moved Gamma into Alpha meanwhile.
    const fetchMock = server({
      place: (pages, id) => serverMove(pages, id, { parent_id: "alpha" }),
      relistGate: relist.promise,
    });
    const qc = renderTree(fetchMock);
    await editorRows();
    const gamma = screen.getByTestId("doc-row-gamma");
    gamma.focus();

    fireEvent.keyDown(gamma, { key: "ArrowRight", altKey: true });
    // The move is done and shown; the refetch after it takes a while.
    await waitFor(() => expect(qc.isMutating()).toBe(0));
    await settle();
    expect(order()).toEqual(["alpha", "beta", "beta1", "gamma"]);
    expect(screen.getByTestId("doc-row-gamma")).toHaveFocus();

    relist.resolve();
    await waitFor(() => expect(qc.isFetching()).toBe(0));
    await settle();
    expect(order()).toEqual(["alpha", "gamma", "beta", "beta1"]);
    expect(screen.getByTestId("doc-row-gamma")).toHaveFocus();
  });

  it("leaves focus where the user put it while the move was on its way", async () => {
    const gate = deferred<void>();
    const fetchMock = server({ gate: gate.promise });
    const qc = renderTree(fetchMock);
    await editorRows();
    const beta = screen.getByTestId("doc-row-beta");
    beta.focus();

    fireEvent.keyDown(beta, { key: "ArrowRight", altKey: true });
    await waitFor(() => expect(screen.getByTestId("doc-row-beta")).toHaveFocus());
    expect(screen.getByTestId("doc-row-beta")).not.toBe(beta);
    const gamma = screen.getByTestId("doc-row-gamma");
    gamma.focus();

    gate.resolve();
    await waitFor(() => {
      expect(qc.isMutating()).toBe(0);
      expect(qc.isFetching()).toBe(0);
    });
    await settle();
    expect(gamma).toHaveFocus();
  });
});

describe("DocTree — refused moves (P12)", () => {
  it.each([
    [422, "doc.bad_anchor"],
    [409, "doc.move_cycle"],
  ])("move_error_rolls_back_and_toasts: a %i %s puts the page back and says so", async (status, code) => {
    const gate = deferred<void>();
    // The refetch after the failure never answers: only a rollback can put the page back.
    const fetchMock = server({ gate: gate.promise, refuse: { status, code }, relistGate: new Promise(() => {}) });
    renderTree(fetchMock);
    await editorRows();
    expect(order()).toEqual(["alpha", "beta", "beta1", "gamma"]);

    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    await waitFor(() => expect(order()).toEqual(["beta", "beta1", "alpha", "gamma"]));

    gate.resolve();
    await waitFor(() => {
      expect(screen.getByTestId("toast-error")).toHaveTextContent("Couldn't move");
      expect(order()).toEqual(["alpha", "beta", "beta1", "gamma"]);
    });
  });

  it("says that only an owner can move a page out of a restricted area", async () => {
    renderTree(server({ refuse: { status: 403, code: "acl.restricted_move" } }));
    await editorRows();

    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    await waitFor(() =>
      expect(screen.getByTestId("toast-error")).toHaveTextContent(
        "Only an owner of this page can move it out of a restricted area.",
      ),
    );
  });

  it.each([
    [422, "doc.bad_anchor"],
    [409, "doc.move_cycle"],
  ])("refused_move_is_announced: after a %i %s the live region says the page didn't move", async (status, code) => {
    const gate = deferred<void>();
    renderTree(server({ gate: gate.promise, refuse: { status, code } }));
    await editorRows();
    const live = screen.getByTestId("tree-live");

    fireEvent.click(within(openMenu("alpha")).getByTestId("ctx-move-down"));
    expect(live).toHaveTextContent(/^Moved “Alpha” to the top level, position 2 of 3$/);

    gate.resolve();
    await waitFor(() => expect(live).toHaveTextContent(/^Couldn't move “Alpha”\.$/));
    await waitFor(() => expect(order()).toEqual(["alpha", "beta", "beta1", "gamma"]));
  });
});
