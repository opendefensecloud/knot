import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { authApi } from "../../auth/session.api";
import { useUi } from "../../stores/ui";
import SettingsPage from "./SettingsPage";
import { workspaceApi } from "./workspace.api";

vi.mock("../../auth/SessionContext", () => ({
  useSession: () => ({
    data: { ok: { user_id: "u1", email: "anna@example.org", display_name: "Anna", role: "owner" } },
  }),
}));
const saved = vi.hoisted(() => ({ saveBlob: vi.fn() }));
vi.mock("../export/exportFile", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../export/exportFile")>()),
  saveBlob: saved.saveBlob,
}));

function envelope(status: number, code: string, details: Record<string, unknown> = {}): Response {
  return new Response(JSON.stringify({ error: { code, message: code, details } }), {
    status,
    headers: { "Content-Type": "application/json" },
  });
}

const TOO_FAST_TEXT = "You're doing that too fast. Try again in 8 s.";

function renderPage() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  return render(
    <QueryClientProvider client={qc}>
      <MemoryRouter>
        <SettingsPage />
      </MemoryRouter>
    </QueryClientProvider>,
  );
}

function pickArchive() {
  const archive = new File(["PK"], "backup.zip", { type: "application/zip" });
  const input = screen.getByTestId("workspace-import").querySelector("input")!;
  fireEvent.change(input, { target: { files: [archive] } });
}

beforeEach(() => {
  useUi.setState({ toasts: [] });
  saved.saveBlob.mockReset();
  vi.spyOn(workspaceApi, "get").mockResolvedValue({
    ok: { id: "w1", name: "Acme", slug: "acme", archive_retention_days: 30 },
  } as Awaited<ReturnType<typeof workspaceApi.get>>);
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("SettingsPage workspace export", () => {
  it("downloads the workspace zip through fetch and saves it", async () => {
    const fetchSpy = vi.spyOn(globalThis, "fetch").mockResolvedValue(
      new Response("zip", { status: 200, headers: { "Content-Type": "application/zip" } }),
    );
    renderPage();
    fireEvent.click(screen.getByTestId("workspace-export"));
    await waitFor(() => expect(saved.saveBlob).toHaveBeenCalledTimes(1));
    expect(fetchSpy.mock.calls[0]?.[0]).toBe("/api/workspace/export");
    expect(saved.saveBlob.mock.calls[0]?.[1]).toBe("knot-workspace-export.zip");
  });

  it("shows the rate-limit text when the export is refused", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(envelope(429, "rate.limited", { retry_after_secs: 8 }));
    renderPage();
    fireEvent.click(screen.getByTestId("workspace-export"));
    await waitFor(() =>
      expect(useUi.getState().toasts).toMatchObject([{ kind: "warn", text: TOO_FAST_TEXT, key: "limit" }]),
    );
    expect(saved.saveBlob).not.toHaveBeenCalled();
  });
});

describe("SettingsPage workspace import", () => {
  it.each<[string, Response, { kind: string; text: string }]>([
    ["a 429", envelope(429, "rate.limited", { retry_after_secs: 8 }), { kind: "warn", text: TOO_FAST_TEXT }],
    [
      "a page quota",
      envelope(403, "quota.exceeded", { quota: "pages", scope: "workspace", limit: 100 }),
      { kind: "warn", text: "This workspace has reached its page limit." },
    ],
    [
      "an archive over the import caps",
      envelope(413, "import.too_large", { pages: 20000, bytes: 1, max_pages: 10000, max_bytes: 536870912 }),
      { kind: "error", text: "This archive is too large to import (over 10,000 pages or 512 MiB of files)." },
    ],
    [
      "an archive over other import caps",
      envelope(413, "import.too_large", { pages: 1, bytes: 2147483648, max_pages: 500, max_bytes: 1610612736 }),
      { kind: "error", text: "This archive is too large to import (over 500 pages or 1,536 MiB of files)." },
    ],
    [
      "an archive over a reference cap",
      envelope(413, "import.too_large", {
        pages: 1,
        bytes: 1,
        references: 60000,
        max_pages: 10000,
        max_bytes: 536870912,
        max_references: 50000,
      }),
      {
        kind: "error",
        text: "This archive is too large to import (over 10,000 pages, 512 MiB of files or 50,000 file references).",
      },
    ],
    [
      "an archive over the page-text cap",
      envelope(413, "import.too_large", {
        pages: 1,
        bytes: 1,
        markdown_bytes: 300000000,
        references: 1,
        max_pages: 10000,
        max_bytes: 536870912,
        max_markdown_bytes: 268435456,
        max_references: 20000,
      }),
      {
        kind: "error",
        text: "This archive is too large to import (over 10,000 pages, 512 MiB of files, 256 MiB of page text or 20,000 file references).",
      },
    ],
    [
      "an archive over the import caps, without details",
      envelope(413, "import.too_large"),
      { kind: "error", text: "This archive is too large to import (over 10,000 pages or 512 MiB of files)." },
    ],
    ["another failure", new Response("", { status: 500 }), { kind: "error", text: "Import failed (500)." }],
  ])("says why %s refused it", async (_what, reply, toast) => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(reply);
    renderPage();
    pickArchive();
    await waitFor(() => expect(useUi.getState().toasts).toMatchObject([toast]));
  });
});

describe("SettingsPage change password", () => {
  it("shows the rate-limit text when password changes are limited", async () => {
    vi.spyOn(authApi, "changePassword").mockResolvedValue({
      error: { code: "rate.limited", message: "too many requests", details: { retry_after_secs: 8 }, status: 429 },
    });
    renderPage();
    fireEvent.change(screen.getByTestId("pw-current"), { target: { value: "old-password" } });
    fireEvent.change(screen.getByTestId("pw-new"), { target: { value: "new-password" } });
    fireEvent.click(screen.getByTestId("pw-submit"));
    expect(await screen.findByTestId("pw-error")).toHaveTextContent(TOO_FAST_TEXT);
  });
});
