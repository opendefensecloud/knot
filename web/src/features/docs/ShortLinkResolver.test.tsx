import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes, useLocation } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { shortLinksApi } from "../../lib/shortLinks.api";

import { ShortLinkResolver } from "./ShortLinkResolver";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

function Where() {
  const loc = useLocation();
  return <p data-testid="where">{loc.pathname}</p>;
}

function renderAt(path: string) {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route index element={<Where />} />
        <Route path="doc/:id" element={<Where />} />
        <Route path=":code" element={<ShortLinkResolver />} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("ShortLinkResolver", () => {
  it("replaces the location with the page", async () => {
    const resolve = vi.spyOn(shortLinksApi, "resolve").mockResolvedValue({ ok: { doc_id: "d1" } });
    renderAt("/k3x9Fa7");
    expect((await screen.findByTestId("where")).textContent).toBe("/doc/d1");
    expect(resolve).toHaveBeenCalledWith("k3x9Fa7");
  });

  it("resolves with a trailing slash", async () => {
    vi.spyOn(shortLinksApi, "resolve").mockResolvedValue({ ok: { doc_id: "d1" } });
    renderAt("/k3x9Fa7/");
    expect((await screen.findByTestId("where")).textContent).toBe("/doc/d1");
  });

  it("shows the not-found text on 404", async () => {
    vi.spyOn(shortLinksApi, "resolve").mockResolvedValue({
      error: { code: "short_link.not_found", message: "", details: {}, status: 404 },
    });
    renderAt("/k3x9Fa7");
    expect(
      await screen.findByText("This link doesn't exist or you don't have access to it."),
    ).toBeTruthy();
  });

  it("ignores a path that is not a code", async () => {
    const resolve = vi.spyOn(shortLinksApi, "resolve");
    for (const path of ["/abcdefg", "/favicon.ico"]) {
      renderAt(path);
      expect((await screen.findByTestId("where")).textContent).toBe("/");
      cleanup();
    }
    expect(resolve).not.toHaveBeenCalled();
  });
});
