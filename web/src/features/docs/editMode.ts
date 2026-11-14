import { isApplePlatform } from "../../lib/platform";

/** sessionStorage key holding the per-tab edit-mode flag for a doc. */
export function editModeKey(id: string): string {
  return `knot.editMode.${id}`;
}

/**
 * Seed edit mode for a freshly-created doc so DocPage opens it editable.
 * Call this with the new doc id immediately before navigating to it.
 */
export function markDocEditMode(id: string): void {
  try {
    window.sessionStorage.setItem(editModeKey(id), "1");
  } catch {
    /* sessionStorage unavailable — DocPage falls back to view mode */
  }
}

/**
 * Mod+E, the edit-mode toggle: ⌘E on Apple platforms, Ctrl+E elsewhere.
 * Shared by DocPage's window listener and the editor, which must keep the
 * same chord away from its own keymap (Tiptap's `Mod-e` toggles inline code).
 *
 * Mod is exactly ProseMirror's: on a Mac, Ctrl+E is the system's "end of
 * line" (Cocoa's ^E, the emacs binding), which Tiptap's mac keymap implements
 * as selectTextblockEnd, so claiming it there broke typing (#86). Elsewhere
 * Meta+E (Super/Windows) belongs to the OS. Shift and Alt variants are left
 * to other bindings.
 *
 * `apple` is for tests; it defaults to the platform the page runs on.
 */
export function isEditModeShortcut(
  e: Pick<KeyboardEvent, "metaKey" | "ctrlKey" | "shiftKey" | "altKey" | "key">,
  apple: boolean = isApplePlatform(),
): boolean {
  const modOnly = apple ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  return modOnly && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "e";
}

/** The toggle's chord as shown in button labels: "⌘E" or "Ctrl+E". */
export function editModeShortcutLabel(): string {
  return isApplePlatform() ? "⌘E" : "Ctrl+E";
}

/**
 * The toggle's chord for `aria-keyshortcuts`, in that attribute's own
 * syntax. Only the one that works here: announcing Control+E on a Mac would
 * send a screen-reader user to the end of the line instead.
 */
export function editModeAriaKeyshortcuts(): string {
  return isApplePlatform() ? "Meta+E" : "Control+E";
}
