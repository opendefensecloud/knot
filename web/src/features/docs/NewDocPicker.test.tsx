import React from "react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, cleanup, fireEvent } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { NewDocPicker } from "./DocTree";

afterEach(cleanup);

function renderPicker(props: Partial<React.ComponentProps<typeof NewDocPicker>> = {}) {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const onPickBlank = vi.fn();
  render(
    <QueryClientProvider client={qc}>
      <NewDocPicker
        onClose={() => {}}
        onPickBlank={onPickBlank}
        onPickTemplate={() => {}}
        current={null}
        {...props}
      />
    </QueryClientProvider>,
  );
  return { onPickBlank };
}

describe("NewDocPicker location selector", () => {
  it("hides the selector when no doc is open", () => {
    renderPicker({ current: null });
    expect(screen.queryByTestId("new-doc-loc-nested")).toBeNull();
  });

  it("defaults to nested and passes the current doc id when a doc is open", () => {
    const { onPickBlank } = renderPicker({ current: { id: "cur", parent_id: "par", title: "Specs", canNest: true, canSibling: true } });
    expect(screen.getByTestId("new-doc-loc-nested")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("cur");
  });

  it("same-level passes the current doc's parent_id", () => {
    const { onPickBlank } = renderPicker({ current: { id: "cur", parent_id: "par", title: "Specs", canNest: true, canSibling: true } });
    fireEvent.click(screen.getByTestId("new-doc-loc-sibling"));
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("par");
  });

  it("offers no same level for a page whose parent the user can't see", () => {
    // A page shared with the user lists `parent_id: null`: same level would
    // be the workspace's top level.
    const { onPickBlank } = renderPicker({
      current: { id: "cur", parent_id: null, title: "Shared", canNest: true, canSibling: false },
    });
    expect(screen.queryByTestId("new-doc-loc-sibling")).toBeNull();
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("cur");
  });

  it("starts on same level when the user can't add pages under the open one", () => {
    const { onPickBlank } = renderPicker({
      current: { id: "cur", parent_id: "par", title: "Specs", canNest: false, canSibling: true },
    });
    expect(screen.queryByTestId("new-doc-loc-nested")).toBeNull();
    expect(screen.getByTestId("new-doc-loc-sibling")).toBeChecked();
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("par");
  });

  it("says the page goes to the top level when neither place is open to the user", () => {
    const { onPickBlank } = renderPicker({
      current: { id: "cur", parent_id: null, title: "Shared", canNest: false, canSibling: false },
    });
    expect(screen.getByTestId("new-doc-loc-top")).toHaveTextContent("At the top level");
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith(null);
  });

  it("moves off a place that stops being offered while the picker is open", () => {
    const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
    const onPickBlank = vi.fn();
    const picker = (canSibling: boolean) => (
      <QueryClientProvider client={qc}>
        <NewDocPicker
          onClose={() => {}}
          onPickBlank={onPickBlank}
          onPickTemplate={() => {}}
          current={{ id: "cur", parent_id: "par", title: "Specs", canNest: true, canSibling }}
        />
      </QueryClientProvider>
    );
    const { rerender } = render(picker(true));
    fireEvent.click(screen.getByTestId("new-doc-loc-sibling"));

    // The list refetched: the user can no longer add pages beside it.
    rerender(picker(false));

    expect(screen.getByTestId("new-doc-loc-nested")).toBeChecked();
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("cur");
  });
});
