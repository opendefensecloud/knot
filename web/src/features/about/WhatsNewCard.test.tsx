import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter, useLocation } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { WhatsNew } from "../../lib/validators";
import { useUi } from "../../stores/ui";

import { WhatsNewCard } from "./WhatsNewCard";

const realWidth = window.innerWidth;

function setViewportWidth(w: number) {
  Object.defineProperty(window, "innerWidth", { value: w, configurable: true, writable: true });
}

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
  setViewportWidth(realWidth);
  useUi.setState({ sidebarOpen: true });
});

function json(body: unknown, status = 200) {
  return Promise.resolve(
    new Response(status === 204 ? null : JSON.stringify(body), {
      status,
      headers: { "Content-Type": "application/json" },
    }),
  );
}

function whatsNew(over: Partial<WhatsNew> = {}): WhatsNew {
  return {
    version: "0.8.1",
    commit: "52fa7a1c0ffee",
    released_on: "2026-10-21",
    dev: false,
    last_seen_version: "0.7.0",
    unseen: ["0.8.1", "0.8.0"],
    ...over,
  };
}

/** Serves GET /api/whats-new; POST …/seen answers with `seen` (204 by default). */
function server(body: WhatsNew, seen: () => Promise<Response> = () => json(null, 204)) {
  let current = body;
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/api/whats-new" && init?.method === "GET") return json(current);
    if (url === "/api/whats-new/seen" && init?.method === "POST") {
      current = { ...current, unseen: [], last_seen_version: current.version };
      return seen();
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

function Where() {
  return <p data-testid="where">{useLocation().pathname}</p>;
}

function renderCard(path = "/doc/d1") {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  render(
    <QueryClientProvider client={qc}>
      <MemoryRouter initialEntries={[path]}>
        <WhatsNewCard />
        <Where />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return qc;
}

/** The card renders nothing until whats-new has loaded, so "not shown" is
 *  only worth asserting once it has. */
async function loaded(qc: QueryClient) {
  await waitFor(() => expect(qc.getQueryState(["whats-new"])?.status).toBe("success"));
}

function seenPosts(fetchMock: ReturnType<typeof server>) {
  return fetchMock.mock.calls.filter(
    ([url, init]) => url === "/api/whats-new/seen" && init?.method === "POST",
  );
}

describe("WhatsNewCard", () => {
  it("names the new version and how many releases were missed", async () => {
    vi.stubGlobal("fetch", server(whatsNew()));
    renderCard();

    const card = await screen.findByRole("region", { name: "knot was updated to 0.8.1" });
    expect(card).toHaveAttribute("data-testid", "whats-new-card");
    expect(card).toHaveTextContent("2 releases since your last visit");
    expect(screen.getByTestId("whats-new-open")).toHaveTextContent("See what's new");
    expect(screen.getByTestId("whats-new-dismiss")).toHaveTextContent("Dismiss");
  });

  it("says '1 new release' when only one was missed", async () => {
    vi.stubGlobal("fetch", server(whatsNew({ unseen: ["0.8.1"] })));
    renderCard();

    expect(await screen.findByTestId("whats-new-card")).toHaveTextContent(
      "1 new release since your last visit",
    );
  });

  it.each([
    ["on a development build", whatsNew({ version: "dev", dev: true }), "/doc/d1"],
    ["when the user is caught up", whatsNew({ unseen: [] }), "/doc/d1"],
    ["on the About page, which shows the notes itself", whatsNew(), "/about"],
  ])("stays hidden %s", async (_, body, path) => {
    vi.stubGlobal("fetch", server(body));
    const qc = renderCard(path);

    await loaded(qc);
    expect(screen.queryByTestId("whats-new-card")).toBeNull();
  });

  it("waits on a phone until the menu drawer is closed, rather than covering it", async () => {
    setViewportWidth(375);
    useUi.setState({ sidebarOpen: true });
    vi.stubGlobal("fetch", server(whatsNew()));
    const qc = renderCard();

    await loaded(qc);
    expect(screen.queryByTestId("whats-new-card")).toBeNull();

    act(() => useUi.setState({ sidebarOpen: false }));
    expect(screen.getByTestId("whats-new-card")).toBeInTheDocument();
  });

  it("goes away as soon as it is dismissed, and records the version as seen", async () => {
    // The server has not answered yet when the card has to be gone.
    let answer!: (r: Response) => void;
    const fetchMock = server(whatsNew(), () => new Promise<Response>((r) => (answer = r)));
    vi.stubGlobal("fetch", fetchMock);
    renderCard();

    fireEvent.click(await screen.findByTestId("whats-new-dismiss"));

    await waitFor(() => expect(screen.queryByTestId("whats-new-card")).toBeNull());
    expect(seenPosts(fetchMock)).toHaveLength(1);
    answer(new Response(null, { status: 204 }));
    // It stays gone once the server's answer is refetched.
    await waitFor(() =>
      expect(fetchMock.mock.calls.filter(([url]) => url === "/api/whats-new")).toHaveLength(2),
    );
    expect(screen.queryByTestId("whats-new-card")).toBeNull();
  });

  it("dismisses from its close button too", async () => {
    const fetchMock = server(whatsNew());
    vi.stubGlobal("fetch", fetchMock);
    renderCard();

    const card = await screen.findByTestId("whats-new-card");
    fireEvent.click(screen.getByRole("button", { name: "Close" }));

    await waitFor(() => expect(card).not.toBeInTheDocument());
    expect(seenPosts(fetchMock)).toHaveLength(1);
  });

  it("opens the About page and leaves marking seen to it", async () => {
    const fetchMock = server(whatsNew());
    vi.stubGlobal("fetch", fetchMock);
    renderCard();

    fireEvent.click(await screen.findByTestId("whats-new-open"));

    expect(screen.getByTestId("where")).toHaveTextContent("/about");
    expect(screen.queryByTestId("whats-new-card")).toBeNull();
    // The About page marks the releases seen once it has captured which
    // ones were new; doing it here would wipe its "new" markers.
    expect(seenPosts(fetchMock)).toHaveLength(0);
  });
});
