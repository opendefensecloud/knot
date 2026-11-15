/**
 * The version pane: the page as it was right after an entry, read-only,
 * with the entry's own changes highlighted, and "Restore here".
 *
 * `historyApi`, `docsApi.get`, `docsApi.list` (the sidebar's list, empty
 * unless a test says otherwise) and `boardsApi.getSvg` are mocked. An HTTP
 * error is mocked as a resolved `{error}`, as `apiFetch` answers it; a
 * rejection stands for a parse or network error only. The real
 * `HistoryDocViewer` runs (React node views included) inside a counting
 * wrapper, so a test can tell how often the viewer mounted.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import type { JSONContent } from "@tiptap/core";
import { EditorContent, useEditor } from "@tiptap/react";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { createElement, useEffect, type ComponentProps } from "react";
import { MemoryRouter, useLocation } from "react-router-dom";
import * as v from "valibot";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { Doc, DocWithRole, parse, type HistoryEntry, type HistoryView } from "../../../lib/validators";
import { useUi } from "../../../stores/ui";
import { createSchemaExtensions } from "../../editor/extensions";
import { copy } from "./copy";
import { formatDateTime } from "./timeline";
import { viewerContent } from "./viewerContent";
import { VersionView } from "./VersionView";

const api = vi.hoisted(() => ({ view: vi.fn(), restoreEntry: vi.fn(), restoreAsNew: vi.fn() }));
const docs = vi.hoisted(() => ({ get: vi.fn(), list: vi.fn() }));
const boards = vi.hoisted(() => ({ getSvg: vi.fn() }));
const mounts = vi.hoisted(() => ({ count: 0 }));

vi.mock("../../../lib/history.api", () => ({
  historyApi: { view: api.view, restoreEntry: api.restoreEntry, restoreAsNew: api.restoreAsNew },
}));

vi.mock("../docs.api", async (importOriginal) => {
  const m = await importOriginal<typeof import("../docs.api")>();
  return { ...m, docsApi: { ...m.docsApi, get: docs.get, list: docs.list } };
});

vi.mock("../../../lib/boards.api", async (importOriginal) => {
  const m = await importOriginal<typeof import("../../../lib/boards.api")>();
  return { ...m, boardsApi: { ...m.boardsApi, getSvg: boards.getSvg } };
});

// The real viewer, counted once per mount.
vi.mock("./HistoryDocViewer", async (importOriginal) => {
  const m = await importOriginal<typeof import("./HistoryDocViewer")>();
  function CountedViewer(props: ComponentProps<typeof m.HistoryDocViewer>) {
    useEffect(() => {
      mounts.count += 1;
    }, []);
    return createElement(m.HistoryDocViewer, props);
  }
  return { ...m, HistoryDocViewer: CountedViewer };
});

// An ES module export cannot be spied on in place.
vi.mock("./viewerContent", async (importOriginal) => {
  const m = await importOriginal<typeof import("./viewerContent")>();
  return { ...m, viewerContent: vi.fn(m.viewerContent) };
});

const T1 = "2026-11-14T10:00:00Z";
const T2 = "2026-11-14T10:05:00Z";

const RENDERED = join(
  import.meta.dirname,
  "../../../../../crates/knot-history/tests/fixtures/pm_json_rendered.json",
);

function entry(id: number, over: Partial<HistoryEntry> = {}): HistoryEntry {
  return {
    id,
    kind: "session",
    author: { key: "u-ann", name: "Ann" },
    started_at: "2026-11-14T09:00:00Z",
    ended_at: T1,
    open: false,
    words_added: 3,
    words_removed: 1,
    meta: {},
    ...over,
  };
}

const doc = (...content: JSONContent[]): JSONContent => ({ type: "doc", content });
const p = (...content: JSONContent[]): JSONContent => ({ type: "paragraph", content });
const text = (s: string, marks?: unknown[]): JSONContent =>
  (marks ? { type: "text", text: s, marks } : { type: "text", text: s }) as JSONContent;

function viewOk(d: JSONContent, over: Partial<HistoryView> = {}): { ok: HistoryView } {
  return {
    ok: {
      entry_id: 7,
      kind: "session",
      changes: true,
      can_restore: true,
      can_erase: false,
      doc: d as HistoryView["doc"],
      ...over,
    },
  };
}

function apiError(status: number, code: string, details: Record<string, unknown> = {}) {
  return { error: { status, code, message: code, details } };
}

/** A row of `GET /api/docs` as the server sends it, parsed as `docsApi.list`
 *  parses it: by default the page `d1`, editable, under a visible parent. */
function listRow(over: Record<string, unknown> = {}) {
  return {
    id: "d1",
    workspace_id: "w1",
    parent_id: "p1",
    title: "Plan",
    sort_key: "m",
    icon: null,
    created_by: "u-ann",
    archived: false,
    is_template: false,
    archived_at: null,
    archive_root_id: null,
    restricted: false,
    within_restricted: false,
    parent_hidden: false,
    effective_role: "editor",
    ...over,
  };
}

function listOk(...rows: Record<string, unknown>[]): { ok: Doc[] } {
  return { ok: parse(v.array(Doc), rows) };
}

/** The doc a restore as a new page answers. */
const NEW_DOC = parse(Doc, { ...listRow({ id: "n1", title: "Plan (version of …)" }), effective_role: null });

/** Where the router is: VersionView navigates to the new page. */
function LocationProbe() {
  const location = useLocation();
  return <output data-testid="location">{location.pathname}</output>;
}

type Props = ComponentProps<typeof VersionView>;

function renderView(over: Partial<Props> = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const invalidate = vi.spyOn(qc, "invalidateQueries");
  const onBack = vi.fn();
  const onRestored = vi.fn();
  const onShowChanges = vi.fn();
  let props: Props = {
    docId: "d1",
    entry: entry(7),
    showChanges: true,
    onShowChanges,
    canRestore: true,
    onBack,
    onRestored,
    focusBack: false,
    ...over,
  };
  const ui = (pr: Props) => (
    <MemoryRouter initialEntries={["/doc/d1"]}>
      <QueryClientProvider client={qc}>
        <VersionView {...pr} />
        <LocationProbe />
      </QueryClientProvider>
    </MemoryRouter>
  );
  const r = render(ui(props));
  return {
    qc,
    invalidate,
    onBack,
    onRestored,
    onShowChanges,
    rerender(next: Partial<Props>) {
      props = { ...props, ...next };
      r.rerender(ui(props));
    },
  };
}

function invalidatedKeys(spy: { mock: { calls: unknown[][] } }): unknown[] {
  return spy.mock.calls.map((c) => (c[0] as { queryKey?: unknown } | undefined)?.queryKey);
}

function toastTexts(): string[] {
  return useUi.getState().toasts.map((t) => t.text);
}

const versionDoc = () => screen.getByTestId("history-version-doc");

beforeEach(() => {
  mounts.count = 0;
  vi.mocked(viewerContent).mockClear();
  docs.get.mockResolvedValue({ ok: { id: "d1", title: "Plan" } });
  docs.list.mockResolvedValue({ ok: [] });
  boards.getSvg.mockResolvedValue({ ok: '<svg xmlns="http://www.w3.org/2000/svg"></svg>' });
  useUi.setState({ toasts: [] });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
  api.view.mockReset();
  api.restoreEntry.mockReset();
  api.restoreAsNew.mockReset();
  docs.get.mockReset();
  docs.list.mockReset();
  boards.getSvg.mockReset();
  useUi.setState({ toasts: [] });
});

describe("VersionView", () => {
  it("shows the version with its changes highlighted by default", async () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("old "), text("new", [{ type: "historyInsert" }])))));
    renderView();
    await waitFor(() => expect(versionDoc().querySelector("ins.history-insert")).not.toBeNull());
    expect(versionDoc().querySelector("ins.history-insert")).toHaveTextContent("new");
    expect(api.view).toHaveBeenCalledTimes(1);
    expect(api.view.mock.calls[0]!.slice(0, 3)).toEqual(["d1", 7, true]);
    const toggle = screen.getByTestId("history-show-changes");
    expect(toggle).toHaveAttribute("role", "switch");
    expect(toggle).toHaveAttribute("aria-checked", "true");
    expect(toggle).toBeEnabled();
  });

  it("turns highlighting off with the switch", async () => {
    api.view
      .mockResolvedValueOnce(viewOk(doc(p(text("new", [{ type: "historyInsert" }])))))
      .mockResolvedValueOnce(viewOk(doc(p(text("new"))), { changes: false }));
    const v = renderView();
    await waitFor(() => expect(versionDoc().querySelector("ins")).not.toBeNull());
    fireEvent.click(screen.getByTestId("history-show-changes"));
    expect(v.onShowChanges).toHaveBeenCalledWith(false);
    v.rerender({ showChanges: false });
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(2));
    expect(api.view.mock.calls[1]!.slice(0, 3)).toEqual(["d1", 7, false]);
    await waitFor(() => expect(versionDoc().querySelector("ins")).toBeNull());
    expect(versionDoc()).toHaveTextContent("new");
    expect(screen.getByTestId("history-show-changes")).toHaveAttribute("aria-checked", "false");
    expect(screen.queryByTestId("history-changes-unavailable")).toBeNull();
  });

  it("turning highlighting back on never reads as unavailable", async () => {
    api.view.mockResolvedValueOnce(viewOk(doc(p(text("new"))), { changes: false }));
    const v = renderView({ showChanges: false });
    await waitFor(() => expect(versionDoc()).toHaveTextContent("new"));
    let answer!: (r: unknown) => void;
    api.view.mockReturnValueOnce(new Promise((res) => (answer = res)));
    v.rerender({ showChanges: true });
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(2));
    expect(api.view.mock.calls[1]!.slice(0, 3)).toEqual(["d1", 7, true]);
    // The changes=0 answer stands in while the changes=1 one loads.
    expect(versionDoc()).toHaveTextContent("new");
    expect(screen.queryByTestId("history-changes-unavailable")).toBeNull();
    const toggle = screen.getByTestId("history-show-changes");
    expect(toggle).toBeEnabled();
    expect(toggle).toHaveAttribute("aria-checked", "true");
    act(() => answer(viewOk(doc(p(text("new", [{ type: "historyInsert" }]))))));
    await waitFor(() => expect(versionDoc().querySelector("ins.history-insert")).not.toBeNull());
    expect(screen.queryByTestId("history-changes-unavailable")).toBeNull();
  });

  it.each([
    ["a baseline", entry(7, { kind: "baseline", author: null, words_added: null, words_removed: null })],
    [
      "the rollout entry",
      entry(7, { kind: "baseline", author: null, words_added: null, words_removed: null, meta: { rollout: true } }),
    ],
  ])("disables Show changes for earlier versions (%s)", async (_name, e) => {
    api.view.mockResolvedValue(viewOk(doc(p(text("earlier"))), { kind: "baseline", changes: false }));
    const v = renderView({ entry: e });
    await waitFor(() => expect(versionDoc()).toHaveTextContent("earlier"));
    const toggle = screen.getByTestId("history-show-changes");
    expect(toggle).toBeDisabled();
    expect(toggle).toHaveAttribute("aria-checked", "false");
    expect(screen.queryByTestId("history-changes-unavailable")).toBeNull();
    expect(api.view.mock.calls[0]!.slice(0, 3)).toEqual(["d1", 7, false]);
    v.rerender({ showChanges: false });
    v.rerender({ showChanges: true });
    await act(async () => {
      await Promise.resolve();
    });
    expect(api.view).toHaveBeenCalledTimes(1);
    expect(screen.queryByTestId("history-changes-unavailable")).toBeNull();
  });

  it("a_version_the_schema_rejects_says_it_cant_be_shown", async () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("x", [{ type: "comment", attrs: { id: "c1" } }])))));
    renderView();
    expect(await screen.findByTestId("history-version-error")).toHaveTextContent(copy.cantShow);
    expect(versionDoc()).not.toHaveTextContent("x");
  });

  it("an_empty_version_renders_an_empty_paragraph", async () => {
    api.view.mockResolvedValue(viewOk({ type: "doc", content: [] }));
    renderView();
    await waitFor(() => expect(versionDoc().querySelectorAll("p")).toHaveLength(1));
    expect(versionDoc().querySelector("p")).toHaveTextContent("");
    expect(screen.queryByTestId("history-version-error")).toBeNull();
  });

  it("a_too_large_version_says_so", async () => {
    api.view.mockResolvedValue(apiError(409, "history.too_large"));
    renderView();
    expect(await screen.findByTestId("history-version-error")).toHaveTextContent(copy.tooLarge);
    expect(api.view).toHaveBeenCalledTimes(1);
    // An error leaves the switch as it was.
    expect(screen.getByTestId("history-show-changes")).toHaveAttribute("aria-checked", "true");
  });

  it("retries a busy version, then says history is busy", async () => {
    vi.useFakeTimers();
    api.view.mockResolvedValue(apiError(503, "history.busy"));
    renderView();
    await act(() => vi.advanceTimersByTimeAsync(0));
    expect(api.view).toHaveBeenCalledTimes(1);
    expect(screen.getByTestId("history-version-busy")).toHaveTextContent(copy.busyRetrying);
    expect(screen.getByTestId("history-version-busy").closest('[aria-live="polite"]')).not.toBeNull();
    expect(screen.queryByTestId("history-version-error")).toBeNull();
    await act(() => vi.advanceTimersByTimeAsync(4_900));
    expect(api.view).toHaveBeenCalledTimes(1);
    await act(() => vi.advanceTimersByTimeAsync(100));
    expect(api.view).toHaveBeenCalledTimes(2);
    await act(() => vi.advanceTimersByTimeAsync(5_000));
    expect(api.view).toHaveBeenCalledTimes(3);
    expect(screen.getByTestId("history-version-busy")).toBeInTheDocument();
    await act(() => vi.advanceTimersByTimeAsync(5_000));
    expect(api.view).toHaveBeenCalledTimes(4);
    await act(() => vi.advanceTimersByTimeAsync(100));
    expect(screen.getByTestId("history-version-error")).toHaveTextContent(copy.busy);
    expect(screen.queryByTestId("history-version-busy")).toBeNull();
    await act(() => vi.advanceTimersByTimeAsync(30_000));
    expect(api.view).toHaveBeenCalledTimes(4);
  });

  it("says a vanished entry is no longer available", async () => {
    api.view.mockResolvedValue(apiError(404, "history.not_found"));
    renderView();
    expect(await screen.findByTestId("history-version-error")).toHaveTextContent(copy.notFound);
    expect(api.view).toHaveBeenCalledTimes(1);
    cleanup();

    api.view.mockReset();
    api.view.mockResolvedValue(apiError(500, "history.view_failed"));
    renderView();
    expect(await screen.findByTestId("history-version-error")).toHaveTextContent(copy.cantShow);
    expect(api.view).toHaveBeenCalledTimes(1);
  });

  it("a_selected_open_session_refetches_when_its_end_moves", async () => {
    api.view.mockResolvedValueOnce(viewOk(doc(p(text("one"))))).mockResolvedValueOnce(viewOk(doc(p(text("one two")))));
    const v = renderView({ entry: entry(7, { open: true, ended_at: T1 }) });
    await waitFor(() => expect(versionDoc()).toHaveTextContent(/^one$/));
    expect(api.view).toHaveBeenCalledTimes(1);
    v.rerender({ entry: entry(7, { open: true, ended_at: T2 }) });
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(versionDoc()).toHaveTextContent(/^one two$/));
  });

  it("keeps the old version visible while the new end loads", async () => {
    api.view.mockResolvedValueOnce(viewOk(doc(p(text("one")))));
    const v = renderView({ entry: entry(7, { open: true, ended_at: T1 }) });
    await waitFor(() => expect(versionDoc()).toHaveTextContent(/^one$/));
    expect(screen.getByTestId("history-restore")).toBeEnabled();
    expect(screen.getByTestId("history-version")).not.toHaveAttribute("aria-busy", "true");

    let answer!: (r: unknown) => void;
    api.view.mockReturnValueOnce(new Promise((res) => (answer = res)));
    v.rerender({ entry: entry(7, { open: true, ended_at: T2 }) });
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(2));
    expect(versionDoc()).toHaveTextContent(/^one$/);
    expect(screen.queryByTestId("history-version-loading")).toBeNull();
    expect(screen.getByTestId("history-version")).toHaveAttribute("aria-busy", "true");
    expect(screen.getByTestId("history-restore")).toBeDisabled();
    expect(mounts.count).toBe(1);

    act(() => answer(viewOk(doc(p(text("one two"))))));
    await waitFor(() => expect(versionDoc()).toHaveTextContent(/^one two$/));
    expect(screen.getByTestId("history-restore")).toBeEnabled();
    expect(screen.getByTestId("history-version")).not.toHaveAttribute("aria-busy", "true");
    expect(mounts.count).toBe(2);
  });

  it("shows Loading version… while nothing of the entry is loaded", async () => {
    let answer!: (r: unknown) => void;
    api.view.mockReturnValueOnce(new Promise((res) => (answer = res)));
    renderView();
    const loading = await screen.findByTestId("history-version-loading");
    expect(loading).toHaveTextContent(copy.loadingVersion);
    expect(loading.closest('[aria-live="polite"]')).not.toBeNull();
    expect(screen.getByTestId("history-version")).toHaveAttribute("aria-busy", "true");
    expect(screen.queryByTestId("history-restore")).toBeNull();
    act(() => answer(viewOk(doc(p(text("one"))))));
    await waitFor(() => expect(screen.queryByTestId("history-version-loading")).toBeNull());
  });

  it("another entry never shows the previous one's text", async () => {
    api.view.mockResolvedValueOnce(viewOk(doc(p(text("seven")))));
    const v = renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("seven"));
    api.view.mockReturnValueOnce(new Promise(() => {}));
    v.rerender({ entry: entry(8) });
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(2));
    expect(screen.queryByTestId("history-version-doc")).toBeNull();
    expect(screen.getByTestId("history-version-loading")).toBeInTheDocument();
  });

  it("a superseded view request is aborted", async () => {
    api.view.mockReturnValue(new Promise(() => {}));
    const v = renderView();
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(1));
    const first = api.view.mock.calls[0]![3] as AbortSignal;
    expect(first).toBeInstanceOf(AbortSignal);
    expect(first.aborted).toBe(false);
    v.rerender({ entry: entry(8) });
    await waitFor(() => expect(api.view).toHaveBeenCalledTimes(2));
    await waitFor(() => expect(first.aborted).toBe(true));
    expect((api.view.mock.calls[1]![3] as AbortSignal).aborted).toBe(false);
  });

  it("a version shown without changes says so", async () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("plain"))), { changes: false }));
    renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("plain"));
    const toggle = screen.getByTestId("history-show-changes");
    expect(toggle).toHaveAttribute("aria-checked", "false");
    expect(toggle).toBeDisabled();
    expect(screen.getByTestId("history-changes-unavailable")).toHaveTextContent(copy.changesUnavailable);
    expect(screen.queryByTestId("history-version-error")).toBeNull();
  });

  it("a_version_with_edited_code_renders_with_changes", async () => {
    const ins = { type: "historyInsert" };
    api.view.mockResolvedValue(
      viewOk(
        doc(
          {
            type: "code_block",
            attrs: { language: "rust", historyChange: "changed" },
            content: [text("let x = 1;\n"), text("let y = 2;", [ins])],
          },
          p(text("see "), text("y", [{ type: "code", attrs: {} }, ins])),
          p(text("link", [{ type: "code", attrs: true }, { type: "link", attrs: { href: "https://example.com" } }])),
          p(text("bold", [{ type: "bold", attrs: true }, { type: "code", attrs: true }])),
        ),
      ),
    );
    renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("let y = 2;"));
    expect(screen.queryByTestId("history-version-error")).toBeNull();
    const block = versionDoc().querySelector('[data-history-change="changed"]');
    expect(block).not.toBeNull();
    expect(block!.querySelector("pre")).not.toBeNull();
    expect(block!.querySelector("ins.history-insert")).toHaveTextContent("let y = 2;");
    const inlineIns = Array.from(versionDoc().querySelectorAll("p ins.history-insert"));
    expect(inlineIns.map((el) => el.textContent)).toEqual(["y"]);
    expect(versionDoc().querySelector("a[href='https://example.com'] code, code a[href='https://example.com']")).not.toBeNull();
    expect(versionDoc().querySelector("strong code, code strong")).not.toBeNull();
  });

  it("a version with stray text renders as the live editor shows it", async () => {
    const cases = JSON.parse(readFileSync(RENDERED, "utf8")) as { name: string; doc: JSONContent }[];
    const pick = (name: string) => cases.find((c) => c.name === name)!.doc;
    api.view.mockResolvedValueOnce(viewOk(pick("editor/damaged/item-text-only")));
    renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("a"));
    expect(versionDoc().querySelector("li")).toHaveTextContent("a");
    expect(screen.queryByTestId("history-version-error")).toBeNull();
    cleanup();

    api.view.mockResolvedValueOnce(viewOk(pick("editor/damaged/unknown-inline")));
    renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("Unsupported content (mention_chip)"));
    expect(versionDoc()).toHaveTextContent("a");
    expect(screen.queryByTestId("history-version-error")).toBeNull();
  });

  it("a version holding a board and an attachment renders read-only", async () => {
    const attachment = {
      type: "attachment",
      attrs: { url: "/api/blobs/f1", name: "spec.pdf", size: 2048, contentType: "application/pdf" },
    };
    api.view.mockResolvedValue(
      viewOk(doc({ type: "excalidraw_board", attrs: { board_id: "b1", label: null, historyChange: "added" } }, attachment)),
    );
    renderView();
    const board = await screen.findByTestId("excalidraw-board");
    expect(screen.queryByTestId("history-version-error")).toBeNull();
    expect(board).toHaveAttribute("data-history-change", "added");
    expect(screen.queryByTestId("excalidraw-board-open")).toBeNull();
    expect(screen.getByTestId("attachment-node")).toHaveTextContent("spec.pdf");
    await waitFor(() => expect(boards.getSvg).toHaveBeenCalledWith("b1"));
    cleanup();

    // The live editor with the same board offers to open it.
    function Live({ content }: { content: JSONContent }) {
      const editor = useEditor({ extensions: createSchemaExtensions({}), content });
      return <EditorContent editor={editor} />;
    }
    render(
      <MemoryRouter>
        <QueryClientProvider client={new QueryClient()}>
          <Live content={doc({ type: "excalidraw_board", attrs: { board_id: "b1", label: null } }, attachment)} />
        </QueryClientProvider>
      </MemoryRouter>,
    );
    expect(await screen.findByTestId("excalidraw-board-open")).toBeInTheDocument();
  });

  it("a reply that is not JSON says the version can't be shown", async () => {
    api.view.mockRejectedValue(new SyntaxError("Unexpected token '<'"));
    renderView();
    expect(await screen.findByTestId("history-version-error")).toHaveTextContent(copy.cantShow);
    expect(api.view).toHaveBeenCalledTimes(1);
  });

  it("a page being rebuilt says so", async () => {
    api.view.mockResolvedValue(apiError(409, "history.rebuilding"));
    renderView();
    expect(await screen.findByTestId("history-version-error")).toHaveTextContent(copy.rebuilding);
    expect(api.view).toHaveBeenCalledTimes(1);
    cleanup();

    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockReset();
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue(apiError(409, "history.rebuilding"));
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.rebuilding]));
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("a rate-limited restore says how long to wait", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue({
      error: { status: 429, code: "rate.limited", message: "too many requests", details: { retry_after_secs: 11 } },
    });
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() =>
      expect(useUi.getState().toasts).toMatchObject([
        { kind: "warn", text: "You're doing that too fast. Try again in 11 s.", key: "limit" },
      ]),
    );
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("asks before restoring and does nothing when declined", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    expect(confirm).toHaveBeenCalledWith(copy.restoreConfirm);
    expect(api.restoreEntry).not.toHaveBeenCalled();
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("restores after confirming and says so", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    const e = entry(7);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    let answer!: (r: unknown) => void;
    api.restoreEntry.mockReturnValue(new Promise((res) => (answer = res)));
    const v = renderView({ entry: e });
    const button = await screen.findByTestId("history-restore");
    expect(button).toHaveTextContent(copy.restoreHere);
    fireEvent.click(button);
    await waitFor(() => expect(api.restoreEntry).toHaveBeenCalledWith("d1", 7, e.ended_at));
    await waitFor(() => expect(screen.getByTestId("history-restore")).toHaveTextContent(copy.restoring));
    expect(screen.getByTestId("history-restore")).toBeDisabled();
    act(() => answer({ ok: { outcome: "restored" } }));
    await waitFor(() => expect(v.onRestored).toHaveBeenCalledTimes(1));
    expect(invalidatedKeys(v.invalidate)).toEqual(
      expect.arrayContaining([
        ["doc", "d1"],
        ["contributors", "d1"],
        ["history", "d1"],
      ]),
    );
    expect(toastTexts()).toEqual(["Restored."]);
    expect(useUi.getState().toasts[0]?.kind).toBe("info");
  });

  it("a fallback restore says comments may have lost their place", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue({ ok: { outcome: "fallback" } });
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(v.onRestored).toHaveBeenCalledTimes(1));
    expect(useUi.getState().toasts).toMatchObject([{ kind: "warn", text: copy.restoredFallback }]);
    expect(invalidatedKeys(v.invalidate)).toEqual(
      expect.arrayContaining([
        ["doc", "d1"],
        ["contributors", "d1"],
        ["history", "d1"],
      ]),
    );
  });

  it("restoring the current version says it already matches", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue({ ok: { outcome: "unchanged" } });
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(v.onRestored).toHaveBeenCalledTimes(1));
    expect(toastTexts()).toEqual([copy.restoreUnchanged]);
    expect(invalidatedKeys(v.invalidate)).toEqual(expect.arrayContaining([["history", "d1"]]));
  });

  it("a busy page asks to try again", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue(apiError(409, "history.restore_busy"));
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreBusy]));
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("a page too large to restore here suggests a new page", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    docs.list.mockResolvedValue(listOk(listRow({ id: "other" }), listRow()));
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValueOnce(apiError(409, "restore.too_large"));
    const v = renderView();
    await screen.findByTestId("history-restore-as-new");
    fireEvent.click(screen.getByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreTooLarge]));
    expect(copy.restoreTooLarge).toContain("as a new page");

    useUi.setState({ toasts: [] });
    api.restoreEntry.mockResolvedValueOnce(apiError(409, "history.too_large"));
    await waitFor(() => expect(screen.getByTestId("history-restore")).toBeEnabled());
    fireEvent.click(screen.getByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreVersionTooLarge]));

    useUi.setState({ toasts: [] });
    api.restoreEntry.mockResolvedValueOnce(apiError(500, "history.restore_failed"));
    await waitFor(() => expect(screen.getByTestId("history-restore")).toBeEnabled());
    fireEvent.click(screen.getByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreFailed]));
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it.each([
    ["a template", () => listOk(listRow({ is_template: true }))],
    ["no row for the page", () => listOk(listRow({ id: "other" }))],
  ])("a page too large to restore here says only that when it cannot be copied (%s)", async (_name, list) => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    docs.list.mockResolvedValue(list());
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue(apiError(409, "restore.too_large"));
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreTooLargeNoCopy]));
    expect(toastTexts()[0]).not.toContain("new page");
    expect(screen.queryByTestId("history-restore-as-new")).toBeNull();
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("restores as a new page and opens it", async () => {
    const confirm = vi.spyOn(window, "confirm");
    const e = entry(7);
    docs.list.mockResolvedValue(listOk(listRow()));
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    let answer!: (r: unknown) => void;
    api.restoreAsNew.mockReturnValue(new Promise((res) => (answer = res)));
    const v = renderView({ entry: e });
    const button = await screen.findByTestId("history-restore-as-new");
    expect(button).toHaveTextContent(copy.restoreAsNew);
    await waitFor(() => expect(button).toBeEnabled());
    fireEvent.click(button);
    fireEvent.click(button);
    expect(confirm).not.toHaveBeenCalled();
    await waitFor(() =>
      expect(api.restoreAsNew).toHaveBeenCalledWith("d1", 7, {
        version_label: formatDateTime(e.ended_at),
        ended_at: e.ended_at,
      }),
    );
    expect(api.restoreAsNew).toHaveBeenCalledTimes(1);
    await waitFor(() => expect(screen.getByTestId("history-restore-as-new")).toHaveTextContent(copy.restoringAsNew));
    expect(screen.getByTestId("history-restore-as-new")).toBeDisabled();
    expect(screen.getByTestId("history-restore")).toBeDisabled();
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/d1");

    act(() => answer({ ok: NEW_DOC }));
    await waitFor(() => expect(v.onRestored).toHaveBeenCalledTimes(1));
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/n1");
    expect(toastTexts()).toEqual([copy.restoredAsNew]);
    expect(invalidatedKeys(v.invalidate)).toContainEqual(["docs"]);
    expect(window.sessionStorage.getItem("knot.editMode.n1")).toBe("1");
    window.sessionStorage.removeItem("knot.editMode.n1");
  });

  it("shows Restore as new page from the sidebar's docs list", async () => {
    docs.get.mockResolvedValue({
      ok: parse(DocWithRole, { ...listRow(), effective_role: "editor", parent_hidden: false }),
    });
    docs.list.mockResolvedValue(listOk(listRow()));
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView();
    const button = await screen.findByTestId("history-restore-as-new");
    expect(button).toHaveTextContent(copy.restoreAsNew);
    expect(docs.list).toHaveBeenCalledTimes(1);
    // The page's own record carries no parent_hidden: it is not the source.
    const record = await (docs.get.mock.results[0]!.value as Promise<{ ok: object }>);
    expect("parent_hidden" in record.ok).toBe(false);
    // The copy is the secondary action, after Restore here.
    const restore = screen.getByTestId("history-restore");
    expect(restore.compareDocumentPosition(button) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });

  it.each([
    ["a template", () => Promise.resolve(listOk(listRow({ is_template: true })))],
    ["a hidden parent", () => Promise.resolve(listOk(listRow({ parent_hidden: true })))],
    ["no row for the page", () => Promise.resolve(listOk(listRow({ id: "other" })))],
    ["a list still loading", () => new Promise(() => {})],
    ["a list that failed", () => Promise.resolve(apiError(500, "internal"))],
    ["a list that rejected", () => Promise.reject(new TypeError("Failed to fetch"))],
  ])("hides Restore as new page for %s", async (_name, list) => {
    docs.list.mockImplementation(list);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView();
    await screen.findByTestId("history-restore");
    await waitFor(() => expect(docs.list).toHaveBeenCalled());
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.queryByTestId("history-restore-as-new")).toBeNull();
  });

  it("hides both restores when the version can't be restored", async () => {
    docs.list.mockResolvedValue(listOk(listRow()));
    api.view.mockResolvedValue(viewOk(doc(p(text("one"))), { can_restore: false }));
    renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("one"));
    await waitFor(() => expect(docs.list).toHaveBeenCalled());
    expect(screen.queryByTestId("history-restore")).toBeNull();
    expect(screen.queryByTestId("history-restore-as-new")).toBeNull();
    cleanup();

    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView({ canRestore: false });
    await waitFor(() => expect(versionDoc()).toHaveTextContent("one"));
    expect(screen.queryByTestId("history-restore")).toBeNull();
    expect(screen.queryByTestId("history-restore-as-new")).toBeNull();
  });

  it("says why a new page could not be made", async () => {
    docs.list.mockResolvedValue(listOk(listRow()));
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    const v = renderView();
    await screen.findByTestId("history-restore-as-new");

    async function copyWith(result: () => Promise<unknown>): Promise<void> {
      useUi.setState({ toasts: [] });
      v.invalidate.mockClear();
      api.restoreAsNew.mockReset();
      api.restoreAsNew.mockImplementation(result);
      await waitFor(() => expect(screen.getByTestId("history-restore-as-new")).toBeEnabled());
      fireEvent.click(screen.getByTestId("history-restore-as-new"));
      await waitFor(() => expect(useUi.getState().toasts).toHaveLength(1));
    }
    const answer = (status: number, code: string, details: Record<string, unknown> = {}) => () =>
      Promise.resolve(apiError(status, code, details));

    await copyWith(answer(403, "acl.editor_required"));
    expect(toastTexts()).toEqual([copy.restoreAsNewForbidden]);
    await copyWith(answer(403, "quota.exceeded", { quota: "pages", scope: "workspace", limit: 10 }));
    expect(useUi.getState().toasts).toMatchObject([
      { kind: "warn", text: "This workspace has reached its page limit.", key: "limit" },
    ]);
    await copyWith(answer(429, "rate.limited", { retry_after_secs: 7 }));
    expect(useUi.getState().toasts).toMatchObject([
      { kind: "warn", text: "You're doing that too fast. Try again in 7 s.", key: "limit" },
    ]);
    await copyWith(answer(413, "doc.duplicate_too_large"));
    expect(toastTexts()).toEqual(["This page is too large to duplicate (over 200 pages or 200 MB of files)."]);
    await copyWith(answer(503, "doc.duplicate_busy"));
    expect(toastTexts()).toEqual(["Too many copies are being made right now. Try again in a moment."]);
    await copyWith(answer(409, "history.moved"));
    expect(toastTexts()).toEqual([copy.restoreMoved]);
    expect(invalidatedKeys(v.invalidate)).toEqual(
      expect.arrayContaining([
        ["history", "d1", "entries"],
        ["history", "d1", "view", 7],
      ]),
    );
    await copyWith(answer(409, "history.rebuilding"));
    expect(toastTexts()).toEqual([copy.rebuilding]);
    await copyWith(answer(404, "history.not_found"));
    expect(toastTexts()).toEqual([copy.notFound]);
    expect(invalidatedKeys(v.invalidate)).toContainEqual(["history", "d1"]);
    await copyWith(answer(503, "history.busy"));
    expect(toastTexts()).toEqual([copy.busy]);
    await copyWith(answer(409, "history.too_large"));
    expect(toastTexts()).toEqual([copy.restoreVersionTooLarge]);
    for (const code of ["doc.duplicate_conflict", "doc.sort_key_conflict"]) {
      await copyWith(answer(409, code));
      expect(toastTexts()).toEqual([copy.restoreAsNewFailed]);
    }
    await copyWith(answer(404, "doc.not_found"));
    expect(toastTexts()).toEqual([copy.restoreAsNewFailed]);
    for (const result of [
      answer(408, "request.timeout"),
      answer(504, "gateway.timeout"),
      () => Promise.reject(new TypeError("Failed to fetch")),
    ]) {
      await copyWith(result);
      expect(toastTexts()).toEqual([copy.restoreAsNewUnknown]);
      expect(invalidatedKeys(v.invalidate)).toContainEqual(["docs"]);
    }
    await copyWith(answer(500, "internal"));
    expect(toastTexts()).toEqual([copy.restoreAsNewFailed]);
    expect(v.onRestored).not.toHaveBeenCalled();
    expect(screen.getByTestId("location")).toHaveTextContent("/doc/d1");
  });

  it("the confirm text describes the revert", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    expect(confirm).toHaveBeenCalledWith(copy.restoreConfirm);
    expect(copy.restoreConfirm).toBe(
      "Bring the page back to this version? Changes made after it are undone, including edits made meanwhile to parts it removes. The restore shows in History.",
    );
    expect(copy.restoreConfirm).not.toContain("canonical schema");
  });

  it("a restore of a moved entry asks to check the new version", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValue(apiError(409, "history.moved"));
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreMoved]));
    expect(invalidatedKeys(v.invalidate)).toEqual(
      expect.arrayContaining([
        ["history", "d1", "entries"],
        ["history", "d1", "view", 7],
      ]),
    );
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("a restore of a vanished entry says so and refreshes the list", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    api.restoreEntry.mockResolvedValueOnce(apiError(404, "history.not_found"));
    const v = renderView();
    fireEvent.click(await screen.findByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.notFound]));
    expect(invalidatedKeys(v.invalidate)).toContainEqual(["history", "d1"]);
    expect(v.onRestored).not.toHaveBeenCalled();

    useUi.setState({ toasts: [] });
    api.restoreEntry.mockResolvedValueOnce(apiError(500, "history.view_failed"));
    await waitFor(() => expect(screen.getByTestId("history-restore")).toBeEnabled());
    fireEvent.click(screen.getByTestId("history-restore"));
    await waitFor(() => expect(toastTexts()).toEqual([copy.restoreFailed]));
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("a restore without an answer may have completed", async () => {
    vi.spyOn(window, "confirm").mockReturnValue(true);
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    const v = renderView();
    await screen.findByTestId("history-restore");

    async function restoreWith(result: () => Promise<unknown>): Promise<void> {
      useUi.setState({ toasts: [] });
      v.invalidate.mockClear();
      api.restoreEntry.mockReset();
      api.restoreEntry.mockImplementation(result);
      await waitFor(() => expect(screen.getByTestId("history-restore")).toBeEnabled());
      fireEvent.click(screen.getByTestId("history-restore"));
      await waitFor(() => expect(useUi.getState().toasts).toHaveLength(1));
    }

    for (const result of [
      () => Promise.resolve(apiError(504, "restore.timeout")),
      () => Promise.resolve(apiError(408, "request.timeout")),
      () => Promise.reject(new TypeError("Failed to fetch")),
    ]) {
      await restoreWith(result);
      expect(toastTexts()).toEqual([copy.restoreUnknown]);
      expect(invalidatedKeys(v.invalidate)).toEqual(expect.arrayContaining([["doc", "d1"], ["history", "d1"]]));
    }

    await restoreWith(() => Promise.resolve(apiError(500, "internal")));
    expect(toastTexts()).toEqual([copy.restoreFailed]);

    await restoreWith(() => Promise.resolve(apiError(503, "history.busy")));
    expect(toastTexts()).toEqual([copy.busy]);
    expect(api.restoreEntry).toHaveBeenCalledTimes(1);
    expect(v.onRestored).not.toHaveBeenCalled();
  });

  it("hides Restore here without restore rights", async () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView({ canRestore: false });
    await waitFor(() => expect(versionDoc()).toHaveTextContent("one"));
    expect(screen.queryByTestId("history-restore")).toBeNull();
    cleanup();

    api.view.mockResolvedValue(viewOk(doc(p(text("one"))), { can_restore: false }));
    renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("one"));
    expect(screen.queryByTestId("history-restore")).toBeNull();
  });

  it("goes back to the timeline on narrow screens", () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    const v = renderView();
    const back = screen.getByTestId("history-back");
    expect(back).toHaveTextContent(copy.back);
    expect(back.className).toContain("md:hidden");
    fireEvent.click(back);
    expect(v.onBack).toHaveBeenCalledTimes(1);
  });

  it("focuses Back when asked to, on mount and for each entry", () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    const v = renderView({ focusBack: true });
    expect(document.activeElement).toBe(screen.getByTestId("history-back"));
    (document.activeElement as HTMLElement).blur();
    v.rerender({ entry: entry(7, { ended_at: T2 }) });
    expect(document.activeElement).not.toBe(screen.getByTestId("history-back"));
    v.rerender({ entry: entry(8) });
    expect(document.activeElement).toBe(screen.getByTestId("history-back"));
  });

  it("shows the page's current title above the version", async () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView();
    const title = await screen.findByTestId("history-version-title");
    await waitFor(() => expect(title).toHaveTextContent("Plan"));
    expect(title.tagName).toBe("H1");
    expect(title.className).toContain("text-[30px]");
    await waitFor(() => expect(versionDoc()).toHaveTextContent("one"));
    expect(title.compareDocumentPosition(versionDoc()) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(screen.getByTestId("history-version-scroll").contains(title)).toBe(true);
    expect(docs.get).toHaveBeenCalledWith("d1");
    cleanup();

    docs.get.mockResolvedValue({ ok: { id: "d1", title: "" } });
    renderView();
    await waitFor(() => expect(screen.getByTestId("history-version-title")).toHaveTextContent(copy.untitled));
  });

  it("shows the entry's line in the bar", () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    renderView({ entry: entry(7, { words_added: 12, words_removed: 4 }) });
    expect(screen.getByTestId("history-version-bar")).toHaveTextContent("Ann");
    expect(screen.getByTestId("history-version-bar")).toHaveTextContent("+12 / −4");
  });

  it("parses a version once while the pane re-renders", async () => {
    api.view.mockResolvedValue(viewOk(doc(p(text("one")))));
    const v = renderView();
    await waitFor(() => expect(versionDoc()).toHaveTextContent("one"));
    expect(vi.mocked(viewerContent)).toHaveBeenCalledTimes(1);
    for (let i = 0; i < 3; i++) v.rerender({ entry: entry(7) });
    await act(async () => {
      await Promise.resolve();
    });
    expect(vi.mocked(viewerContent)).toHaveBeenCalledTimes(1);
    expect(mounts.count).toBe(1);
  });
});
