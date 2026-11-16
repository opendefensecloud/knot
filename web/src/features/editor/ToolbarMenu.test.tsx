/** The toolbar menu closes on Escape inside it and on a click outside it. */

import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { ToolbarMenu } from "./ToolbarMenu";

let bound: BoundEditor | null = null;
afterEach(() => {
  cleanup();
  bound?.destroy();
  bound = null;
});

function mount() {
  bound = mountBoundEditor();
  const run = vi.fn();
  render(
    <div>
      <ToolbarMenu
        editor={bound.editor}
        btnClass=""
        testId="menu"
        label="Menu"
        icon={<span />}
        items={[{ testId: "item", label: "Item", icon: <span />, run }]}
      />
      <button data-testid="elsewhere">elsewhere</button>
    </div>,
  );
  act(() => {
    fireEvent.click(screen.getByTestId("menu"));
  });
  return run;
}

describe("toolbar menu", () => {
  it("closes on Escape in the menu and returns to the text", async () => {
    mount();
    act(() => {
      fireEvent.keyDown(screen.getByTestId("item"), { key: "Escape" });
    });
    expect(screen.queryByTestId("item")).toBeNull();
    await waitFor(() => expect(bound!.editor.view.hasFocus()).toBe(true));
  });

  it("leaves an Escape meant for something else alone", () => {
    mount();
    act(() => {
      fireEvent.keyDown(screen.getByTestId("elsewhere"), { key: "Escape" });
    });
    expect(screen.getByTestId("item")).toBeInTheDocument();
  });

  it("closes on a click outside without running an item", () => {
    const run = mount();
    act(() => {
      fireEvent.mouseDown(screen.getByTestId("elsewhere"));
    });
    expect(screen.queryByTestId("item")).toBeNull();
    expect(run).not.toHaveBeenCalled();
  });
});
