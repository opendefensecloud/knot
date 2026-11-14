/**
 * Whether keyboard shortcuts follow Apple's conventions: ⌘ is the command
 * modifier, and Ctrl keeps the Cocoa/emacs text bindings (^A, ^E, …).
 *
 * The test is prosemirror-keymap's, character for character: it decides what
 * a `Mod-` binding means in the editor, so knot's own chords (the ⌘E/Ctrl+E
 * edit toggle) must agree with it, or a chord knot claims and the editor's
 * `Mod-e` (inline code) would part ways on some platform.
 *
 * Read at call time rather than once at load, so tests can switch platforms.
 */
export function isApplePlatform(): boolean {
  return typeof navigator !== "undefined" && /Mac|iP(hone|[oa]d)/.test(navigator.platform);
}
