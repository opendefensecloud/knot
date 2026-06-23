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
