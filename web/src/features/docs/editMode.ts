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
 * ⌘E / Ctrl+E, the edit-mode toggle. Shared by DocPage's window listener and
 * the editor, which must keep the same chord away from its own keymap.
 * Shift and Alt variants are left to other bindings.
 */
export function isEditModeShortcut(
  e: Pick<KeyboardEvent, "metaKey" | "ctrlKey" | "shiftKey" | "altKey" | "key">,
): boolean {
  return (e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "e";
}
