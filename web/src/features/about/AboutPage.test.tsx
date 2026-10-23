import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen, waitFor, within } from "@testing-library/react";
import { StrictMode } from "react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { ReleaseNotes, WhatsNew } from "../../lib/validators";

import { formatReleaseDate } from "./about";
import AboutPage from "./AboutPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
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
    commit: "52fa7a1c0ffee0123456789abcdef0123456789a",
    released_on: "2026-10-21",
    dev: false,
    last_seen_version: "0.7.0",
    unseen: [],
    ...over,
  };
}

const NOTES: ReleaseNotes = {
  releases: [
    {
      version: "0.8.1",
      date: "2026-10-21",
      sections: [
        { title: "Upgrading", html: "<ul><li>Drops the <code>emSign</code> root.</li></ul>" },
        { title: "Security", html: "<ul><li>eMudhra CA key compromise.</li></ul>" },
      ],
    },
    {
      version: "0.8.0",
      date: "2026-10-13",
      sections: [
        // Text straight under the release heading, before any "###".
        { title: "", html: "<p>The archive release.</p>" },
        { title: "Added", html: "<ul><li>Page byline.</li></ul>" },
      ],
    },
    {
      version: "0.7.0",
      date: "2026-10-02",
      sections: [{ title: "Fixed", html: "<ul><li>Older fix.</li></ul>" }],
    },
  ],
};

function server(body: WhatsNew, notes: ReleaseNotes = NOTES) {
  let current = body;
  return vi.fn((url: string, init?: RequestInit) => {
    if (url === "/api/whats-new" && init?.method === "GET") return json(current);
    if (url === "/api/release-notes" && init?.method === "GET") return json(notes);
    if (url === "/api/whats-new/seen" && init?.method === "POST") {
      current = { ...current, unseen: [], last_seen_version: current.version };
      return json(null, 204);
    }
    return json({ error: { code: "not_found", message: url, details: {} } }, 404);
  });
}

/** With the app's staleTime (lib/queryClient.ts): it decides whether what
 *  is cached is fresh enough for the page to go by. */
function client() {
  return new QueryClient({ defaultOptions: { queries: { retry: false, staleTime: 30_000 } } });
}

/** `cached` is what the sidebar and the upgrade card already loaded, `ago`
 *  ms back, as when the user arrives from the card's "See what's new". */
function renderPage(cached?: WhatsNew, ago = 0, qc = client()) {
  if (cached) qc.setQueryData(["whats-new"], { ok: cached }, { updatedAt: Date.now() - ago });
  // StrictMode as in main.tsx: its doubled mount effects must not mark twice.
  render(
    <StrictMode>
      <QueryClientProvider client={qc}>
        <MemoryRouter initialEntries={["/about"]}>
          <AboutPage />
        </MemoryRouter>
      </QueryClientProvider>
    </StrictMode>,
  );
  return qc;
}

function seenPosts(fetchMock: ReturnType<typeof server>) {
  return fetchMock.mock.calls.filter(
    ([url, init]) => url === "/api/whats-new/seen" && init?.method === "POST",
  );
}

function release(version: string) {
  return screen.getByTestId<HTMLDetailsElement>(`release-${version}`);
}

describe("AboutPage", () => {
  it("shows the running version, its short commit and its release date", async () => {
    vi.stubGlobal("fetch", server(whatsNew()));
    renderPage();

    expect(await screen.findByTestId("about-version")).toHaveTextContent("0.8.1");
    expect(screen.getByTestId("about-commit")).toHaveTextContent(/^52fa7a1$/);
    expect(screen.getByTestId("about-released")).toHaveTextContent(formatReleaseDate("2026-10-21"));
  });

  it("calls a dev build a development build, and still shows its commit", async () => {
    vi.stubGlobal("fetch", server(whatsNew({ version: "dev", commit: "52fa7a1", released_on: null, dev: true })));
    renderPage();

    expect(await screen.findByTestId("about-released")).toHaveTextContent("Development build");
    expect(screen.getByTestId("about-commit")).toHaveTextContent("52fa7a1");
  });

  it("lists every release newest first, with its sections as written", async () => {
    vi.stubGlobal("fetch", server(whatsNew()));
    renderPage();

    await screen.findByTestId("release-0.8.1");
    expect(screen.getAllByTestId(/^release-\d/).map((el) => el.dataset.testid)).toEqual([
      "release-0.8.1",
      "release-0.8.0",
      "release-0.7.0",
    ]);

    const newest = release("0.8.1");
    expect(within(newest).getByRole("heading", { name: "0.8.1" })).toBeInTheDocument();
    expect(within(newest).getByText(formatReleaseDate("2026-10-21"))).toBeInTheDocument();
    expect(within(newest).getAllByRole("heading", { level: 4 }).map((h) => h.textContent)).toEqual([
      "Upgrading",
      "Security",
    ]);
    expect(within(newest).getByText("emSign").tagName).toBe("CODE");

    // An untitled section renders its text without an empty heading.
    const older = release("0.8.0");
    expect(within(older).getByText("The archive release.")).toBeInTheDocument();
    expect(within(older).getAllByRole("heading", { level: 4 }).map((h) => h.textContent)).toEqual([
      "Added",
    ]);
  });

  it("opens only the newest release, and marks nothing seen, when the user is caught up", async () => {
    const fetchMock = server(whatsNew());
    vi.stubGlobal("fetch", fetchMock);
    renderPage();

    await screen.findByTestId("release-0.8.1");
    await screen.findByTestId("about-version");
    expect(release("0.8.1").open).toBe(true);
    expect(release("0.8.0").open).toBe(false);
    expect(release("0.7.0").open).toBe(false);
    expect(screen.queryByText("New since your last visit")).toBeNull();
    expect(seenPosts(fetchMock)).toHaveLength(0);
  });

  it.each([
    ["loading what's new itself", false],
    ["arriving from the upgrade card", true],
  ])("opens and flags the releases new since the last visit, and marks them seen once, %s", async (_, fromCard) => {
    const body = whatsNew({ unseen: ["0.8.1", "0.8.0"] });
    const fetchMock = server(body);
    vi.stubGlobal("fetch", fetchMock);
    const qc = renderPage(fromCard ? body : undefined);

    await waitFor(() => expect(seenPosts(fetchMock)).toHaveLength(1));
    // Refetched after marking: the server now has nothing unseen…
    await waitFor(() => {
      const calls = fetchMock.mock.calls.map(([url, init]) => `${init?.method} ${url}`);
      expect(calls.slice(calls.indexOf("POST /api/whats-new/seen"))).toContain("GET /api/whats-new");
    });
    await waitFor(() => expect(qc.isFetching()).toBe(0));
    // …and the markers stay for the rest of this visit regardless.
    await screen.findByTestId("release-0.8.1");
    expect(release("0.8.1").open).toBe(true);
    expect(release("0.8.0").open).toBe(true);
    expect(release("0.7.0").open).toBe(false);
    expect(within(release("0.8.1")).getByText("New since your last visit")).toBeInTheDocument();
    expect(within(release("0.8.0")).getByText("New since your last visit")).toBeInTheDocument();
    expect(within(release("0.7.0")).queryByText("New since your last visit")).toBeNull();
    expect(seenPosts(fetchMock)).toHaveLength(1);
  });

  it("goes by the server, not by what a tab left open across an upgrade has cached", async () => {
    // A visit before the upgrade leaves both answers cached…
    const before = whatsNew({ version: "0.8.0", released_on: "2026-10-13", last_seen_version: "0.8.0" });
    vi.stubGlobal("fetch", server(before, { releases: NOTES.releases.slice(1) }));
    const qc = renderPage();
    await screen.findByTestId("release-0.8.0");
    cleanup();

    // …then the server is upgraded, and the user comes back a minute later.
    const fetchMock = server(whatsNew({ last_seen_version: "0.8.0", unseen: ["0.8.1"] }));
    vi.stubGlobal("fetch", fetchMock);
    renderPage(before, 60_000, qc);

    await waitFor(() => expect(seenPosts(fetchMock)).toHaveLength(1));
    await waitFor(() => expect(qc.isFetching()).toBe(0));
    expect(screen.getByTestId("about-version")).toHaveTextContent("0.8.1");
    expect(release("0.8.1").open).toBe(true);
    expect(within(release("0.8.1")).getByText("New since your last visit")).toBeInTheDocument();
  });

  it("strips anything unsafe from the notes before rendering them", async () => {
    const notes: ReleaseNotes = {
      releases: [
        {
          version: "0.8.1",
          date: "2026-10-21",
          sections: [
            { title: "Fixed", html: '<p>ok</p><img src="x" onerror="alert(1)"><script>alert(2)</script>' },
          ],
        },
      ],
    };
    vi.stubGlobal("fetch", server(whatsNew(), notes));
    renderPage();

    const newest = await screen.findByTestId("release-0.8.1");
    expect(within(newest).getByText("ok")).toBeInTheDocument();
    expect(newest.querySelector("img, script, [onerror]")).toBeNull();
  });
});
