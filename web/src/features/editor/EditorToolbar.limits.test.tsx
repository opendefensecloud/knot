import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { boardsApi } from "../../lib/boards.api";
import { useUi } from "../../stores/ui";
import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { EditorToolbar } from "./EditorToolbar";

describe("EditorToolbar when a limit refuses a new diagram", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    cleanup();
    bound?.destroy();
    bound = null;
    vi.restoreAllMocks();
  });

  it("names the page quota instead of 'Failed to create diagram.'", async () => {
    useUi.setState({ toasts: [] });
    vi.spyOn(boardsApi, "create").mockResolvedValue({
      error: {
        code: "quota.exceeded",
        message: "quota exceeded",
        details: { quota: "blob_bytes", scope: "workspace", limit: 100 },
        status: 403,
      },
    });
    bound = mountBoundEditor();
    render(
      <QueryClientProvider client={new QueryClient({ defaultOptions: { queries: { retry: false } } })}>
        <EditorToolbar editor={bound.editor} docId="doc-1" />
      </QueryClientProvider>,
    );
    fireEvent.click(screen.getByTestId("toolbar-excalidraw"));
    await waitFor(() =>
      expect(useUi.getState().toasts).toMatchObject([
        { kind: "warn", text: "This workspace has reached its file storage limit.", key: "limit" },
      ]),
    );
  });
});
