import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { useUi } from "../../stores/ui";

import { DocWidthToggle } from "./DocWidthToggle";

const realWidth = window.innerWidth;

function setViewportWidth(w: number) {
  Object.defineProperty(window, "innerWidth", { value: w, configurable: true, writable: true });
}

afterEach(() => {
  cleanup();
  setViewportWidth(realWidth);
});

beforeEach(() => {
  localStorage.clear();
  useUi.getState().setDocWidth("fixed");
  setViewportWidth(1280);
});

describe("DocWidthToggle", () => {
  it("reports the current mode through aria-pressed", () => {
    render(<DocWidthToggle />);
    expect(screen.getByTestId("toggle-doc-width")).toHaveAttribute("aria-pressed", "false");
  });

  it("switches the store to wide when clicked", () => {
    render(<DocWidthToggle />);
    fireEvent.click(screen.getByTestId("toggle-doc-width"));
    expect(useUi.getState().docWidth).toBe("wide");
    expect(screen.getByTestId("toggle-doc-width")).toHaveAttribute("aria-pressed", "true");
  });

  it("switches back to fixed on a second click", () => {
    render(<DocWidthToggle />);
    const btn = screen.getByTestId("toggle-doc-width");
    fireEvent.click(btn);
    fireEvent.click(btn);
    expect(useUi.getState().docWidth).toBe("fixed");
  });

  it("teaches the keyboard shortcut through its accessible label", () => {
    render(<DocWidthToggle />);
    expect(screen.getByTestId("toggle-doc-width")).toHaveAccessibleName(/⌘⇧F/);
  });

  it("renders nothing below the desktop breakpoint, where wide mode is a no-op", () => {
    setViewportWidth(500);
    render(<DocWidthToggle />);
    expect(screen.queryByTestId("toggle-doc-width")).toBeNull();
  });
});
