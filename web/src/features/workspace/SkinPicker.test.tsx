import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

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

const ui = () => useUi.getState();

describe("SkinPicker modes", () => {
  beforeEach(() => {
    // Drops the slots, mode and location earlier tests left in the store.
    ui().adoptStoredSkinPrefs();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("checks the radio for the current mode", () => {
    ui().setSkinMode("sun");
    render(<SkinPicker />);
    const choice = screen.getByRole("group", { name: "Skin switching" });
    for (const mode of ["manual", "system", "sun"]) {
      const radio = screen.getByTestId(`skin-mode-${mode}`);
      expect(choice).toContainElement(radio);
      expect(radio).toHaveAttribute("name", "skin-mode");
      if (mode === "sun") expect(radio).toBeChecked();
      else expect(radio).not.toBeChecked();
    }
    expect(screen.getByRole("radio", { name: "By hand" })).toBe(screen.getByTestId("skin-mode-manual"));
    expect(screen.getByRole("radio", { name: "Follow system" })).toBe(screen.getByTestId("skin-mode-system"));
    expect(screen.getByRole("radio", { name: "Follow the sun" })).toBe(screen.getByTestId("skin-mode-sun"));
  });

  it("switches the mode from the radios", () => {
    render(<SkinPicker />);
    fireEvent.click(screen.getByTestId("skin-mode-system"));
    expect(ui().skinMode).toBe("system");
    expect(screen.getByTestId("skin-mode-system")).toBeChecked();
    expect(screen.getByTestId("skin-mode-manual")).not.toBeChecked();
  });

  it("picks a light and a dark skin in an automatic mode", () => {
    ui().setSkinMode("sun");
    render(<SkinPicker />);
    fireEvent.click(screen.getByTestId("skin-paper"));
    fireEvent.click(screen.getByTestId("skin-nord"));
    expect(ui()).toMatchObject({ skinMode: "sun", lightSkin: "paper", darkSkin: "nord" });
    expect(screen.getByTestId("skin-paper")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("skin-nord")).toHaveAttribute("aria-pressed", "true");
    expect(screen.getByTestId("skin-light")).toHaveAttribute("aria-pressed", "false");
  });

  it("picks by hand in manual mode", () => {
    render(<SkinPicker />);
    fireEvent.click(screen.getByTestId("skin-paper"));
    expect(ui()).toMatchObject({ skinMode: "manual", manualSkin: "paper", lightSkin: "light" });
  });

  it("says when each group's skin shows, in the automatic modes only", () => {
    render(<SkinPicker />);
    expect(screen.queryByTestId("skin-hint-light")).toBeNull();
    expect(screen.queryByTestId("skin-hint-dark")).toBeNull();

    act(() => ui().setSkinMode("system"));
    expect(screen.getByTestId("skin-hint-light")).toHaveTextContent("While your system is in light mode");
    expect(screen.getByTestId("skin-hint-dark")).toHaveTextContent("While your system is in dark mode");
    // The legends stay the group names; the hints describe the groups.
    expect(screen.getByRole("group", { name: "Light" })).toHaveAccessibleDescription("While your system is in light mode");
    expect(screen.getByRole("group", { name: "Dark" })).toHaveAccessibleDescription("While your system is in dark mode");

    act(() => ui().setSkinMode("sun"));
    expect(screen.getByRole("group", { name: "Light" })).toHaveAccessibleDescription("From sunrise to sunset");
    expect(screen.getByRole("group", { name: "Dark" })).toHaveAccessibleDescription("From sunset to sunrise");
  });

  it("marks the card showing as Now in the automatic modes only", () => {
    render(<SkinPicker />);
    expect(screen.queryByTestId("skin-now")).toBeNull();

    // jsdom has no matchMedia; this one reports a dark system.
    vi.stubGlobal("matchMedia", () => ({ matches: true }));
    act(() => {
      ui().setDarkSkin("nord");
      ui().setSkinMode("system");
    });
    expect(ui().skin).toBe("nord");
    const marks = screen.getAllByTestId("skin-now");
    expect(marks).toHaveLength(1);
    expect(marks[0]).toHaveTextContent("Now");
    expect(screen.getByTestId("skin-nord")).toContainElement(marks[0]!);
  });

  it("shows the sun's times and location in sun mode only", () => {
    render(<SkinPicker />);
    expect(screen.queryByTestId("sun-times")).toBeNull();

    act(() => ui().setSkinMode("system"));
    expect(screen.queryByTestId("sun-times")).toBeNull();

    act(() => ui().setSkinMode("sun"));
    expect(screen.getByTestId("sun-times")).toBeInTheDocument();
    expect(screen.getByTestId("sun-location-source")).toBeInTheDocument();
  });
});
