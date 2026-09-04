import { render, screen, cleanup, fireEvent, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { ImportMarkdownButton } from "./ImportMarkdownButton";

const importMarkdown = vi.fn<(...a: unknown[]) => unknown>();
const exportMarkdown = vi.fn<(...a: unknown[]) => unknown>();

vi.mock("./docs.api", () => ({
  docsApi: { importMarkdown: (...a: unknown[]) => importMarkdown(...a) },
}));
vi.mock("../../lib/history.api", () => ({
  historyApi: { exportMarkdown: (...a: unknown[]) => exportMarkdown(...a) },
}));

function pick(name = "notes.md", body = "# Imported\n") {
  const file = new File([body], name, { type: "text/markdown" });
  fireEvent.change(screen.getByTestId("doc-import-md-input"), { target: { files: [file] } });
  return file;
}

beforeEach(() => {
  importMarkdown.mockReset().mockResolvedValue({ ok: undefined });
  exportMarkdown.mockReset().mockResolvedValue({ ok: "" });
});
afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ImportMarkdownButton", () => {
  it("imports without prompting when the page is empty", async () => {
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() =>
      expect(importMarkdown).toHaveBeenCalledWith("d1", "# Imported\n", "replace"),
    );
    expect(confirm).not.toHaveBeenCalled();
  });

  it("prompts before replacing a page that has content", async () => {
    exportMarkdown.mockResolvedValue({ ok: "# Existing\n" });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(importMarkdown).toHaveBeenCalled());
    expect(confirm).toHaveBeenCalledOnce();
    expect(confirm.mock.calls[0]?.[0]).toContain("Notes");
    expect(confirm.mock.calls[0]?.[0]).toContain("notes.md");
  });

  it("imports nothing when the prompt is declined", async () => {
    exportMarkdown.mockResolvedValue({ ok: "# Existing\n" });
    vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(exportMarkdown).toHaveBeenCalled());
    expect(importMarkdown).not.toHaveBeenCalled();
  });

  it("prompts when the page's current content cannot be read", async () => {
    exportMarkdown.mockResolvedValue({
      error: { code: "internal", message: "boom", details: {}, status: 500 },
    });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    pick();
    await waitFor(() => expect(confirm).toHaveBeenCalled());
    expect(importMarkdown).not.toHaveBeenCalled();
  });

  it("rejects an oversized file before any network call", async () => {
    render(<ImportMarkdownButton docId="d1" docTitle="Notes" />);
    const big = new File(["x".repeat(1024 * 1024 + 1)], "big.md", { type: "text/markdown" });
    fireEvent.change(screen.getByTestId("doc-import-md-input"), { target: { files: [big] } });
    await waitFor(() => expect(screen.getByTestId("doc-import-md")).toBeInTheDocument());
    expect(exportMarkdown).not.toHaveBeenCalled();
    expect(importMarkdown).not.toHaveBeenCalled();
  });
});
