# Spec: Doc dock, reachable document controls mid-document without losing your place

Status: ready to implement · Effort: **M** · Scope: `web/` only. No server, migration or Helm changes.

## 1. Problem

The user reads long documents and adds comments as they go. When they want to fix something partway down, the only Edit and Comments controls are in the action row under the title (`DocPage.tsx:301-380`). That row is not sticky, so the user has to scroll to the top and then find their place again.

Three things in the code make this worse. Each was checked:

1. **Toggling moves the text.** `EditorBody` renders `<EditorToolbar>` only while `canEdit` is true (`KnotEditor.tsx:405`). The toolbar sits in the normal document flow above `editor-host`: a 49px band (`py-2` + `h-8` + `border-b`) plus `mb-4`. Switching mode while scrolled down therefore shifts every line by **about 65px**. The scroll container is `<main className="overflow-y-auto">` (`AppShell.tsx:111`), not `window`.
2. **Entering edit mode leaves the caret where it was.** `setEditable` runs in a passive `useEffect` (`KnotEditor.tsx:322-325`). Nothing focuses the editor, so the first keystroke does nothing. If the caret is far away, typing jumps there.
3. **⌘E is broken in two ways:**
   - `@tiptap/extension-code@3.31.3` binds `"Mod-e": toggleCode` (`dist/index.js:96`), and StarterKit leaves Code enabled. DocPage's window listener (`DocPage.tsx:110-120`) only calls `preventDefault`. So pressing ⌘E while typing leaves edit mode **and** toggles an inline-code mark, which is a Yjs write.
   - The listener checks `effRole` but not `archived`, so ⌘E on an archived doc silently flips `knot.editMode.<id>` in sessionStorage.

There is also an existing mobile bug that this feature makes far more likely to hit. The fixed `menu-toggle` (`fixed top-3 left-3 z-30`, 36px) covers the Bold button whenever the toolbar is stuck to the top on a phone.

Corrections to the brief:
- The app is on **React 19.2** (`web/package.json`), not 18. Discrete-event flushing and the order of layout and passive effects behave the same.
- Tailwind is 3.4, so `max-sm:` and `motion-reduce:` are available.
- E2E runs one project, Desktop Chrome at 1280×720. `mobile.spec.ts` overrides the viewport to 375×667.

## 2. Decision

**Base: Design 1, the "Doc dock".** Once the action row scrolls out of `<main>`, a slim sticky bar appears at the top of the document column. It holds the truncated title, an Edit toggle and a Comments toggle. In edit mode the formatting toolbar is already sticky, so the same controls live in a **reserved slot at the right end of the toolbar**. Only one bar is ever stuck at the top, and the Edit/Stop toggle stays in the same place in both modes.

**Why this over the alternatives:**
- **Selection-bubble "Edit" (Design 2).** It only works after selecting text. It gives no way in from view mode and no Comments control for viewers, so it does not answer "access to controls".
- **Bottom floating pill (Design 3).** It puts chrome in two places while editing (toolbar at the top, Done at the bottom). It covers the lines being read and the selection handles, it collides with `add-comment-float` and toasts, and the iOS keyboard hides it.
- **Top dock.** It reuses the existing sticky-band pattern, its z-order and its translucent style, and it adds no layout height.

**Ideas taken from the other designs and the judges:**
- **Caret placement (Designs 2 and 3):** use `posAtCoords` on the **first visible line** below the band. Never use the start of a block that may begin above the viewport.
- **Mod-e fix (Design 3):** in `editorProps.handleDOMEvents.keydown`, return `true` for exactly Mod+E. That skips ProseMirror's keymap while the event still bubbles to the window listener. Do not use a capture-phase `stopPropagation` on the window.
- **`setEditable` timing (Design 2):** move it into `useLayoutEffect`. DocPage's layout-effect restore then measures after the edit-only CSS has applied (`.ProseMirror[contenteditable="false"] .knot-col-handles { display:none }`, `prose.css:176`).
- **⌘E gating:** gate on `!archived`, ignore `e.repeat` and `e.isComposing`, and share one predicate between the window listener and the editor.
- **Announcements:** add an sr-only live region that says "Editing" or "Viewing", only after a toggle the user made.
- **Small graft:** `aria-keyshortcuts`, IconButton `md` for the dock on phones, no caret move on mount.

**Judge constraints applied:**
- **Scroll correction.** One measured correction plus a single rAF re-check. The only multi-frame hold is for the comment rail, capped at 250ms. Every frame checks whether something other than us has scrolled `<main>` and stops if so. This handles user wheel, touch and key input and programmatic `scrollIntoView` the same way, without listeners or `overflow-anchor` changes.
- **Focus rescue.** Decided **before** the commit: in `toggleEditMode`, and in the IntersectionObserver callback before `setState`. Never in an after-the-fact effect that would already see `<body>`.
- **Toolbar slot.** The judges warned against adding to a toolbar row that could wrap. The slot is budgeted deterministically: 643 + 2 + 63 = **708 of 712px**. An e2e assertion keeps the toolbar a single row at the default desktop width. The slot only toggles `visibility`, never width or `display`, so crossing the threshold cannot change the toolbar's height.

## 3. UX behaviour

### Desktop / tablet (owner or editor, live doc)

**View mode, reading mid-document**
1. When the action row (`doc-actions`) has fully scrolled above the top of `<main>`, `doc-dock` fades in (150ms opacity). It sits at the top of the column with: `[Title (truncated) ………… ✎ Edit (⌘E) | 💬 Comments]`. It is 49px tall, `bg-bg/80 backdrop-blur border-b`, the same look as the toolbar.
2. The user clicks the dock's Edit button, or presses ⌘E/Ctrl+E:
   - The dock disappears and the formatting toolbar appears in the same band, already stuck.
   - The toolbar's right-end slot shows 👁 "Stop editing (⌘E)" (pressed) and 💬 Comments, roughly where the dock's controls were.
   - **The text does not move:** the block at the top of the visible area keeps its screen y (±1px).
   - **The caret goes to the reading point.** If the user had a text selection or a click caret on screen, it is kept. Otherwise the caret goes to the first line fully below the toolbar. The editor gets focus without scrolling, so typing starts there.
3. The user leaves edit mode with the toolbar's 👁 or ⌘E (now safe while typing):
   - The toolbar goes, the dock returns, and the text keeps its y.
   - Focus moves to the dock's Edit button (`preventScroll`), not to `<body>`.

**Commenting mid-document (any mode)**
- Selecting text shows the existing `add-comment-float`. It now clears whichever sticky band covers the top (dock or toolbar), not only the toolbar.
- The dock or slot Comments button is a **toggle** (`aria-pressed` follows `commentSidebarOpen`).
- When the column is at least 1020px wide, the rail pushes `.doc-shell` inward and the text re-wraps over a 160ms transition. The block at the reading line is held in place during the re-wrap, so the commented passage does not drift. Below 1020px the rail overlays the page and nothing moves.

**Back near the top**
- When the action row scrolls back into view, the view-mode dock unmounts. In edit mode the slot becomes `invisible` but keeps its place, so the toolbar never changes width or height.
- If focus was inside the dock or slot at that moment, it moves to the matching action-row button (`toggle-edit-mode` / `open-comments`).
- **The header is unchanged.** Toggling from the header while it is on screen behaves exactly as today: the toolbar visibly slides in, there is no scroll correction and no caret placement. The one exception is leaving edit mode: if focus was in the editor, toolbar or `<body>`, it goes to `toggle-edit-mode`.

### Mobile (<640px, the `mobile` bucket from `useViewport`)
- **Same structure.** The dock hides its title (`max-sm:hidden`) and right-aligns its controls, so it never sits under `menu-toggle` (left 12–48px).
- **Touch size.** Dock controls use IconButton `md` (36px), so the dock is about 53px tall.
- **Toolbar.** The toolbar row gets `max-sm:pl-7` (28px). Its first button then starts at x≈52px, clear of the menu button's right edge at 48px. This fixes the existing Bold overlap.
- **Wrapping.** At 327px the toolbar already wraps to about 3 rows. The slot is reserved and right-aligned on the last row, and its size never changes, so nothing jumps.
- **No horizontal overflow.** The dock stays inside the shell's content box (`inset-x-0`), so `<main>` gains no horizontal overflow.
- **Comment rail.** The rail is `max-w-full`. It covers the dock while open and has its own X.
- **Entry point.** Phones have no ⌘E, so the dock's Edit is the way in. Entering edit mode focuses the editor, which opens the on-screen keyboard. That is intended, because the user pressed Edit.

### Keyboard
- ⌘E/Ctrl+E toggles edit mode everywhere (editor, title, body, comment composer). Key repeat and IME composition are ignored.
- Inside the editor, ⌘E no longer toggles inline code. Inline code stays available from the toolbar's `toolbar-code` button and the backtick input rule.
- **Pressed with focus in a text field.** If the title input, comment composer or another textarea/contenteditable outside ProseMirror has focus, the mode still toggles but focus and caret are left alone.
- **Tab order.** Action row, then dock, then archived banner, then editor. In edit mode the slot is last in the toolbar row.
- **Announcements.** A polite live region says "Editing" or "Viewing" after each toggle the user makes. It stays silent on mount.

### Viewers, archived docs, Markdown view
- **Viewer** (live doc): the dock shows Comments only. There is no Edit, and ⌘E does nothing (unchanged).
- **Archived:** no dock at all, and ⌘E is a no-op. The latter fixes the sessionStorage flip.
- **Markdown view:** the dock shows Comments only. The toolbar slot is never used, because there is no toolbar.
- **Initial mount** (sessionStorage restore, `knot.editMode.defaultOn`, new docs): no caret move, no focus change, no scroll correction.

## 4. Implementation

### 4.1 `web/src/features/docs/editMode.ts` (edit)
Add one shared predicate:
```ts
export function isEditModeShortcut(e: Pick<KeyboardEvent, "metaKey" | "ctrlKey" | "shiftKey" | "altKey" | "key">): boolean {
  return (e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "e";
}
```

### 4.2 NEW `web/src/features/docs/useScrolledPast.ts`
```ts
export function isScrolledPast(entry: Pick<IntersectionObserverEntry, "isIntersecting" | "boundingClientRect" | "rootBounds">): boolean
// !entry.isIntersecting && entry.boundingClientRect.bottom <= (entry.rootBounds?.top ?? 0) + 1
// (below the viewport does NOT count)

export function useScrolledPast(el: Element | null, onBeforeChange?: (next: boolean) => void): boolean
```
- Takes an **element**, not a ref. DocPage passes state from a callback ref, because its early returns (`Loading…` and the error branch, `DocPage.tsx:199-211`) render before the action row exists.
- **Observer setup.** Effect on `[el]`: return early if `!el || typeof IntersectionObserver === "undefined"` (jsdom, so the existing `DocPage.*.test.tsx` keep passing). Otherwise create `new IntersectionObserver(cb, { root: el.closest("main"), threshold: 0 })`.
- **Callback.** It computes `next`. When `next !== current`, it calls `onBeforeChange?.(next)` and then `setState(next)`. Keep `onBeforeChange` in a ref so the observer is not recreated.
- **No `rootMargin`.** The dock host sits right after the header (§4.6), so its natural position is already above the top of `<main>` when it appears, and it is stuck from its first frame. In edit mode the toolbar's natural position is about 60px lower (`mt-6` + PresenceBar `h-7 mb-2`). The slot therefore becomes visible just before the toolbar sticks, which leaves no gap where the controls are unreachable.

### 4.3 NEW `web/src/features/docs/readingAnchor.ts`
Mostly pure DOM geometry, shared by DocPage and the editor:
```ts
export type ReadingAnchor = { element: Element; top: number };

export function findScroller(el: Element): HTMLElement | null           // el.closest("main")

/** Max bottom of visible [data-sticky-band] elements under `scope`
 *  (the doc-page section; not `document`). Skips elements with zero height
 *  or computed visibility "hidden". null if none. */
export function stickyCoverBottom(scope: ParentNode): number | null

/** Lowest i in [0,count) with bottomAt(i) > line (bottoms are monotonic in
 *  document order), else -1. Binary search → O(log n) rect reads. */
export function firstIndexBelow(count: number, bottomAt: (i: number) => number, line: number): number

/** line = max(scrollerRect.top, stickyCoverBottom(scope) ?? -Infinity).
 *  Anchor = first top-level `prose.children[i]` whose bottom > line, with
 *  its current rect.top. null if no prose or no such block. */
export function captureReadingAnchor(scroller: HTMLElement, prose: Element, scope: ParentNode): ReadingAnchor | null

/** Pin `anchor` at its recorded viewport top. Corrects synchronously, then
 *  re-pins on each rAF while elapsed < durationMs (durationMs = 0 → exactly
 *  one rAF re-check). Stops as soon as scroller.scrollTop differs by more
 *  than 1px from the value we last left it at (user wheel/touch/key, or a
 *  programmatic scrollIntoView), or when !anchor.element.isConnected.
 *  Returns cancel(). */
export function holdReadingPosition(scroller: HTMLElement, anchor: ReadingAnchor, opts?: { durationMs?: number }): () => void
```
`correct()` is:
```
delta = anchor.element.getBoundingClientRect().top - anchor.top
if |delta| >= 1: scroller.scrollTop += delta
expected = scroller.scrollTop
```
Reading `expected` back from `scrollTop` absorbs fractional rounding. The correction is measured, so it cannot double up with Chrome's native scroll anchoring. If native anchoring has already compensated, delta is 0. `overflow-anchor` is **not** touched.

### 4.4 NEW `web/src/features/editor/readingCaret.ts`
```ts
export type KnotEditorHandle = { placeCaretAtReadingLine(): void };

export function chooseCaretStrategy(a: { isText: boolean; headTop: number; headBottom: number; visibleTop: number; visibleBottom: number }): "keep" | "reading-line"
// "keep" iff isText && headTop >= visibleTop - 1 && headBottom <= visibleBottom + 1

export function placeCaretAtReadingLine(view: EditorView, scope: ParentNode): void
```
`placeCaretAtReadingLine` works in five steps:
1. **Visible area.** `scroller = view.dom.closest("main")`; return if null. Let `sr` be the scroller's rect and `pr` the ProseMirror root's rect. Compute `visibleTop = max(sr.top, stickyCoverBottom(scope) ?? -Inf, pr.top)` and `visibleBottom = sr.bottom`.
2. **Keep.** `sel = view.state.selection`. If `chooseCaretStrategy({ isText: sel instanceof TextSelection, headTop/headBottom: view.coordsAtPos(sel.head) … })` returns `"keep"`, call `view.focus()` and return. Requiring a TextSelection matters: keeping a NodeSelection on an image or board would let the first keystroke replace it.
3. **Probe.** `hit = view.posAtCoords({ left: pr.left + 4, top: visibleTop + 4 })`.
   - If the hit's line is partly covered (`coordsAtPos(hit.pos).top < visibleTop - 1`), probe again at `{ top: coordsAtPos(hit.pos).bottom + 4 }`.
   - If there is still no hit, use the first block below `visibleTop` (`firstIndexBelow` over `view.dom.children`) and take `pos = view.posAtDOM(block, 0)`.
4. **Text-only selection.** `$p = doc.resolve(pos)`; `next = Selection.findFrom($p, 1, true) ?? Selection.findFrom($p, -1, true)`. The `textOnly` argument means the caret never lands as a NodeSelection on Excalidraw, Mermaid or an attachment.
5. **Dispatch.** If `next`: `view.dispatch(view.state.tr.setSelection(next))`. This changes the selection only: no `scrollIntoView()` and no Yjs write. Then call `view.focus()`, which uses ProseMirror's `focusPreventScroll`.

### 4.5 NEW `web/src/features/docs/DocDock.tsx`
```tsx
export function DocDockControls(props: {
  showEdit: boolean; editMode: boolean; onToggleEdit: () => void;
  showComments: boolean; commentsOpen: boolean; onToggleComments: () => void;
  editToggleRef: Ref<HTMLButtonElement>; commentsRef: Ref<HTMLButtonElement>;
  size: "sm" | "md";
})
```
- Root: `<div data-doc-controls className="flex items-center gap-0.5">`.
- `{showEdit && <IconButton data-testid="dock-toggle-edit-mode" data-dock-control="edit" ref={editToggleRef} size={size} label={editMode ? "Stop editing (⌘E)" : "Edit (⌘E)"} active={editMode} aria-keyshortcuts="Control+E Meta+E" onClick={onToggleEdit}>{editMode ? <Eye size={16} aria-hidden/> : <Pencil size={16} aria-hidden/>}</IconButton>}`
- `{showComments && <IconButton data-testid="dock-open-comments" data-dock-control="comments" ref={commentsRef} size={size} label="Comments" active={commentsOpen} onClick={onToggleComments}><MessageSquare size={16} aria-hidden/></IconButton>}`

```tsx
export function DocDockBar({ title, children }: { title: string; children: ReactNode })
```
- Host, zero height so mounting never shifts layout: `<div data-testid="doc-dock-host" className="sticky top-0 z-10 h-0 print:hidden">`.
- Bar inside the host: `<div data-testid="doc-dock" data-sticky-band role="group" aria-label="Document controls" className="knot-dock-in absolute inset-x-0 top-0 py-2 bg-bg/80 backdrop-blur border-b border-border">`.
- Row inside the bar: `<div className="measure flex items-center gap-2">`, holding
  - `<span aria-hidden className="min-w-0 flex-1 truncate text-[13px] font-medium text-fg-muted max-sm:hidden">{title || "Untitled"}</span>`
  - `<div className="ml-auto">{children}</div>`

Use `z-10`, **not z-20**. That keeps the dock below the mobile `sidebar-backdrop` (fixed z-20, earlier in the DOM), below `menu-toggle` and the drawer (z-30), and below CommentSidebar and HistoryDrawer (z-40). `role="group"` is deliberate: `toolbar` would promise roving arrow keys, which we don't implement. The blur sits on the bar element only, never on `.doc-shell` or `<main>`.

### 4.6 `web/src/features/docs/DocPage.tsx` (edit)
**All new hooks are declared above `if (!id) return null`.**

State and refs:
- `const sectionRef = useRef<HTMLElement>(null)` on `<section>`.
- `const [actionRowEl, setActionRowEl] = useState<HTMLDivElement | null>(null)`. The existing row `<div className="mt-2 flex flex-wrap …">` gets `ref={setActionRowEl} data-testid="doc-actions"`.
- `actionEditToggleRef`, `actionCommentsRef` (header buttons), `dockEditToggleRef`, `dockCommentsRef` (whichever DocDockControls instance is mounted), `editorHandleRef = useRef<KnotEditorHandle | null>(null)`. Import the handle type with `import type` so the lazy editor chunk is not pulled in eagerly.
- `editModeRef` and `condensedRef`, kept in sync by a layout effect.
- `pendingToggleRef = useRef<{ entering: boolean; anchor: ReadingAnchor | null; fieldHadFocus: boolean; refocusToggle: boolean } | null>(null)`.
- `rescueFocusRef = useRef<"edit" | "comments" | null>(null)`.
- `const [announcement, setAnnouncement] = useState("")`.

`condensed`:
```ts
const condensed = useScrolledPast(actionRowEl, (next) => {
  if (next) return;
  const a = document.activeElement as HTMLElement | null;
  const ctl = a?.closest<HTMLElement>("[data-doc-controls] [data-dock-control]");
  rescueFocusRef.current = (ctl?.dataset.dockControl as "edit" | "comments") ?? null;
});
```
Then a `useLayoutEffect(() => { if (condensed) return; const which = rescueFocusRef.current; rescueFocusRef.current = null; (which === "edit" ? actionEditToggleRef : actionCommentsRef).current?.focus({ preventScroll: true }) /* only if which */ }, [condensed])`.

`toggleEditMode = useCallback(() => { … }, [])`. It reads refs only and runs **before** the state change:
```ts
const section = sectionRef.current;
const scroller = section && findScroller(section);
const prose = section?.querySelector("[data-testid='editor-host'] .ProseMirror");
const active = document.activeElement as HTMLElement | null;
const inPM = !!active?.closest(".ProseMirror");
const inToolbar = !!active?.closest("[data-testid='editor-toolbar']");
const fieldHadFocus = !inPM && !!active?.matches("input, textarea, [contenteditable='true']");
const entering = !editModeRef.current;
pendingToggleRef.current = {
  entering,
  anchor: condensedRef.current && scroller && prose ? captureReadingAnchor(scroller, prose, section!) : null,
  fieldHadFocus,
  refocusToggle: !entering && (inPM || inToolbar || active === document.body || active === null),
};
setEditMode(entering);
setAnnouncement(entering ? "Editing" : "Viewing");
```

Restore and caret, in **one** layout effect. Child layout effects run first, so by now `setEditable` has run and the toolbar is in the DOM:
```ts
useLayoutEffect(() => {
  const p = pendingToggleRef.current; pendingToggleRef.current = null;
  if (!p) return;
  const scroller = sectionRef.current && findScroller(sectionRef.current);
  const cancel = p.anchor && scroller ? holdReadingPosition(scroller, p.anchor, { durationMs: 0 }) : undefined;
  if (p.entering && p.anchor && !p.fieldHadFocus) editorHandleRef.current?.placeCaretAtReadingLine();
  if (!p.entering && p.refocusToggle)
    (condensedRef.current ? dockEditToggleRef.current : null)?.focus({ preventScroll: true })
      ?? actionEditToggleRef.current?.focus({ preventScroll: true });
  return cancel;
}, [editMode]);
```
- Write the last statement as a plain if/else. `focus()` returns `undefined`, so the `??` chain above only illustrates the intent.
- Caret placement and scroll correction are both gated on `p.anchor`, which exists only when condensed. At the top of the document, behaviour stays as today.

Rail hold:
```ts
const railFirstRun = useRef(true);
useLayoutEffect(() => {
  if (railFirstRun.current) { railFirstRun.current = false; return; }
  const s = sectionRef.current; const scroller = s && findScroller(s);
  const prose = s?.querySelector("[data-testid='editor-host'] .ProseMirror");
  if (!condensedRef.current || !scroller || !prose) return;
  const a = captureReadingAnchor(scroller, prose, s!);
  return a ? holdReadingPosition(scroller, a, { durationMs: 250 }) : undefined;
}, [commentSidebarOpen]);
```
This works for every opener: dock, header, float, highlight click and mention. The capture reads the old layout because the `.doc-shell` padding transition has just started from its old computed value. In overlay mode nothing moves, so every delta is 0. If the user wheels or touches, or `CommentThread.scrollToHighlight` (`CommentThread.tsx:210-213`) smooth-scrolls `<main>`, the next frame sees a foreign `scrollTop` and stops.

⌘E listener:
- Move it **below** the `archived` derivation (line 144, still above the early returns).
- `const canToggleEdit = !archived && (effRole === "owner" || effRole === "editor");`
- Effect on `[canToggleEdit]`: `if (!canToggleEdit) return;` then `onKey`: `if (e.repeat || e.isComposing || !isEditModeShortcut(e)) return; e.preventDefault(); toggleEditModeRef.current();`
- Normal bubbling (no capture flag). Keep `toggleEditModeRef` pointing at `toggleEditMode`.

Header row:
- The `toggle-edit-mode` `onClick` becomes `toggleEditMode`. Add `ref={actionEditToggleRef}` and `aria-keyshortcuts="Control+E Meta+E"`. Test id, label and icon are unchanged.
- `open-comments` gets `ref={actionCommentsRef}`. It stays open-only, as today.

Derived values, after `meta`:
```ts
const showEditInDock = canEditDoc && !mdView;
const showCommentsInDock = !archived;
const dockInToolbar = showEditInDock && editMode;
const vp = useViewport(); // declare above the early returns
const dockControls = (
  <DocDockControls showEdit={showEditInDock} editMode={editMode} onToggleEdit={toggleEditMode}
    showComments={showCommentsInDock} commentsOpen={commentSidebarOpen}
    onToggleComments={commentSidebarOpen ? closeCommentSidebar : openCommentSidebar}
    editToggleRef={dockEditToggleRef} commentsRef={dockCommentsRef}
    size={vp === "mobile" ? "md" : "sm"} />
);
```
- Add `closeCommentSidebar` from `useUi`.
- In edit mode with a toolbar, `showCommentsInDock` is always true there, because archived docs never show the toolbar.

Render:
- **Between** the header `<div className="measure mt-3">…</div>` and `{archived && <ArchivedBanner/>}`: `{condensed && !dockInToolbar && (showEditInDock || showCommentsInDock) && <DocDockBar title={meta.title}>{dockControls}</DocDockBar>}`. Its sticky containing block is the `section`, so it stays stuck for the whole document. There is no overflow ancestor between it and `<main>`.
- `KnotEditor` gets `toolbarTrailing={dockInToolbar ? dockControls : undefined}`, `toolbarTrailingVisible={condensed}` and `handleRef={editorHandleRef}`.
- At the end of the section: `<span role="status" aria-live="polite" className="sr-only" data-testid="edit-mode-announcer">{announcement}</span>`.

### 4.7 `web/src/features/editor/KnotEditor.tsx` (edit)
Props:
- New optional props `toolbarTrailing?: ReactNode`, `toolbarTrailingVisible?: boolean`, `handleRef?: MutableRefObject<KnotEditorHandle | null>`.
- Thread them through `KnotEditor` → `EditorBody` → `EditorToolbar` (`trailing`, `trailingVisible`).

`setEditable`:
- Change the effect at lines 322-325 from `useEffect` to **`useLayoutEffect`**. The body stays the same.
- This removes the frame where the toolbar is up but the editor is still read-only. It also means DocPage's restore measures the final layout, including the column handles un-hiding.

Handle:
```ts
useLayoutEffect(() => {
  if (!handleRef || !editor) return;
  handleRef.current = { placeCaretAtReadingLine: () => {
    if (editor.isDestroyed || !editor.isEditable) return;
    const scope = editor.view.dom.closest("[data-testid='doc-page']") ?? document;
    placeCaretAtReadingLine(editor.view, scope);
  } };
  return () => { handleRef.current = null; };
}, [editor, handleRef]);
```
Because it is a layout effect it registers before DocPage's layout effect runs. It is re-registered after an `editorEpoch` remount.

Mod-e: in `editorProps.handleDOMEvents.keydown`, add `if (isEditModeShortcut(event)) return true;` before the existing paste latch. Returning true skips ProseMirror's built-in keydown, and with it Tiptap's `Mod-e` toggleCode. ProseMirror does not stop propagation, so DocPage's window listener still toggles. Leave a comment that points at `@tiptap/extension-code`'s binding.

Float clearance: in `onSelectionUpdate`, replace `document.querySelector("[data-testid='editor-toolbar']")` with
```ts
toolbarBottom: stickyCoverBottom(ed.view.dom.closest("[data-testid='doc-page']") ?? document)
```
`computeAddCommentPos`, `ADD_COMMENT_BUTTON_WIDTH` and its test are **unchanged**. Only the doc comment on `toolbarBottom` changes, to "bottom of the sticky band covering the top (dock or toolbar)".

### 4.8 `web/src/features/editor/EditorToolbar.tsx` (edit)
- Props: `trailing?: ReactNode; trailingVisible?: boolean`.
- The root gets `data-sticky-band`. Everything else stays: `sticky top-0 z-10 py-2 mb-4 bg-bg/80 backdrop-blur border-b`.
- The measure row gets `max-sm:pl-7`.
- After the link button and its popover, as the last child of the row:
```tsx
{trailing && (
  <div data-testid="toolbar-doc-controls"
       className={`ml-auto flex items-center pl-1 border-l border-border transition-opacity duration-150 ease-swift motion-reduce:transition-none ${trailingVisible ? "" : "invisible opacity-0"}`}>
    {trailing}
  </div>
)}
```
- The slot is always laid out while editing. Only `visibility` and `opacity` change, so the row never re-wraps on condense, and `invisible` removes it from the tab order and the accessibility tree.
- **Width budget at the 712px measure:**

  | Part | Width |
  |---|---|
  | 18 buttons × 32 | 576 |
  | 3 separators × 9 | 27 |
  | 20 gaps × 2 | 40 |
  | Gap before the slot | 2 |
  | Slot: 1 + 4 + 28 + 2 + 28 | 63 |
  | **Total** | **708** |

  The hidden file input is `display:none` and is not a flex item. **If the e2e single-row check fails, change the row's `gap-0.5` to `gap-0`, which saves 40px. Do not drop the slot.**

### 4.9 CSS: `web/src/styles/global.css` (edit)
```css
@keyframes knot-dock-in { from { opacity: 0 } to { opacity: 1 } }
.knot-dock-in { animation: knot-dock-in 150ms cubic-bezier(0.16, 1, 0.3, 1); }
```
- The existing global `prefers-reduced-motion` rule (`global.css:30`) already cuts this to 0.01ms.
- **No changes** to `.doc-shell` or `<main>` in `layout.css`.

### 4.10 `CHANGELOG.md` (edit)
Under `[Unreleased]`:
- **Added:** document controls (Edit, Comments) stay reachable while scrolled down.
- **Fixed:**
  - Switching edit mode, or opening the comment rail, no longer moves the text you're reading.
  - Entering edit mode places the caret where you were reading.
  - ⌘E while typing no longer toggles inline code.
  - ⌘E does nothing on archived pages.
  - On phones, the menu button no longer covers Bold in the stuck toolbar.
- **Changed:** ⌘E/Ctrl+E is no longer the inline-code shortcut. Use the toolbar button or backticks.

### Test ids (all new, none reused)
| id | element |
|---|---|
| `doc-actions` | existing header action row |
| `doc-dock-host`, `doc-dock` | view-mode sticky host and bar (mounted only while condensed) |
| `dock-toggle-edit-mode`, `dock-open-comments` | DocDockControls buttons. Rendered in either the overlay dock or the toolbar slot, never both |
| `toolbar-doc-controls` | reserved slot in EditorToolbar (edit mode only) |
| `edit-mode-announcer` | sr-only live region |

`toggle-edit-mode`, `open-comments`, `editor-toolbar` and `add-comment-float` are unchanged.

## 5. Tests

Run web tests with `cd web && NODE_OPTIONS=--no-experimental-webstorage npx pnpm test` (Node 26 quirk).

### Unit (vitest)
- `editMode.test.ts` (extend): `isEditModeShortcut` returns true for Ctrl+E and Meta+E (also "E" uppercase), and false with Shift, with Alt, and for other keys.
- `useScrolledPast.test.ts` (new):
  - `isScrolledPast` is true above the root, false below it, and false when intersecting.
  - The hook returns false with no IntersectionObserver.
  - With an IO stub, `onBeforeChange` is called before the state flips.
- `readingAnchor.test.ts` (new):
  - `firstIndexBelow`: empty list, all above (-1), exact boundary, 1000 items.
  - `stickyCoverBottom` skips `visibility:hidden` and zero-height bands, and returns null with none.
  - `captureReadingAnchor` picks the first block below the band.
  - `holdReadingPosition`, with stubbed rects and a fake rAF:
    - adds the delta to `scrollTop`
    - does nothing under 1px
    - stops when `scrollTop` is changed externally
    - stops when the anchor is disconnected
    - `durationMs: 0` runs exactly one rAF
    - cancel works
- `readingCaret.test.ts` (new): `chooseCaretStrategy` keeps a visible text selection; returns reading-line when off-screen, under the band, or not a TextSelection.
- `DocDock.test.tsx` (new):
  - labels and `aria-pressed` follow `editMode` and `commentsOpen`
  - `showEdit=false` renders no edit button
  - `aria-keyshortcuts` is present
  - `size` is passed through
- `EditorToolbar.trailing.test.tsx` (new): the slot renders with `trailing`; it has the `invisible` class when `trailingVisible=false`; the root has `data-sticky-band`. The existing `EditorToolbar.active` and `.link` tests need no change.
- `DocPage.dock.test.tsx` (new): stub `IntersectionObserver` so it fires a scrolled-past entry inside `act()`. Mock KnotEditor so it renders `props.toolbarTrailing` inside `<div data-testid="mock-trailing">`.
  - Editor in view mode: `doc-dock` contains `dock-toggle-edit-mode` and `dock-open-comments`. Clicking the dock toggle sets `toggle-edit-mode` `aria-pressed="true"`, removes `doc-dock`, and makes `mock-trailing` contain `dock-toggle-edit-mode`.
  - Viewer: the dock has only `dock-open-comments`.
  - Archived: no `doc-dock`.
  - Markdown view: no `dock-toggle-edit-mode`.
  - `edit-mode-announcer` is empty on mount and says "Editing" after a toggle.
  - Without the IO stub there is no dock (guards the existing tests).
- `DocPage.archived.test.tsx` (extend): an archived doc with editor role. `fireEvent.keyDown(window, { key: "e", ctrlKey: true })` leaves `sessionStorage["knot.editMode.d1"]` not `"1"` and the mock's `data-edit-mode="false"`.
- `addCommentPos.test.ts`: **no change**, because the signature is unchanged.

### E2E (Playwright): NEW `e2e/flows/doc-dock.spec.ts`
- **Setup.** Owner via the API in `beforeAll`, as in `table-column-resize.spec.ts`. Keep the default storageState (`defaultOn`). Create a blank doc, then `doc-import-md-input.setInputFiles` with 150 numbered paragraphs, each about two lines long. Wait for `status-dot` to report connected, and switch to view mode with `toggle-edit-mode`.
- `scrollMid(page)` sets `main.scrollTop` to roughly the middle. `paraTop(n)` returns `.ProseMirror > p:nth-child(n)` `boundingBox().y`.
1. **Dock appears.** After `scrollMid`, `doc-dock` is visible and in the viewport, and `toggle-edit-mode` is not in the viewport.
2. **Enter edit without a jump.**
   - Record the first paragraph fully below `doc-dock`, call it P, and its y.
   - Click `dock-toggle-edit-mode`. Expect `editor-toolbar` in the viewport, `toolbar-doc-controls` visible, `toggle-edit-mode` with `aria-pressed="true"`, and P's y within ±2px.
   - Type "Z". Expect P's text to contain "Z", and `main.scrollTop` unchanged within ±2px.
3. **Leave with ⌘E.**
   - With the caret in the editor, `keyboard.press("ControlOrMeta+e")`.
   - Expect `aria-pressed="false"`, `.ProseMirror code` count 0, P's y within ±2px, and `document.activeElement.dataset.testid === "dock-toggle-edit-mode"`.
4. **Click then ⌘E keeps the click point** (this is the gate for the "keep" path).
   - View mode, mid-doc: click inside the text of paragraph P+5, then ⌘E, then type "Q". Expect "Q" in P+5.
   - If this fails in Chromium, read-only click selections are not tracked. Restrict "keep" to non-empty selections and update this test to expect "Q" in P.
5. **Float clears the dock.** In view mode, double-click a word in P. `add-comment-float`'s box top is at least `doc-dock`'s box bottom.
6. **Rail hold.** At 1280×720 the column is 1020px, so the rail insets the page.
   - Record P's y, click `dock-open-comments`, and wait for CommentSidebar plus 400ms. P's y is within ±2px and `dock-open-comments` has `aria-pressed="true"`.
   - Close with the same button; P's y is again within ±2px.
7. **Toolbar width budget.** At the top in edit mode, with no table selected, the `editor-toolbar` box height is at most 50, i.e. a single row. `toolbar-doc-controls` is hidden (`toBeHidden`).
8. **Back at the top.** `main.scrollTop = 0`, so `doc-dock` has count 0.
9. **Mobile** (`test.describe` with `test.use({ viewport: { width: 375, height: 667 } })`):
   - With the dock shown, `main.scrollWidth - clientWidth <= 1`.
   - The `dock-toggle-edit-mode` box doesn't intersect the `menu-toggle` box. Close the drawer first via `sidebar-backdrop` if it is open, as `mobile.spec.ts` does.
   - After entering edit mode mid-doc, the `toolbar-bold` box left is at least `menu-toggle`'s box right.
10. **Roles.**
    - Viewer, as a second user with a viewer grant (follow `role-gating.spec.ts`): the dock shows `dock-open-comments` only.
    - Archived: in `archive.spec.ts`, after archiving, add `await expect(page.getByTestId("doc-dock")).toHaveCount(0)`. Scroll first if the doc is long enough; otherwise the assertion holds trivially and still documents the intent.

### Existing specs: none expected to need changes
`new-doc-edit-mode`, `table-column-resize`, `mobile` (header checks at scrollTop 0), `comment-anchors` (float id unchanged), `comments*`, `doc-width` and `sidebar-resize` all toggle or act near the top of the document, where nothing is condensed. Run the full suite. Follow the throwaway-Postgres setup from memory notes so the user's dev database is not touched.

## 6. Risks

- **Focus timing.** The view-mode dock unmounts in the same commit that flips `condensed`. Focus rescue is therefore decided in the IO callback before `setState`, and in `toggleEditMode` before `setEditMode`, never after the fact. Clicking 👁 in Safari leaves `activeElement` on `<body>`, which the `refocusToggle` condition covers.
- **Toolbar budget (708/712px).** The width is deterministic: buttons are fixed `h-8 min-w-8` with 15px icons. Any future toolbar button will trip e2e test 7, which is the intended guard. The fallback is `gap-0`. Inside a table, or with the rail inset or below 712px, the toolbar already wraps today. The slot is reserved, so a wrap changes how it looks but nothing jumps.
- **Mod-e.** ⌘E is no longer the inline-code shortcut (changelog). The predicate refuses Shift and Alt, so ⌘⇧E and others are untouched. ⌘⇧F (width toggle) is unaffected.
- **Stacking.**
  - The dock and toolbar are both z-10 and never stuck at the same time.
  - `add-comment-float` (z-20, no stacking context on `editor-host`) can paint over the dock, but it is positioned below the band.
  - Node views with their own `z-index > 10` (check the Excalidraw preview) could paint over either band while scrolling under it. That is existing toolbar behaviour; verify by eye.
- **Forbidden styles.** No transform, filter, backdrop-filter, contain or will-change on `.doc-shell` or `<main>`. Blur is on `doc-dock` only. The dock host must stay a direct child of `section.doc-shell`.
- **jsdom.** It has no IntersectionObserver, so the hook feature-detects. Only the new tests stub it.
- **Collaboration.** A remote update can replace the anchor block between capture and restore. `isConnected` then turns the restore into a no-op, which is today's behaviour. The caret transaction changes only the selection: no Yjs write, only an awareness cursor update.
- **Rail hold.** It is capped at 250ms and stops on the first foreign scroll, so it cannot fight CommentThread's smooth `scrollIntoView` or the user. If the `.doc-shell` transition is ever removed, the capture already sees the new layout and the hold becomes a harmless no-op.
- **iOS keyboard.** With the keyboard open, Safari can pan the visual viewport so the stuck band is off-screen. 👁 is reachable again after dismissing the keyboard. Test by hand on a device; it is not automated.
- **Keeping a selection.** If the user had text selected and presses Edit, the selection is kept, so the next keystroke replaces it. This is standard editor behaviour and undo works.
- **`editorEpoch` reconnect while scrolled.** "Connecting…" briefly shows no toolbar, so the slot controls vanish until the editor returns. The handle is re-registered on remount.

## 7. Out of scope / follow-ups

1. **"Edit" in the selection bubble** (`[Add comment | Edit]`, owner/editor, view mode, `preferBelow` on coarse pointers). Today, select text and then dock Edit gives the same result, because the selection is kept.
2. **"Back to top / Back to where you were"** in the dock, or a "More" menu, for History, Export, Share, Template and Markdown mid-document.
3. **`scrollThreshold`/`scrollMargin` from the measured band height**, so arrow-key navigation near the top never hides the caret under the stuck toolbar. This is an existing issue.
4. **A visible "Done" text label** on the toolbar slot's toggle. It needs width the 712px budget doesn't have.
5. **Caret placement when toggling from the visible header at the top of the document** (currently the old behaviour on purpose).
6. **iOS `visualViewport` handling** for the stuck band while the keyboard is open.

## 8. Implementation notes (deviations found while building it)

Each of these came from an e2e failure or from the review panel and replaced the spec's text above.

- **Keeping a click point.** In view mode ProseMirror does not track a plain click (the view is not focused), so `placeCaretAtReadingLine` reads the browser's own selection (`window.getSelection()`) first. "Click where you want to edit, then ⌘E" keeps the caret at the click. §4.4 step 2 relied on `view.state.selection` only, which never saw the click.
- **Reading anchor.** `captureReadingAnchor` takes the first block that *starts* below the band when it is on screen. The block straddling the band is used only when it fills the view. Pinning the straddling block's top let the lines being read drift by about 27px when the comment rail re-wrapped it.
- **Probe.** The probe steps down in 6px increments, up to 160px, until it finds a line that is fully below the band. It starts 16px into the text block under the column's middle, never at the editor root's left edge. A position at the very start of a wrapped line is also the end of the line above, and Chrome drew the caret there, under the band. Probing from the root's left edge also missed indented text (lists, quotes, cells) and centred text (wide mode). The spec's single re-probe at `line.bottom + 4` landed in the gap between paragraphs.
- **Taller band.** `holdReadingPosition` takes `scope` and measures the band again on each correction. An anchor that would now sit under the band is pinned just below it, using `max(top, band bottom)`. This covers a phone, where the toolbar wraps to several rows but the dock has one. The dock's row is `min-h-8` (49px), so on desktop the band height does not change between modes.
- **Caret in a table.** The toolbar adds a row of table buttons after the caret lands in a table. One frame later, `KnotEditorHandle.revealCaretBelowBand()` scrolls the caret's line out from under the band.
- **Focus rules.** Entering edit mode moves focus into the editor only if focus was in the text, the toolbar, a toggle, or nowhere. Focus stays put in the title, the comment composer, the comment rail and the history drawer. Leaving edit mode while condensed focuses the dock's toggle if there is one, and otherwise leaves focus alone (the Markdown view has no dock toggle).
- **⌘E is always claimed for editor+.** The listener calls `preventDefault` for every matching chord, including repeats, IME composition and archived pages. It only skips the toggle in those cases, so the browser's own Ctrl+E (address bar or search) never fires there. Viewers keep the browser default, as before.
- **Not done from §5:** the viewer e2e (covered by `DocPage.dock.test.tsx`) and the archived assertion in `archive.spec.ts` (covered by `DocPage.dock.test.tsx` and `DocPage.archived.test.tsx`).

Per the user's standing instruction, the implementer stages the changes and does not commit or push; the user commits.
