import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { createRef } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { restorePlatform, stubPlatform } from "../../test/platform";
import { DocDockBar, DocDockControls } from "./DocDock";

afterEach(() => {
  cleanup();
  restorePlatform();
});

function controls(overrides: Partial<Parameters<typeof DocDockControls>[0]> = {}) {
  return (
    <DocDockControls
      showEdit
      editMode={false}
      onToggleEdit={() => {}}
      showComments
      commentsOpen={false}
      onToggleComments={() => {}}
      editToggleRef={createRef()}
      commentsRef={createRef()}
      size="sm"
      {...overrides}
    />
  );
}

describe("DocDockControls", () => {
  it("labels the edit toggle by mode and advertises the shortcut", () => {
    stubPlatform("Linux x86_64");
    const { rerender } = render(controls());
    const edit = screen.getByTestId("dock-toggle-edit-mode");
    expect(edit).toHaveAttribute("aria-label", "Edit (Ctrl+E)");
    expect(edit).toHaveAttribute("aria-pressed", "false");
    expect(edit).toHaveAttribute("aria-keyshortcuts", "Control+E");

    rerender(controls({ editMode: true }));
    expect(edit).toHaveAttribute("aria-label", "Stop editing (Ctrl+E)");
    expect(edit).toHaveAttribute("aria-pressed", "true");
  });

  it("advertises ⌘E on a Mac, where Ctrl+E is left to the editor", () => {
    stubPlatform("MacIntel");
    render(controls());
    const edit = screen.getByTestId("dock-toggle-edit-mode");
    expect(edit).toHaveAttribute("aria-label", "Edit (⌘E)");
    expect(edit).toHaveAttribute("aria-keyshortcuts", "Meta+E");
  });

  it("presses Comments while the rail is open", () => {
    const { rerender } = render(controls());
    expect(screen.getByTestId("dock-open-comments")).toHaveAttribute("aria-pressed", "false");
    rerender(controls({ commentsOpen: true }));
    expect(screen.getByTestId("dock-open-comments")).toHaveAttribute("aria-pressed", "true");
  });

  it("omits Edit for readers", () => {
    render(controls({ showEdit: false }));
    expect(screen.queryByTestId("dock-toggle-edit-mode")).toBeNull();
    expect(screen.getByTestId("dock-open-comments")).toBeInTheDocument();
  });

  it("uses the larger touch size when asked", () => {
    render(controls({ size: "md" }));
    expect(screen.getByTestId("dock-toggle-edit-mode").className).toContain("h-9 w-9");
  });

  it("calls its handlers", () => {
    const onToggleEdit = vi.fn();
    const onToggleComments = vi.fn();
    render(controls({ onToggleEdit, onToggleComments }));
    fireEvent.click(screen.getByTestId("dock-toggle-edit-mode"));
    fireEvent.click(screen.getByTestId("dock-open-comments"));
    expect(onToggleEdit).toHaveBeenCalledOnce();
    expect(onToggleComments).toHaveBeenCalledOnce();
  });
});

describe("DocDockBar", () => {
  it("is a labelled group marked as a sticky band", () => {
    render(<DocDockBar title="Runbook">{controls()}</DocDockBar>);
    const dock = screen.getByRole("group", { name: "Document controls" });
    expect(dock).toHaveAttribute("data-testid", "doc-dock");
    expect(dock).toHaveAttribute("data-sticky-band");
    expect(dock).toHaveTextContent("Runbook");
  });
});
