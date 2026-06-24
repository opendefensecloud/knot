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
    const { onPickBlank } = renderPicker({ current: { id: "cur", parent_id: "par", title: "Specs" } });
    expect(screen.getByTestId("new-doc-loc-nested")).toBeInTheDocument();
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("cur");
  });

  it("same-level passes the current doc's parent_id", () => {
    const { onPickBlank } = renderPicker({ current: { id: "cur", parent_id: "par", title: "Specs" } });
    fireEvent.click(screen.getByTestId("new-doc-loc-sibling"));
    fireEvent.click(screen.getByTestId("new-doc-blank"));
    expect(onPickBlank).toHaveBeenCalledWith("par");
  });
});
