import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import type { DocContributors } from "../../lib/validators";

import { DocByline } from "./DocByline";

const contributors = vi.fn<(id: string) => Promise<unknown>>();

vi.mock("./docs.api", () => ({
  docsApi: { contributors: (id: string) => contributors(id) },
}));

const CREATED = "2026-03-03T10:00:00Z";

function body(over: Partial<DocContributors> = {}): DocContributors {
  return {
    created_by: { id: "u-alice", display_name: "Alice" },
    created_at: CREATED,
    // Same instant as creation: tracking covered the doc's whole life.
    contributors_since: CREATED,
    contributors: [],
    ...over,
  };
}

function person(id: string, name: string, lastEdited = "2026-10-03T11:00:00Z") {
  return {
    user_id: id,
    display_name: name,
    first_edited_at: "2026-10-01T09:00:00Z",
    last_edited_at: lastEdited,
  };
}

const DATE_OPTS: Intl.DateTimeFormatOptions = { day: "numeric", month: "short", year: "numeric" };

function renderByline() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <DocByline docId="d1" />
      <p data-testid="elsewhere">outside</p>
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  contributors.mockReset();
});
afterEach(() => cleanup());

describe("DocByline", () => {
  it("names the creator and the creation date", async () => {
    contributors.mockResolvedValue({ ok: body() });
    renderByline();

    const creator = await screen.findByTestId("doc-byline-creator");
    expect(creator).toHaveTextContent(
      `Created by Alice · ${new Date(CREATED).toLocaleDateString(undefined, DATE_OPTS)}`,
    );
    expect(contributors).toHaveBeenCalledWith("d1");
  });

  it("shows no contributors button when nobody has edited the content", async () => {
    contributors.mockResolvedValue({ ok: body() });
    renderByline();

    await screen.findByTestId("doc-byline-creator");
    expect(screen.queryByTestId("doc-contributors-button")).toBeNull();
    expect(screen.getByTestId("doc-byline")).not.toHaveTextContent("contributor");
  });

  it("says '1 contributor' for one", async () => {
    contributors.mockResolvedValue({ ok: body({ contributors: [person("u-bob", "Bob")] }) });
    renderByline();

    const button = await screen.findByTestId("doc-contributors-button");
    expect(button).toHaveTextContent(/1 contributor$/);
    expect(button.querySelectorAll("span[title]")).toHaveLength(1);
  });

  it("caps the avatar stack at three but counts everyone", async () => {
    contributors.mockResolvedValue({
      ok: body({
        contributors: [
          person("u-1", "Bob"),
          person("u-2", "Carol"),
          person("u-3", "Dan"),
          person("u-4", "Erin"),
          person("u-5", "Frank"),
        ],
      }),
    });
    renderByline();

    const button = await screen.findByTestId("doc-contributors-button");
    expect(button).toHaveTextContent(/5 contributors$/);
    const avatars = [...button.querySelectorAll("span[title]")];
    expect(avatars).toHaveLength(3);
    // The stack shows the most recent editors, which the server lists first.
    expect(avatars.map((a) => a.getAttribute("title"))).toEqual(["Bob", "Carol", "Dan"]);
  });

  it("opens the list on click, in server order, and refetches it", async () => {
    contributors.mockResolvedValue({
      ok: body({
        contributors: [
          person("u-zed", "Zed", "2026-10-03T11:58:00Z"),
          person("u-amy", "Amy", "2026-10-03T09:00:00Z"),
        ],
      }),
    });
    renderByline();

    const button = await screen.findByTestId("doc-contributors-button");
    expect(button).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByTestId("doc-contributors-popover")).toBeNull();
    expect(contributors).toHaveBeenCalledTimes(1);

    fireEvent.click(button);

    const popover = screen.getByTestId("doc-contributors-popover");
    expect(button).toHaveAttribute("aria-expanded", "true");
    expect(button).toHaveAttribute("aria-controls", popover.id);
    // Server order is kept even though it is not alphabetical.
    const rows = within(popover).getAllByTestId(/^doc-contributor-/);
    expect(rows.map((r) => r.dataset.testid)).toEqual([
      "doc-contributor-u-zed",
      "doc-contributor-u-amy",
    ]);
    // Opening asks for a fresh list.
    await waitFor(() => expect(contributors).toHaveBeenCalledTimes(2));
  });

  it("shows when each contributor last edited, with the exact time on hover", async () => {
    const last = new Date(Date.now() - 2 * 3_600_000).toISOString();
    contributors.mockResolvedValue({ ok: body({ contributors: [person("u-bob", "Bob", last)] }) });
    renderByline();

    fireEvent.click(await screen.findByTestId("doc-contributors-button"));

    const row = screen.getByTestId("doc-contributor-u-bob");
    expect(row).toHaveTextContent("Bob");
    const when = within(row).getByText("edited 2h ago");
    expect(when).toHaveAttribute("title", new Date(last).toLocaleString());
  });

  it("closes on Escape and hands focus back to the button", async () => {
    contributors.mockResolvedValue({ ok: body({ contributors: [person("u-bob", "Bob")] }) });
    renderByline();

    const button = await screen.findByTestId("doc-contributors-button");
    fireEvent.click(button);
    expect(screen.getByTestId("doc-contributors-popover")).toBeInTheDocument();

    fireEvent.keyDown(document, { key: "Escape" });

    expect(screen.queryByTestId("doc-contributors-popover")).toBeNull();
    expect(button).toHaveAttribute("aria-expanded", "false");
    expect(button).toHaveFocus();
  });

  it("closes on a click outside, but not on a click inside", async () => {
    contributors.mockResolvedValue({ ok: body({ contributors: [person("u-bob", "Bob")] }) });
    renderByline();

    fireEvent.click(await screen.findByTestId("doc-contributors-button"));
    const popover = screen.getByTestId("doc-contributors-popover");

    fireEvent.mouseDown(within(popover).getByTestId("doc-contributor-u-bob"));
    expect(screen.getByTestId("doc-contributors-popover")).toBeInTheDocument();

    fireEvent.mouseDown(screen.getByTestId("elsewhere"));
    expect(screen.queryByTestId("doc-contributors-popover")).toBeNull();
  });

  it("toggles closed when the button is clicked again", async () => {
    contributors.mockResolvedValue({ ok: body({ contributors: [person("u-bob", "Bob")] }) });
    renderByline();

    const button = await screen.findByTestId("doc-contributors-button");
    fireEvent.click(button);
    // A real click is mousedown then click; the button must not count as
    // "outside" or the two would cancel out and the popover would reopen.
    fireEvent.mouseDown(button);
    fireEvent.click(button);
    expect(screen.queryByTestId("doc-contributors-popover")).toBeNull();
  });

  it("footnotes the list when tracking started after the doc was created", async () => {
    const since = "2026-10-03T12:00:00Z";
    contributors.mockResolvedValue({
      ok: body({ contributors_since: since, contributors: [person("u-bob", "Bob")] }),
    });
    renderByline();

    fireEvent.click(await screen.findByTestId("doc-contributors-button"));

    expect(screen.getByTestId("doc-contributors-popover")).toHaveTextContent(
      `Edits before ${new Date(since).toLocaleDateString(undefined, DATE_OPTS)} aren't listed.`,
    );
  });

  it("has no footnote when tracking began with the doc (within a minute)", async () => {
    contributors.mockResolvedValue({
      ok: body({
        // Row default vs. created_at can differ by a few seconds for a new doc.
        contributors_since: "2026-03-03T10:00:30Z",
        contributors: [person("u-bob", "Bob")],
      }),
    });
    renderByline();

    fireEvent.click(await screen.findByTestId("doc-contributors-button"));

    expect(screen.getByTestId("doc-contributors-popover")).not.toHaveTextContent("Edits before");
  });

  it("reserves its line while loading", () => {
    contributors.mockReturnValue(new Promise(() => {}));
    renderByline();

    const line = screen.getByTestId("doc-byline");
    expect(line).toBeEmptyDOMElement();
    expect(line.className).toContain("min-h-5");
  });

  it("renders nothing when the request fails", async () => {
    contributors.mockResolvedValue({
      error: { code: "auth.forbidden", message: "no", details: {}, status: 403 },
    });
    renderByline();

    await waitFor(() => expect(screen.queryByTestId("doc-byline")).toBeNull());
  });
});
