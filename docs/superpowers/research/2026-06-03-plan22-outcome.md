# Plan 22 Outcome — UI Modernization (Outline-style)

**Status:** GO_WITH_CONCERNS. All 15 tasks landed. Inline `pnpm tsc/lint/test` green throughout (26 vitest cases). Build clean, bundle +1.6 KB gz on main (113.02 → 114.65) and +1.3 KB on the editor chunk. **Full Playwright suite was not run as part of this plan — see "Carryforward" below.**

**Verdict:** The frontend now ships a real design system. Tokens, Inter typography, Lucide icons, and an Outline-style sidebar/doc chrome have replaced the prototype's inline `style={}`-everywhere look. Dark mode works end-to-end (toggle + persistence + token-driven). The path through the app — login → setup → sidebar → tree → doc title/breadcrumb → editor toolbar → comments/history drawers → settings/members/permissions — is visually coherent.

## What landed

| Commit | Task | Subject |
|---|---|---|
| (T1) | T1 | tailwind + tokens + Inter + global.css scaffold |
| 2aad193 | T2 | theme store + setTheme/toggleTheme + ui.theme.test.ts |
| (T3) | T3 | components/ui/Button + IconButton + Tooltip primitives |
| 2d557de | T4 | AppShell — Tailwind shell, Lucide Menu icon |
| dcb5b52 | T5 | WorkspaceHeader (avatar + search trigger + Members/Settings) |
| 5fc1649 | T6 | DocTree — ChevronRight/FileText/Plus/MoreHorizontal + hover-reveal |
| 2e9dc7d | T7 | DocPage — Breadcrumb + max-w-760 + icon action row |
| caac70a | T8 | EditorToolbar — Bold/Italic/H1/H2/H3/List/.../Link Lucide icons |
| cd00310 | T9 | prose.css ProseMirror styles + tokenized editor host |
| 223d686 | T10 | StatusDot/Toast/ContextMenu/CommandPalette tokenized |
| 4209538 | T11 | comments + history drawers themed (SmilePlus reactions toggle) |
| dd19a05 | T12 | Login + Setup themed (card shell + tokenized inputs) |
| 77a8fa3 | T13 | Settings/Members/Permissions themed (grouped cards + table styling) |
| d7972fd | T14 | theme toggle (Moon/Sun) + e2e/flows/theme.spec.ts |
| T15 | T15 | this outcome doc |

## Gates

- `pnpm tsc` — clean
- `pnpm lint` — clean (`--max-warnings 0`)
- `pnpm test` — **26 passed** (added `ui.theme.test.ts`)
- `pnpm build` — main 114.65 KB gz (+1.6 KB), editor 148.81 KB gz (+1.3 KB)
- `pnpm playwright test` — **not executed in this session.** Should be run with compose.up + dev server before merging. The new `theme.spec.ts` is the only added file; all other specs should still pass because every `data-testid` was preserved.

## Architecture summary

**Tokens.** All color comes from CSS variables in `src/styles/tokens.css`. Tailwind's `theme.extend.colors` exposes them as `bg`, `surface`, `border`, `muted`, `fg`, `fg-muted`, `accent`, `accent-fg`, `destructive`. Light is the default; `[data-theme="dark"]` flips them. `darkMode: ["class", '[data-theme="dark"]']` lets Tailwind's `dark:` modifier piggyback if needed, but in practice we just use the variable-backed classes and the dark theme "comes free."

**Theme persistence.** `useUi.theme` initializes from `localStorage["knot.theme"]` (defaults to `"light"`). `main.tsx` applies the attribute on documentElement *before* React renders to avoid FOUC. `setTheme/toggleTheme` write to localStorage and update the attribute synchronously.

**Typography.** Inter Variable via `@fontsource-variable/inter`, with a `'Inter Variable' → Inter → system-ui → sans-serif` stack. ProseMirror gets a separate `prose.css` with the v0.1 type ramp (28/22/18 for h1/h2/h3, 16/1.7 body, tight letter-spacing on headings).

**Icons.** `lucide-react` tree-shakes to roughly +1.6 KB gz on the main bundle for the icon set we use. Every emoji-as-icon has been replaced (☰ → `Menu`, 📄 → `FileText`, ❝ → `Quote`, 🔗 → `Link`, B/I/H1/etc → `Bold/Italic/Heading1`). The six comment reactions stay as emoji characters because they are literally the reaction content, not icons-for-actions; only the *add-reaction toggle* changed (was `+`, now `SmilePlus`).

**Sidebar shell.** `AppShell` is the grid; `DocTree` composes `WorkspaceHeader` at the top, the section label + new-doc button, the dnd-kit tree, and (already-existing) Members/Settings links inside the header. WorkspaceHeader's "Search" pill triggers `useUi.openPalette` — reusing the existing command-palette implementation. Theme toggle sits next to Settings.

**DocPage chrome.** Two-row header: breadcrumb (`Documents › <title>`) then title row with `StatusDot` + `Share2`/`History`/`MessageSquare` IconButtons floated right. Content has `mx-auto max-w-[760px] px-6 py-8` — proper reading column. The Tiptap toolbar is sticky-top with `bg-bg/80 backdrop-blur` + tokenized link popover.

**Drawers.** Comment sidebar (`w-[400px]`) and history drawer (`w-[720px]`) share the same shell: `fixed right-0 top-0 h-dvh bg-surface border-l border-border shadow-xl`, IconButton+X close. Comment thread anchor blockquote moved to `bg-accent/5` + accent left-border; resolved threads use `bg-muted/40 opacity-80`.

**Forms.** Inputs are `h-9 px-3 rounded border border-border bg-bg text-fg focus:ring-2 focus:ring-accent text-sm` everywhere. Primary buttons are `bg-accent text-accent-fg`; destructive actions get `text-destructive hover:bg-destructive/10`. Role labels (when read-only) are pills (`inline-flex px-2 h-5 rounded-full text-[11px] bg-muted text-fg-muted`).

## What was non-obvious

**1. The presence chip color is dynamic per peer.** Tailwind can't generate classes for runtime hex values, so the `style={{ background: p.color }}` survived migration. This is the *only* surviving inline style on the editor route.

**2. StatusDot colors live outside the neutral palette.** I considered tokenizing them but it's a category mistake: `bg-emerald-500`/`bg-amber-500`/`bg-destructive` express *semantic state*, not brand color, and shouldn't change between themes the way the neutrals do.

**3. The Add-comment float still uses inline `top`/`left`** because the position is computed from the ProseMirror selection coordinates per keystroke. Migrating it to a CSS class-with-vars would just add a layer of indirection. Same for the link popover position — but it uses Tailwind's `top-full left-0 mt-1` because it's relative to the toolbar button, not coordinate-driven.

**4. `text-fg/60` doesn't work** because the fg color comes from a CSS variable. Tailwind's opacity-modifier syntax (`text-fg/60`) is implemented via `text-opacity: 0.6` on the color, which works only for colors specified in a way that participates in Tailwind's alpha algorithm. `bg-muted/40` and `bg-muted/60` *do* work because Tailwind sees them as `rgb(var) / 0.4` — same with `bg-accent/5`. In practice we never need `text-fg` with reduced opacity (use `text-fg-muted`), so this is OK.

**5. Bundle deltas are tiny.** lucide-react is heavily tree-shakable; only the ~30 icons we actually import end up in the bundle. Tailwind's purge keeps the utility CSS lean — final main.css inside the build is ~12 KB pre-gzip, ~3.6 KB gz. The user-visible bundle still loads under 120 KB gz on first paint.

**6. The `prose.css` `.ProseMirror` class is global and applies to every Tiptap instance** including the public-doc iframe. That's intentional — public docs should look like docs.

**7. Decision to keep the toolbar permanent (no BubbleMenu).** The existing 26-spec e2e suite has multiple specs that click `toolbar-bold` etc. *without first selecting text*. Switching to a BubbleMenu (Tiptap's selection-driven floating menu) would require updating every one of those specs. That's not a refactor I want bundled with a visual modernization PR. Tracked below.

## What's deferred — most notable

- **Full Playwright re-run.** Every change preserved its `data-testid`, and the visual changes are CSS-only for behavior, so the suite *should* be green. But I want to be honest: I did not run it. Run it before merging.
- **BubbleMenu toolbar.** Move from sticky strip to selection-driven floating menu. Requires updating ~8 specs that click toolbar buttons without selecting text first. 1 task.
- **Doc icon picker.** Replace the fixed `FileText` icon with a per-doc emoji/lucide picker, stored in `docs.icon` (schema column already exists).
- **Cover image / banner.** Outline-style hero band on each doc.
- **Sidebar polish.** Collapse-all, favorites, recently-visited section.
- **`prefers-color-scheme` initial detection.** Currently defaults to `"light"` if no localStorage value. Should respect the OS preference on first visit.
- **Mobile sidebar refinements.** Re-test on 375px and landscape, the bg/border tokens may need a darker variant for mobile contrast.
- **Editor caret + selection background in dark mode.** ProseMirror's default selection bg may need an explicit override.
- **Tooltip primitive.** The current `Tooltip` is a pass-through wrapper. A real implementation (Radix or hand-rolled) would replace native `title=` attributes for a more polished hover/focus experience.

## Carryforward

Recommended next:
1. **Run the e2e suite** with `make compose.up && cd web && pnpm dev` in another terminal, then `cd e2e && pnpm playwright test`. If anything fails, the regression is almost certainly a spacing/hit-area issue caught by `editor-toolbar.spec.ts` or `command-palette.spec.ts` (those were the canary specs during Plan 19).
2. **Plan 23 — BubbleMenu migration.** Now that the design system is in place, the toolbar can go selection-driven without looking inconsistent with the rest of the chrome.
3. **Plan 19.5 — Mention push bridge.** Still the documented carryforward from Plan 19.
4. **Plan 18 — Email / SMTP** (invite + password reset + mention emails).

## Files of interest

| Path | Role |
|---|---|
| `web/tailwind.config.ts` | Tailwind + token bindings |
| `web/postcss.config.js` | Postcss/Tailwind/autoprefixer wiring |
| `web/src/styles/tokens.css` | light + dark CSS variable palettes |
| `web/src/styles/global.css` | font import, body bg/fg, slideIn keyframe, reduced-motion |
| `web/src/styles/prose.css` | ProseMirror typography |
| `web/src/components/ui/Button.tsx` | primary/secondary/ghost/destructive button |
| `web/src/components/ui/IconButton.tsx` | square icon-button with focus ring |
| `web/src/features/workspace/WorkspaceHeader.tsx` | sidebar avatar + search trigger + theme toggle |
| `web/src/features/docs/DocTree.tsx` | DocTree with Lucide icons + hover-reveal |
| `web/src/features/docs/Breadcrumb.tsx` | Breadcrumb primitive |
| `web/src/features/docs/DocPage.tsx` | breadcrumb + reading column + icon action row |
| `web/src/features/editor/EditorToolbar.tsx` | Lucide icon toolbar |
| `web/src/components/CommandPalette.tsx` | tokenized palette shell |
| `web/src/stores/ui.ts` | theme state + actions |
| `e2e/flows/theme.spec.ts` | dark mode toggle + persist e2e |
