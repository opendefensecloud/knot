import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { useUi } from "../../stores/ui";
import { SKINS } from "../../styles/skins";

import { SkinPicker } from "./SkinPicker";

afterEach(cleanup);

beforeEach(() => {
  localStorage.clear();
  useUi.getState().setSkin("light");
});

describe("SkinPicker", () => {
  it("renders one self-previewing card per registered skin", () => {
    render(<SkinPicker />);
    for (const skin of SKINS) {
      const card = screen.getByTestId(`skin-${skin.id}`);
      // The card carries its own data-skin so the CSS tokens scope to it
      // and it previews itself in its real colours.
      expect(card).toHaveAttribute("data-skin", skin.id);
      expect(card).toHaveTextContent(skin.name);
    }
  });

  it("groups cards under Light and Dark headings", () => {
    render(<SkinPicker />);
    expect(screen.getByRole("group", { name: "Light" })).toContainElement(screen.getByTestId("skin-paper"));
    expect(screen.getByRole("group", { name: "Dark" })).toContainElement(screen.getByTestId("skin-nord"));
  });

  it("marks only the active skin as pressed", () => {
    render(<SkinPicker />);
    expect(screen.getByTestId("skin-light")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("skin-nord")).toHaveAttribute("aria-pressed", "false");
  });

  it("switches the store to the clicked skin", () => {
    render(<SkinPicker />);
    fireEvent.click(screen.getByTestId("skin-gruvbox"));
    expect(useUi.getState().skin).toBe("gruvbox");
    expect(document.documentElement.getAttribute("data-skin")).toBe("gruvbox");
    expect(screen.getByTestId("skin-gruvbox")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("skin-light")).toHaveAttribute("aria-pressed", "false");
  });
});
