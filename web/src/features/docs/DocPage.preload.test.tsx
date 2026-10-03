import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import { afterEach, describe, expect, it, vi } from "vitest";

import { SessionProvider } from "../../auth/SessionContext";

// The factory runs the first time anything imports the editor module, so the
// spy records the moment the (lazily split) editor bundle is requested.
const editorModule = vi.hoisted(() => ({ requested: vi.fn() }));
vi.mock("../editor/KnotEditor", () => {
  editorModule.requested();
  return { KnotEditor: () => null };
});

import DocPage from "./DocPage";

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

describe("DocPage editor preload", () => {
  it("requests the editor bundle while the doc metadata is still loading", async () => {
    // Every API call hangs: the page can only ever show its loading state.
    vi.stubGlobal("fetch", vi.fn(() => new Promise<Response>(() => {})));

    render(
      <QueryClientProvider client={new QueryClient()}>
        <SessionProvider>
          <MemoryRouter initialEntries={["/docs/d1"]}>
            <Routes>
              <Route path="/docs/:id" element={<DocPage />} />
            </Routes>
          </MemoryRouter>
        </SessionProvider>
      </QueryClientProvider>,
    );

    expect(screen.getByText("Loading…")).toBeInTheDocument();
    await vi.waitFor(() => expect(editorModule.requested).toHaveBeenCalled());
  });
});
