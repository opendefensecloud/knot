import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const hook = vi.hoisted(() => ({ exporting: false, run: vi.fn() }));
vi.mock("./useExport", () => ({
  useExport: () => ({ exporting: hook.exporting, run: hook.run }),
}));

import { ExportMenu } from "./ExportMenu";

beforeEach(() => {
  hook.exporting = false;
  hook.run.mockReset();
});
afterEach(cleanup);

function mount(isTemplate = false) {
  render(
    <ExportMenu docId="d1" title="Runbook" isTemplate={isTemplate} status="connected" pendingBytes={0} />,
  );
}

function open() {
  fireEvent.click(screen.getByTestId("doc-export"));
}

describe("ExportMenu", () => {
  it("opens a menu with Word, OpenDocument and both Markdown items", () => {
    mount();
    const trigger = screen.getByTestId("doc-export");
    expect(trigger).toHaveAttribute("aria-haspopup", "menu");
    expect(trigger).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByRole("menu")).toBeNull();
    open();
    expect(trigger).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByRole("menu")).toBeInTheDocument();
    expect(screen.getAllByRole("menuitem")).toHaveLength(4);
    expect(screen.getByRole("separator")).toBeInTheDocument();
    expect(screen.getByTestId("export-docx")).toHaveTextContent("Word document");
    expect(screen.getByTestId("export-docx")).toHaveTextContent(
      ".docx · opens in Word, LibreOffice, Google Docs",
    );
    expect(screen.getByTestId("export-odt")).toHaveTextContent("OpenDocument text");
    expect(screen.getByTestId("export-odt")).toHaveTextContent(".odt · LibreOffice's own format");
    expect(screen.getByTestId("export-md")).toHaveTextContent("Markdown (zip)");
    expect(screen.getByTestId("export-md-subtree")).toHaveTextContent("Markdown with subpages (zip)");
  });

  it("hides the Markdown items on template pages", () => {
    mount(true);
    open();
    expect(screen.getAllByRole("menuitem")).toHaveLength(2);
    expect(screen.queryByRole("separator")).toBeNull();
    expect(screen.queryByTestId("export-md")).toBeNull();
    expect(screen.queryByTestId("export-md-subtree")).toBeNull();
  });

  it("supports arrow keys, Home/End and Escape returning focus to the trigger", () => {
    mount();
    open();
    const items = screen.getAllByRole("menuitem");
    expect(items[0]).toHaveFocus();
    fireEvent.keyDown(items[0]!, { key: "ArrowDown" });
    expect(items[1]).toHaveFocus();
    fireEvent.keyDown(items[1]!, { key: "End" });
    expect(items[3]).toHaveFocus();
    fireEvent.keyDown(items[3]!, { key: "ArrowDown" });
    expect(items[0]).toHaveFocus();
    fireEvent.keyDown(items[0]!, { key: "ArrowUp" });
    expect(items[3]).toHaveFocus();
    fireEvent.keyDown(items[3]!, { key: "Home" });
    expect(items[0]).toHaveFocus();
    fireEvent.keyDown(items[0]!, { key: "Escape" });
    expect(screen.queryByRole("menu")).toBeNull();
    expect(screen.getByTestId("doc-export")).toHaveFocus();
  });

  describe("at phone width", () => {
    /** Lays the menu out at `left`..`right` in a 375 px wide viewport. */
    function layOut(left: number, right: number) {
      vi.spyOn(document.documentElement, "clientWidth", "get").mockReturnValue(375);
      vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function (this: HTMLElement) {
        const [l, r] = this.getAttribute("role") === "menu" ? [left, right] : [0, 0];
        return { left: l, right: r, width: r - l, top: 0, bottom: 0, height: 0, x: l, y: 0, toJSON: () => ({}) };
      });
    }
    afterEach(() => vi.restoreAllMocks());

    it("shifts a menu that would open past the left edge back into view", () => {
      // A trigger near the left edge of a wrapped header row.
      layOut(-190, 100);
      mount();
      open();
      expect(screen.getByRole("menu").style.transform).toBe("translateX(206px)");
    });

    it("shifts a menu that would open past the right edge back into view", () => {
      layOut(300, 590);
      mount();
      open();
      expect(screen.getByRole("menu").style.transform).toBe("translateX(-231px)");
    });

    it("leaves a menu that fits where it is", () => {
      layOut(40, 330);
      mount();
      open();
      expect(screen.getByRole("menu").style.transform).toBe("");
    });
  });

  it("closes on an outside mousedown", () => {
    mount();
    open();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("disables export items and spins the icon while exporting", () => {
    hook.exporting = true;
    mount();
    open();
    expect(screen.getByTestId("export-docx")).toBeDisabled();
    expect(screen.getByTestId("export-odt")).toBeDisabled();
    expect(screen.getByTestId("doc-export").querySelector(".animate-spin")).not.toBeNull();
  });

  it("runs docx export on select and closes the menu", () => {
    mount();
    open();
    fireEvent.click(screen.getByTestId("export-docx"));
    expect(hook.run).toHaveBeenCalledWith("docx");
    expect(screen.queryByRole("menu")).toBeNull();
  });

  it("runs odt export on select", () => {
    mount();
    open();
    fireEvent.click(screen.getByTestId("export-odt"));
    expect(hook.run).toHaveBeenCalledWith("odt");
  });

  it("downloads Markdown zips with and without subpages", () => {
    const hrefs: string[] = [];
    const click = vi
      .spyOn(HTMLAnchorElement.prototype, "click")
      .mockImplementation(function (this: HTMLAnchorElement) {
        hrefs.push(`${this.getAttribute("href")}|${this.download}`);
      });
    mount();
    open();
    fireEvent.click(screen.getByTestId("export-md"));
    expect(screen.queryByRole("menu")).toBeNull();
    open();
    fireEvent.click(screen.getByTestId("export-md-subtree"));
    expect(hrefs).toEqual([
      "/api/docs/d1/export?descendants=false|Runbook.zip",
      "/api/docs/d1/export?descendants=true|Runbook.zip",
    ]);
    click.mockRestore();
  });
});
