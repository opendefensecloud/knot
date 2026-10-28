import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, fireEvent } from "@testing-library/react";
import { ContextMenu } from "./ContextMenu";

// The menu portals into <body>; unmount it so tests don't see each other's.
afterEach(cleanup);

describe("ContextMenu", () => {
  it("renders items + calls onSelect + onClose on click", () => {
    const onClose = vi.fn();
    const onSelect = vi.fn();
    render(
      <ContextMenu
        x={10}
        y={20}
        onClose={onClose}
        items={[{ label: "Rename", testId: "ctx-rename", onSelect }]}
      />,
    );
    fireEvent.click(screen.getByTestId("ctx-rename"));
    expect(onSelect).toHaveBeenCalled();
    expect(onClose).toHaveBeenCalled();
  });

  // The menu portals to <body>, but React events still bubble through the
  // React tree. Defensive: the sidebar renders it next to its row, not inside
  // it, but no key handler around it may take Enter/Space. The old sortable
  // row's keyboard sensor did (with preventDefault), so a keyboard user could
  // never activate an item.
  it("keeps key and pointer events from reaching the component that opened it", () => {
    const onSelect = vi.fn();
    const rowKeyDown = vi.fn();
    const rowPointerDown = vi.fn();
    render(
      <div onKeyDown={rowKeyDown} onPointerDown={rowPointerDown}>
        <ContextMenu
          x={0}
          y={0}
          onClose={() => {}}
          items={[{ label: "Move to archive", testId: "ctx-delete", onSelect }]}
        />
      </div>,
    );
    const item = screen.getByTestId("ctx-delete");
    fireEvent.keyDown(item, { key: "Enter", code: "Enter" });
    fireEvent.pointerDown(item);
    expect(rowKeyDown).not.toHaveBeenCalled();
    expect(rowPointerDown).not.toHaveBeenCalled();
  });

  // Defensive: the sidebar's rows start a drag on mousedown and touchstart
  // (dnd-kit's mouse and touch sensors). The menu is rendered next to a row,
  // not inside it, but a press on it must not start a drag wherever it is.
  it("keeps mouse and touch presses from reaching the component that opened it", () => {
    const rowMouseDown = vi.fn();
    const rowTouchStart = vi.fn();
    render(
      <div onMouseDown={rowMouseDown} onTouchStart={rowTouchStart}>
        <ContextMenu
          x={0}
          y={0}
          onClose={() => {}}
          items={[{ label: "Rename", testId: "ctx-rename", onSelect: () => {} }]}
        />
      </div>,
    );
    const item = screen.getByTestId("ctx-rename");
    fireEvent.mouseDown(item);
    fireEvent.touchStart(item);
    expect(rowMouseDown).not.toHaveBeenCalled();
    expect(rowTouchStart).not.toHaveBeenCalled();
  });

  it("takes focus when it opens, so keyboard users can reach its items", () => {
    render(
      <ContextMenu
        x={0}
        y={0}
        onClose={() => {}}
        items={[
          { label: "Rename", testId: "ctx-rename", onSelect: () => {} },
          { label: "Move to archive", testId: "ctx-delete", onSelect: () => {} },
        ]}
      />,
    );
    expect(screen.getByTestId("ctx-rename")).toHaveFocus();
    fireEvent.keyDown(screen.getByTestId("ctx-rename"), { key: "ArrowDown", code: "ArrowDown" });
    expect(screen.getByTestId("ctx-delete")).toHaveFocus();
    fireEvent.keyDown(screen.getByTestId("ctx-delete"), { key: "ArrowUp", code: "ArrowUp" });
    expect(screen.getByTestId("ctx-rename")).toHaveFocus();
  });

  it("closes on Escape pressed on one of its items", () => {
    const onClose = vi.fn();
    render(
      <ContextMenu
        x={0}
        y={0}
        onClose={onClose}
        items={[{ label: "Rename", testId: "ctx-rename", onSelect: () => {} }]}
      />,
    );
    fireEvent.keyDown(screen.getByTestId("ctx-rename"), { key: "Escape", code: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("closes on Escape", () => {
    const onClose = vi.fn();
    render(<ContextMenu x={0} y={0} onClose={onClose} items={[]} />);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(onClose).toHaveBeenCalled();
  });

  it("closes on outside mousedown", () => {
    const onClose = vi.fn();
    render(
      <div>
        <button data-testid="outside">outside</button>
        <ContextMenu x={0} y={0} onClose={onClose} items={[]} />
      </div>,
    );
    fireEvent.mouseDown(screen.getByTestId("outside"));
    expect(onClose).toHaveBeenCalled();
  });
});
