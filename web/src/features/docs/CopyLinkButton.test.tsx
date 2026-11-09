import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { shortLinksApi } from "../../lib/shortLinks.api";
import { useUi } from "../../stores/ui";

import { CopyLinkButton } from "./CopyLinkButton";

const LINK = { code: "k3x9Fa7", url: "https://knot.example/k3x9Fa7" };
const writeText = vi.fn<(s: string) => Promise<void>>();
const notFound = {
  error: { code: "short_link.none", message: "", details: {}, status: 404 },
};

beforeEach(() => {
  writeText.mockReset().mockResolvedValue(undefined);
  Object.defineProperty(navigator, "clipboard", { value: { writeText }, configurable: true });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("CopyLinkButton", () => {
  it("copies the short link of a live page", async () => {
    const getOrCreate = vi.spyOn(shortLinksApi, "getOrCreate").mockResolvedValue({ ok: LINK });
    const notify = vi.fn();
    useUi.setState({ notify });
    render(<CopyLinkButton docId="d1" archived={false} />);
    fireEvent.click(screen.getByTestId("copy-link"));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(LINK.url));
    expect(getOrCreate).toHaveBeenCalledWith("d1");
    expect(notify).toHaveBeenCalledWith("info", "Copied!");
  });

  it("copies the existing short link of an archived page", async () => {
    const get = vi.spyOn(shortLinksApi, "get").mockResolvedValue({ ok: LINK });
    const getOrCreate = vi.spyOn(shortLinksApi, "getOrCreate");
    render(<CopyLinkButton docId="d1" archived />);
    fireEvent.click(screen.getByTestId("copy-link"));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(LINK.url));
    expect(get).toHaveBeenCalledWith("d1");
    expect(getOrCreate).not.toHaveBeenCalled();
  });

  it("copies the long link of an archived page without one", async () => {
    vi.spyOn(shortLinksApi, "get").mockResolvedValue(notFound);
    render(<CopyLinkButton docId="d1" archived />);
    fireEvent.click(screen.getByTestId("copy-link"));
    await waitFor(() => expect(writeText).toHaveBeenCalledWith(`${window.location.origin}/doc/d1`));
  });

  it("falls back to showing the link when the clipboard refuses", async () => {
    vi.spyOn(shortLinksApi, "getOrCreate").mockResolvedValue({ ok: LINK });
    writeText.mockRejectedValue(new Error("denied"));
    render(<CopyLinkButton docId="d1" archived={false} />);
    fireEvent.click(screen.getByTestId("copy-link"));
    const field = await screen.findByTestId("copy-link-fallback");
    expect((field as HTMLInputElement).value).toBe(LINK.url);
  });
});
