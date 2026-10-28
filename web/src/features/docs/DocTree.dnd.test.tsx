import type { Active, Over } from "@dnd-kit/core";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { MemoryRouter, useLocation, useNavigationType } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

import { DocTree, makeAnnouncements } from "./DocTree";
import { INDENT, type FlatRow, type Projection } from "./tree";
import type { Drop } from "./TreeRow";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

const INSTRUCTIONS = "Drag to move. Use the row menu or Alt with the arrow keys to move by keyboard.";

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(status === 204 ? null : JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

function doc(id: string, title: string, parent_id: string | null) {
  return {
    id,
    workspace_id: "w1",
    parent_id,
    title,
    sort_key: "m",
    icon: null,
    created_by: "u1",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
  };
}

/** Folder > Child > Grandchild. */
const NESTED = [doc("folder", "Folder", null), doc("child", "Child", "folder"), doc("grandchild", "Grandchild", "child")];

/** Alpha > Alpha one; Beta; Gamma. Beta can nest under Alpha, and Alpha one,
 *  a last child, can un-nest. */
const SIBLINGS = [
  doc("alpha", "Alpha", null),
  doc("alpha1", "Alpha one", "alpha"),
  doc("beta", "Beta", null),
  doc("gamma", "Gamma", null),
];

/** A workspace with `pages` (equal sort keys: the list order is the sidebar's). */
function server(role: "owner" | "editor" | "viewer", pages = NESTED) {
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/auth/session") {
      return json({ user_id: "u1", email: "a@x.test", display_name: "A", workspace_id: "w1", role });
    }
    if (url === "/api/workspace") {
      return json({ id: "w1", slug: "default", name: "W", role, archive_retention_days: 14 });
    }
    if (url === "/api/docs" && (!init?.method || init.method === "GET")) {
      return json(pages);
    }
    if (url.endsWith("/move") && init?.method === "POST") return json({});
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

/** The moves the tree sent: page id and request body. */
function moves(fetchMock: ReturnType<typeof server>) {
  return fetchMock.mock.calls
    .filter(([url, init]) => url.endsWith("/move") && init?.method === "POST")
    .map(([url, init]) => ({ id: url.split("/")[3], body: JSON.parse(init?.body as string) as unknown }));
}

const ROW_H = 28;
const TREE_W = 260;

/** jsdom has no layout. Give the tree's list and its rows the boxes a real
 *  sidebar would: rows 28px high, top to bottom in DOM order, moved up by
 *  as far as the sidebar has scrolled. */
function layOut() {
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function (this: Element) {
    const rows = Array.from(document.querySelectorAll("[data-testid^='tree-row-']"));
    const i = rows.indexOf(this);
    const scrolled = document.querySelector("[data-testid='sidebar']")?.scrollTop ?? 0;
    const box =
      i >= 0 ? { left: 0, top: i * ROW_H - scrolled, width: TREE_W, height: ROW_H }
      : this.matches("[data-testid='doc-tree'] > ul") ? { left: 0, top: -scrolled, width: TREE_W, height: 400 }
      : { left: 0, top: 0, width: 0, height: 0 };
    const { left, top, width, height } = box;
    return { ...box, x: left, y: top, right: left + width, bottom: top + height, toJSON: () => box };
  });
}

/** Let the move mutation run (it starts after an awaited cancelQueries). */
async function settle() {
  await act(() => new Promise((r) => setTimeout(r, 30)));
}

/** Where the router is: a click that reaches a row link navigates there.
 *  `data-navigation` says how it got there: PUSH, REPLACE or POP. */
function Location() {
  return (
    <output data-testid="location" data-navigation={useNavigationType()}>
      {useLocation().pathname}
    </output>
  );
}

function renderTree(role: "owner" | "editor" | "viewer" = "editor", pages = NESTED, at = "/") {
  const fetchMock = server(role, pages);
  vi.stubGlobal("fetch", fetchMock);
  render(
    <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
      <SessionProvider>
        <MemoryRouter initialEntries={[at]}>
          {/* It scrolls, as the app's sidebar does (AppShell). */}
          <div data-testid="sidebar" style={{ overflowY: "auto" }}>
            <DocTree />
          </div>
          <Location />
        </MemoryRouter>
      </SessionProvider>
    </QueryClientProvider>,
  );
  return fetchMock;
}

/** Rows for editors: the session has loaded once every row has its "More" button. */
async function editorRows(count = NESTED.length) {
  await waitFor(() => expect(screen.getAllByRole("button", { name: "More" })).toHaveLength(count));
}

/** The inline left padding of the nearest ancestor that has one: e2e's depth probe. */
function indentOf(el: HTMLElement): number {
  for (let cur: HTMLElement | null = el; cur; cur = cur.parentElement) {
    const pl = cur.style.paddingLeft;
    if (pl && pl !== "0px") return parseFloat(pl);
  }
  return 0;
}

/** The text of every element `el`'s aria-describedby names. */
function descriptions(el: HTMLElement): string[] {
  return (el.getAttribute("aria-describedby") ?? "")
    .split(/\s+/)
    .filter(Boolean)
    .map((id) => document.getElementById(id)?.textContent ?? `(no element #${id})`);
}

/** Press on a row link and move far enough for the mouse sensor to start a drag. */
function startDrag(link: HTMLElement) {
  fireEvent.mouseDown(link, { button: 0, clientX: 400, clientY: 300 });
  fireEvent.mouseMove(document, { clientX: 400, clientY: 320 });
}

function endDrag() {
  fireEvent.mouseUp(document, { clientX: 400, clientY: 320 });
}

/** A drop that makes the page a top-level page right after `id`. */
function topLevelAfter(id: string): Projection {
  return { depth: 0, minDepth: 0, maxDepth: 1, args: { parent_id: null, after_id: id } };
}

/** What dnd-kit's live region last announced. */
function announced(): string {
  return document.querySelector("[id^='DndLiveRegion']")?.textContent ?? "(no live region)";
}

describe("DocTree — rows", () => {
  it("gives rows no button role and no extra tab stop; the link takes focus", async () => {
    renderTree();
    await editorRows();
    for (const id of ["folder", "child", "grandchild"]) {
      const link = screen.getByTestId(`doc-row-${id}`);
      const li = link.closest("li")!;
      const row = link.closest(`[data-testid="tree-row-${id}"]`);
      for (const el of [li, row]) {
        expect(el).not.toBeNull();
        expect(el).not.toHaveAttribute("role");
        expect(el).not.toHaveAttribute("tabindex");
        expect(el).not.toHaveAttribute("aria-roledescription");
      }
      link.focus();
      expect(link).toHaveFocus();
    }
  });

  // P13: the e2e specs find a row's depth through the inline paddingLeft of
  // an ancestor of its link, open its menu by right-clicking the link, and
  // count `doc-row-` testids.
  it("keeps the link's indent, its context menu and one doc-row testid per page", async () => {
    renderTree();
    await editorRows();
    expect(indentOf(screen.getByTestId("doc-row-folder"))).toBe(4);
    expect(indentOf(screen.getByTestId("doc-row-child"))).toBe(16);
    expect(indentOf(screen.getByTestId("doc-row-grandchild"))).toBe(28);
    expect(document.querySelectorAll("[data-testid^='doc-row-']")).toHaveLength(3);

    const notCancelled = fireEvent.contextMenu(screen.getByTestId("doc-row-child"));
    expect(notCancelled).toBe(false);
    expect(screen.getByTestId("context-menu")).toBeInTheDocument();
  });

  it("describes each row's level and subpages to screen readers", async () => {
    renderTree();
    await editorRows();
    const folder = descriptions(screen.getByTestId("doc-row-folder"));
    expect(folder).toContain("level 1, 2 subpages");
    expect(folder).toContain(INSTRUCTIONS);
    expect(descriptions(screen.getByTestId("doc-row-child"))).toContain("level 2, 1 subpage");
    expect(descriptions(screen.getByTestId("doc-row-grandchild"))).toContain("level 3");
  });

  it("doesn't tell viewers, who can't move pages, how to drag them", async () => {
    renderTree("viewer");
    await screen.findByText("a@x.test");
    expect(descriptions(screen.getByTestId("doc-row-folder"))).toEqual(["level 1, 2 subpages"]);
  });
});

describe("DocTree — dragging", () => {
  it("announces drags by title, never by id", () => {
    const rows: FlatRow[] = [
      { id: "0b6c3d0e-1f2a-4b5c-8d9e-0a1b2c3d4e5f", parentId: null, depth: 0, title: "Roadmap", descendantCount: 1 },
      {
        id: "5f1e2a9c-3b4d-4e6f-9a0b-1c2d3e4f5a6b",
        parentId: "0b6c3d0e-1f2a-4b5c-8d9e-0a1b2c3d4e5f",
        depth: 1,
        title: "Q3 goals",
        descendantCount: 0,
      },
    ];
    const docs = [doc(rows[0]!.id, "Roadmap", null), doc(rows[1]!.id, "Q3 goals", rows[0]!.id)];
    let drop: Drop | null = null;
    const say = makeAnnouncements(() => rows, () => docs, () => drop);
    const active = { id: rows[1]!.id } as unknown as Active;
    const over = { id: rows[0]!.id } as unknown as Over;
    const messages = [
      say.onDragStart({ active }),
      say.onDragOver({ active, over }),
      say.onDragOver({ active, over: null }),
      say.onDragEnd({ active, over }),
      say.onDragEnd({ active, over: null }),
      say.onDragCancel({ active, over }),
    ];
    drop = { overId: rows[0]!.id, zone: "below", projection: topLevelAfter(rows[0]!.id) };
    messages.push(say.onDragEnd({ active, over }));
    for (const m of messages) {
      expect(m).toContain("“Q3 goals”");
      for (const r of rows) expect(m).not.toContain(r.id);
    }
    expect(say.onDragOver({ active, over })).toContain("“Roadmap”");
  });

  // The drop is what moved, not the row it ended over: an un-nest on one's
  // own row moves the page, and a drop over another row can leave it where
  // it was.
  it("announces where a drop put the page, or that it didn't move", () => {
    const rows: FlatRow[] = [
      { id: "roadmap", parentId: null, depth: 0, title: "Roadmap", descendantCount: 1 },
      { id: "goals", parentId: "roadmap", depth: 1, title: "Q3 goals", descendantCount: 0 },
    ];
    const docs = [doc("roadmap", "Roadmap", null), doc("goals", "Q3 goals", "roadmap")];
    let drop: Drop | null = { overId: "goals", zone: "below", projection: topLevelAfter("roadmap") };
    const say = makeAnnouncements(() => rows, () => docs, () => drop);
    const active = { id: "goals" } as unknown as Active;
    const own = { id: "goals" } as unknown as Over;
    const other = { id: "roadmap" } as unknown as Over;

    expect(say.onDragEnd({ active, over: own })).toBe("Moved “Q3 goals” to the top level, position 2 of 2");
    drop = null;
    expect(say.onDragEnd({ active, over: own })).toBe("“Q3 goals” was dropped. It didn't move.");
    expect(say.onDragEnd({ active, over: other })).toBe("“Q3 goals” was dropped. It didn't move.");
    expect(say.onDragEnd({ active, over: null })).toBe("“Q3 goals” was dropped outside the tree. It didn't move.");
  });

  // A drag released on a row link ends in a click on it. dnd-kit only stops
  // that click's propagation, so the browser still followed the href: a full
  // page load.
  it("swallows the click that ends a drag, and only that one", async () => {
    renderTree();
    await editorRows();
    const link = screen.getByTestId("doc-row-child");
    vi.useFakeTimers();

    startDrag(link);
    // The dragged row's subtree is unmounted, so it can never be a target;
    // the drag ghost carries no row testid.
    expect(screen.queryByTestId("doc-row-grandchild")).toBeNull();
    expect(screen.queryByTestId("tree-row-grandchild")).toBeNull();
    expect(document.querySelectorAll("[data-testid^='doc-row-']")).toHaveLength(2);
    expect(document.querySelectorAll("[data-testid^='tree-row-']")).toHaveLength(2);
    endDrag();
    expect(screen.getByTestId("doc-row-grandchild")).toBeInTheDocument();

    expect(fireEvent.click(link)).toBe(false);
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);

    // dnd-kit drops its own click listener after 50 ms. Ours went with the
    // first click, well before its 100 ms timeout.
    act(() => void vi.advanceTimersByTime(60));
    fireEvent.click(link);
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/child");
  });

  it("stops swallowing clicks 100 ms after a drag ends", async () => {
    renderTree();
    await editorRows();
    const link = screen.getByTestId("doc-row-child");
    vi.useFakeTimers();

    startDrag(link);
    endDrag();
    act(() => void vi.advanceTimersByTime(60));
    expect(fireEvent.click(link)).toBe(false);
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);

    startDrag(link);
    endDrag();
    act(() => void vi.advanceTimersByTime(100));
    // Seen on the link itself, before the router handles it.
    const seen: boolean[] = [];
    link.addEventListener("click", (e) => seen.push(e.defaultPrevented));
    fireEvent.click(link);
    expect(seen).toEqual([false]);
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/child");
  });
});

// Folder (0-28), Child (28-56), Grandchild (56-84), in a list 260px wide
// whose top is at y=0. The 28px above it count as the gap above Folder.
describe("DocTree — drops", () => {
  it("un-nests a page dragged one level left on its own row", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    fireEvent.mouseDown(screen.getByTestId("doc-row-grandchild"), { button: 0, clientX: 100, clientY: 70 });
    fireEvent.mouseMove(document, { clientX: 92, clientY: 70 }); // past 6px: the drag starts
    fireEvent.mouseMove(document, { clientX: 86, clientY: 70 }); // past one INDENT left
    const row = screen.getByTestId("tree-row-grandchild");
    expect(row).toHaveAttribute("data-drop-zone", "below");
    expect(row).toHaveAttribute("data-drop-depth", "1");
    fireEvent.mouseUp(document, { clientX: 86, clientY: 70 });
    // Dropped on its own row, and still moved.
    expect(announced()).toBe("Moved “Grandchild” into “Folder”, position 2 of 2");

    await waitFor(() =>
      expect(moves(fetchMock)).toEqual([
        { id: "grandchild", body: { parent_id: "folder", after_id: "child" } },
      ]),
    );
  });

  it("nests a page dropped on the middle of another row", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    fireEvent.mouseDown(screen.getByTestId("doc-row-grandchild"), { button: 0, clientX: 100, clientY: 70 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 62 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 14 });
    const folder = screen.getByTestId("tree-row-folder");
    expect(folder).toHaveAttribute("data-drop-zone", "into");
    expect(folder).toHaveAttribute("data-drop-depth", "1");
    fireEvent.mouseUp(document, { clientX: 100, clientY: 14 });

    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id: "grandchild", body: { parent_id: "folder" } }]));
  });

  it("sends nothing for a drop in place", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    fireEvent.mouseDown(screen.getByTestId("doc-row-child"), { button: 0, clientX: 100, clientY: 42 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 50 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 50 });
    // The drag is on (Child's subtree is unmounted), but there's nothing to show.
    expect(screen.queryByTestId("doc-row-grandchild")).toBeNull();
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 100, clientY: 50 });
    expect(announced()).toBe("“Child” was dropped. It didn't move.");

    await settle();
    expect(moves(fetchMock)).toEqual([]);
  });

  it("cancels a drop outside the tree", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    fireEvent.mouseDown(screen.getByTestId("doc-row-child"), { button: 0, clientX: 100, clientY: 42 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 34 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 3 }); // above Folder: a real move
    expect(screen.getByTestId("tree-row-folder")).toHaveAttribute("data-drop-zone", "above");
    fireEvent.mouseMove(document, { clientX: 400, clientY: 3 }); // right of the sidebar
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 400, clientY: 3 });

    await settle();
    expect(moves(fetchMock)).toEqual([]);
  });

  it("moves a page to the very top when dropped a few px above the first row", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    fireEvent.mouseDown(screen.getByTestId("doc-row-child"), { button: 0, clientX: 100, clientY: 42 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 34 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: -5 }); // above the list, in the band
    const folder = screen.getByTestId("tree-row-folder");
    expect(folder).toHaveAttribute("data-drop-zone", "above");
    expect(folder).toHaveAttribute("data-drop-depth", "0");
    fireEvent.mouseUp(document, { clientX: 100, clientY: -5 });

    await waitFor(() =>
      expect(moves(fetchMock)).toEqual([{ id: "child", body: { parent_id: null, before_id: "folder" } }]),
    );
  });

  it("still cancels a drop more than a row height above the list", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    fireEvent.mouseDown(screen.getByTestId("doc-row-child"), { button: 0, clientX: 100, clientY: 42 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: 34 });
    fireEvent.mouseMove(document, { clientX: 100, clientY: -27 }); // still in the 28px band
    expect(screen.getByTestId("tree-row-folder")).toHaveAttribute("data-drop-zone", "above");
    fireEvent.mouseMove(document, { clientX: 100, clientY: -29 }); // past it
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 100, clientY: -29 });
    expect(announced()).toBe("“Child” was dropped outside the tree. It didn't move.");

    await settle();
    expect(moves(fetchMock)).toEqual([]);
  });
});

/** Press on page `id`'s link at (100, y), then move the mouse through
 *  `path`: each point [dx, dy] from where it pressed. */
function mouseDrag(id: string, y: number, path: [number, number][], init: MouseEventInit = {}): HTMLElement {
  const link = screen.getByTestId(`doc-row-${id}`);
  fireEvent.mouseDown(link, { button: 0, clientX: 100, clientY: y, ...init });
  for (const [dx, dy] of path) fireEvent.mouseMove(document, { clientX: 100 + dx, clientY: y + dy });
  return link;
}

// Alpha (0-28) > Alpha one (28-56); Beta (56-84); Gamma (84-112). A trackpad
// click easily slips a few px: past the mouse sensor's 6px, a drag starts.
// Less than one INDENT of travel that moves nothing is still a click.
describe("DocTree — a click that slips", () => {
  it.each([
    ["sideways", "beta", 70, 7],
    ["left on a last child", "alpha1", 42, -7],
  ])("a click that slips 7px %s sends no move and opens the page", async (_how, id, y, dx) => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    // 4px, then 7px with a little vertical jitter: the drag starts at 7px.
    const link = mouseDrag(id, y, [[Math.sign(dx) * 4, 0], [dx, 1], [dx, 2]]);
    expect(screen.getByTestId(`tree-row-${id}`)).toHaveStyle({ opacity: "0.5" }); // dragging
    // No drop line: letting go here moves nothing.
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 100 + dx, clientY: y + 2 });
    // A click after it, if the browser sends one, loads no page in full.
    expect(fireEvent.click(link)).toBe(false);

    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent(`/doc/${id}`);
  });

  // INDENT + 2, not INDENT: a drag of exactly one INDENT sits on the
  // boundary of Math.trunc(deltaX / INDENT).
  it.each([
    ["right still nests", "beta", 70, INDENT + 2, { parent_id: "alpha", after_id: "alpha1" }],
    ["left still un-nests", "alpha1", 42, -(INDENT + 2), { parent_id: null, after_id: "alpha" }],
  ])("a drag just past one INDENT %s", async (_how, id, y, dx, body) => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    const link = mouseDrag(id, y, [[Math.sign(dx) * 4, 0], [Math.sign(dx) * 7, 1], [dx, 1]]);
    const row = screen.getByTestId(`tree-row-${id}`);
    expect(row).toHaveAttribute("data-drop-zone", "below");
    expect(row).toHaveAttribute("data-drop-depth", dx > 0 ? "1" : "0");
    fireEvent.mouseUp(document, { clientX: 100 + dx, clientY: y + 1 });
    expect(fireEvent.click(link)).toBe(false);

    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id, body }]));
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  // Collision is by pointer: pressed at the top edge of Beta (56-84), 8px up
  // is in the middle band of Alpha one (28-56), which nests. On main that
  // did nothing, and a drop less than one INDENT from the press moves
  // nothing (ruling F6): no drop line shows, and after a path that short it
  // is a click.
  it("a click that slips 8px up into the row above moves nothing and opens the page", async () => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    // 4px, then 7px: the drag starts; then 8px, to y=49.
    const link = mouseDrag("beta", 57, [[0, -4], [0, -7], [0, -8]]);
    expect(screen.getByTestId("tree-row-beta")).toHaveStyle({ opacity: "0.5" }); // dragging
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 100, clientY: 49 });
    expect(fireEvent.click(link)).toBe(false);

    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/beta");
  });

  // A hand that jitters on the way: the path runs past one INDENT, but the
  // pointer ends 8px from the press, in the middle band of Alpha one, which
  // nests. Less than one INDENT from the press, a drop moves nothing; after
  // a path of one INDENT or more it is no click either.
  it("a jittery slip that ends less than one INDENT from the press moves nothing and opens nothing", async () => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    // 4px, then 7px: the drag starts; then side to side, 8px up, to y=49.
    const link = mouseDrag("beta", 57, [[0, -4], [0, -7], [2, -8], [-2, -8], [2, -8], [-2, -8], [0, -8]]);
    expect(screen.getByTestId("tree-row-beta")).toHaveStyle({ opacity: "0.5" }); // dragging
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 100, clientY: 49 });
    expect(fireEvent.click(link)).toBe(false);

    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  // As react-router's Link does: going where the app already is replaces the
  // history entry, so Back doesn't stay on the same page.
  it("a slipped click on the page that is open replaces its history entry", async () => {
    renderTree("editor", SIBLINGS, "/doc/beta");
    await editorRows(SIBLINGS.length);
    layOut();
    const location = screen.getByTestId("location");

    mouseDrag("beta", 70, [[4, 0], [7, 1], [7, 2]]);
    fireEvent.mouseUp(document, { clientX: 107, clientY: 72 });
    await settle();
    expect(location).toHaveTextContent("/doc/beta");
    expect(location).toHaveAttribute("data-navigation", "REPLACE");

    mouseDrag("alpha1", 42, [[-4, 0], [-7, 1], [-7, 2]]);
    fireEvent.mouseUp(document, { clientX: 93, clientY: 44 });
    await settle();
    expect(location).toHaveTextContent("/doc/alpha1");
    expect(location).toHaveAttribute("data-navigation", "PUSH");
  });

  // Back where it pressed, a drop moves nothing; but a click is a path of
  // less than one INDENT, so a drag that went away first opens nothing.
  it("a drag that goes away and comes back moves nothing and opens nothing", async () => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    const link = mouseDrag("beta", 70, [[4, 0], [7, 1], [20, 1]]);
    expect(screen.getByTestId("tree-row-beta")).toHaveAttribute("data-drop-zone", "below");
    fireEvent.mouseMove(document, { clientX: 100, clientY: 70 }); // back where it pressed
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    fireEvent.mouseUp(document, { clientX: 100, clientY: 70 });
    expect(fireEvent.click(link)).toBe(false);

    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  // The sidebar scrolls under a drag (it autoscrolls near its edge, or the
  // wheel turns): the pointer has moved along the list by as much, and the
  // drop line counts that. Here it ends 7px below the press on screen, but
  // a row further down the list, over Gamma: the drop is what the line
  // shows.
  it("a drop after the sidebar scrolled under the pointer goes where the drop line shows", async () => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    // Pressed in Beta (56-84); 4px, then 7px down: the drag starts.
    const link = mouseDrag("beta", 66, [[0, 4], [0, 7]]);
    expect(screen.getByTestId("tree-row-beta")).toHaveStyle({ opacity: "0.5" }); // dragging
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    // One row: Gamma (now 56-84) is under the pointer, in its middle band.
    const sidebar = screen.getByTestId("sidebar");
    sidebar.scrollTop = ROW_H;
    fireEvent.scroll(sidebar);
    const gamma = screen.getByTestId("tree-row-gamma");
    expect(gamma).toHaveAttribute("data-drop-zone", "into");
    expect(gamma).toHaveAttribute("data-drop-depth", "1");
    fireEvent.mouseUp(document, { clientX: 100, clientY: 73 });
    expect(fireEvent.click(link)).toBe(false);

    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id: "beta", body: { parent_id: "gamma" } }]));
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  it("a slipped click on the row's More button opens no page", async () => {
    const fetchMock = renderTree("editor", SIBLINGS);
    await editorRows(SIBLINGS.length);
    layOut();

    const more = within(screen.getByTestId("tree-row-beta")).getByRole("button", { name: "More" });
    fireEvent.mouseDown(more, { button: 0, clientX: 240, clientY: 70 });
    fireEvent.mouseMove(document, { clientX: 244, clientY: 70 });
    fireEvent.mouseMove(document, { clientX: 247, clientY: 71 });
    fireEvent.mouseMove(document, { clientX: 247, clientY: 72 });
    expect(screen.getByTestId("tree-row-beta")).toHaveStyle({ opacity: "0.5" }); // dragging
    fireEvent.mouseUp(document, { clientX: 247, clientY: 72 });

    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });

  // With a modifier key the browser opens a link elsewhere (a new tab or
  // window), so a slipped click doesn't open it here.
  it.each(["metaKey", "ctrlKey", "shiftKey", "altKey"])(
    "a slipped click with %s moves nothing and opens nothing here",
    async (key) => {
      const fetchMock = renderTree("editor", SIBLINGS);
      await editorRows(SIBLINGS.length);
      layOut();

      const link = mouseDrag("beta", 70, [[4, 0], [7, 1], [7, 2]], { [key]: true });
      fireEvent.mouseUp(document, { clientX: 107, clientY: 72 });
      fireEvent.click(link, { [key]: true });

      await settle();
      expect(moves(fetchMock)).toEqual([]);
      expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
    },
  );
});

/** One touch on `el` at (x, y): its start, a move, or its end. */
function touch(el: Element, type: "touchStart" | "touchMove" | "touchEnd", x: number, y: number) {
  const t = { clientX: x, clientY: y, pageX: x, pageY: y, identifier: 0, target: el };
  fireEvent[type](el, type === "touchEnd" ? { touches: [], changedTouches: [t] } : { touches: [t], changedTouches: [t] });
}

// Folder (0-28), Child (28-56), Grandchild (56-84). On touch a drag starts
// once the finger has been held still for 250 ms.
describe("DocTree — touch", () => {
  // Spec bug C.8: touch drags fought scrolling.
  it("a swipe scrolls instead of dragging", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    vi.useFakeTimers();

    const link = screen.getByTestId("doc-row-grandchild");
    touch(link, "touchStart", 100, 70);
    touch(link, "touchMove", 100, 50); // 20px within the 250 ms: a swipe
    act(() => void vi.advanceTimersByTime(300));
    touch(link, "touchMove", 100, 14); // over Folder's middle
    expect(screen.getByTestId("tree-row-grandchild")).not.toHaveStyle({ opacity: "0.5" }); // not dragging
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    touch(link, "touchEnd", 100, 14);
    vi.useRealTimers();

    await settle();
    expect(moves(fetchMock)).toEqual([]);
  });

  it("press, hold and move drags", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    vi.useFakeTimers();

    const link = screen.getByTestId("doc-row-grandchild");
    touch(link, "touchStart", 100, 70);
    act(() => void vi.advanceTimersByTime(260));
    touch(link, "touchMove", 100, 62);
    touch(link, "touchMove", 100, 14); // over Folder's middle
    expect(screen.getByTestId("tree-row-folder")).toHaveAttribute("data-drop-zone", "into");
    touch(link, "touchEnd", 100, 14);
    vi.useRealTimers();

    await waitFor(() => expect(moves(fetchMock)).toEqual([{ id: "grandchild", body: { parent_id: "folder" } }]));
  });

  it("a tap held past 250 ms that rolls 7px sideways moves nothing and opens the page", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    vi.useFakeTimers();

    const link = screen.getByTestId("doc-row-grandchild");
    touch(link, "touchStart", 100, 70);
    act(() => void vi.advanceTimersByTime(260));
    // Grandchild is a last child: 7px left is less than the INDENT that un-nests it.
    touch(link, "touchMove", 93, 71);
    expect(screen.getByTestId("tree-row-grandchild")).toHaveStyle({ opacity: "0.5" }); // dragging
    expect(document.querySelector("[data-drop-zone]")).toBeNull();
    touch(link, "touchEnd", 93, 71);
    vi.useRealTimers();

    await settle();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/grandchild");
  });

  // A long press opens the row menu (Android sends contextmenu while the
  // finger is still down), and the drag has started by then.
  it("a long press that opens the row menu opens no page", async () => {
    const fetchMock = renderTree();
    await editorRows();
    layOut();
    vi.useFakeTimers();

    const link = screen.getByTestId("doc-row-grandchild");
    touch(link, "touchStart", 100, 70);
    act(() => void vi.advanceTimersByTime(600));
    fireEvent.contextMenu(link, { clientX: 100, clientY: 70 });
    touch(link, "touchEnd", 100, 70);
    vi.useRealTimers();

    await settle();
    expect(screen.getByTestId("context-menu")).toBeInTheDocument();
    expect(moves(fetchMock)).toEqual([]);
    expect(screen.getByTestId("location")).toHaveTextContent(/^\/$/);
  });
});
