/**
 * On a Mac, Ctrl+E is the system's "end of line" (Cocoa's ^E, also the emacs
 * binding), and Tiptap's core keymap binds it there to selectTextblockEnd.
 * The edit-mode toggle used to claim it on every platform, and KnotEditor
 * stops ProseMirror's keymaps for the toggle chord, so a Mac user's ^E left
 * edit mode instead of moving the caret (#86). The toggle is now Mod+E, the
 * chord Tiptap's `Mod-e` (toggleCode) means: ⌘E on a Mac.
 *
 * prosemirror-keymap decides what `Mod-` means, and Tiptap whether to add its
 * mac keymap, from `navigator.platform`; prosemirror-keymap reads it once,
 * when it loads. Hence the stub in vi.hoisted, before any import.
 */

import { afterAll, afterEach, describe, expect, it, vi } from "vitest";

vi.hoisted(() => {
  Object.defineProperty(window.navigator, "platform", { value: "MacIntel", configurable: true });
});

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { placeSelection } from "../../test/editorInput";
import { restorePlatform } from "../../test/platform";
import { isEditModeShortcut } from "../docs/editMode";

afterAll(restorePlatform);

describe("Ctrl+E and ⌘E on a Mac", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    bound?.destroy();
    bound = null;
  });

  function withParagraph(text: string): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.setContent(`<p>${text}</p>`);
    return bound;
  }

  function press(b: BoundEditor, init: KeyboardEventInit): { handled: boolean; event: KeyboardEvent } {
    const event = new KeyboardEvent("keydown", { code: "KeyE", ...init });
    const handled = b.editor.view.someProp("handleKeyDown", (fn) => fn(b.editor.view, event)) ?? false;
    return { handled, event };
  }

  function hasCodeMark(b: BoundEditor): boolean {
    return JSON.stringify(b.editor.getJSON()).includes('"code"') || b.editor.isActive("code");
  }

  it("Ctrl+E is not the toggle, and moves the caret to the end of the paragraph", () => {
    const b = withParagraph("hello world");
    placeSelection(b, 2);

    const { handled, event } = press(b, { key: "e", ctrlKey: true });

    // KnotEditor and DocPage let a chord through unless this says it is theirs.
    expect(isEditModeShortcut(event)).toBe(false);
    expect(handled).toBe(true);
    // "hello world" fills positions 1..12 of the paragraph.
    expect(b.editor.state.selection.empty).toBe(true);
    expect(b.editor.state.selection.head).toBe(12);
    expect(hasCodeMark(b)).toBe(false);
  });

  it("Ctrl+A, its counterpart, moves the caret to the start", () => {
    const b = withParagraph("hello world");
    placeSelection(b, 6);
    const { handled } = press(b, { key: "a", code: "KeyA", ctrlKey: true });
    expect(handled).toBe(true);
    expect(b.editor.state.selection.head).toBe(1);
  });

  it("⌘E is the toggle — the same chord as the editor's Mod-e, which KnotEditor must hold back", () => {
    const b = withParagraph("hello world");
    placeSelection(b, 1, 6);

    const { handled, event } = press(b, { key: "e", metaKey: true });

    expect(isEditModeShortcut(event)).toBe(true);
    // Unblocked, ProseMirror's keymap reads ⌘E as Mod-e and toggles inline
    // code. That is why KnotEditor's keydown returns true for the toggle: the
    // predicate and the keymap must agree on what Mod is.
    expect(handled).toBe(true);
    expect(hasCodeMark(b)).toBe(true);
  });
});
