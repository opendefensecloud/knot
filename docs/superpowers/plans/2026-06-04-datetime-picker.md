# Inline Date/Time Picker (Plan 34)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task.

**Goal:** Typing `//` in the editor opens a date+time picker; selecting a value inserts a styled chip that round-trips through markdown and can be re-edited by clicking it.

**Architecture:**
- Tiptap inline atom node `datetime` with a single attr `iso` (RFC 3339 UTC, e.g. `2026-06-04T14:00:00Z`). Renders local-time human label; tooltip shows ISO.
- Suggestion trigger `//` mirrors the `@` mention plugin already in `MentionExtension.ts`.
- Picker UI: lightweight popover combining `react-day-picker` (already small, no other deps) for the date and a native `<input type="time">` for the clock. Hidden behind dynamic import — only paid when the popover opens.
- Markdown serialization uses the existing `knot://` sentinel pipeline: chip ↔ `[Jun 4, 2026 2:00 PM](knot://time/2026-06-04T14:00:00Z)`. Reuses `rewrite_link_urls` for export/import normalization (relative paths in zips not applicable — datetimes are not files).
- Timezone: store UTC; render in browser local time; never persist the local string. Users on different TZs see their own.

---

## Tasks

### T1: Schema — datetime node
- `tools/schema.json`: add inline atom `datetime` with attrs `{ "iso": { "type": "string" } }`. Regen.
- knot-crdt: nothing extra — yrs sees it as a regular inline node.

### T2: knot-markdown round-trip
- `from_markdown.rs`: when a `Tag::Link` with `dest_url` matching `knot://time/<iso>` is encountered, emit a `datetime` node with `iso = <iso>` and skip the link text (it's purely a render hint).
- `to_markdown.rs`: a `datetime` node emits `[<local-format>](knot://time/<iso>)`. Local-format is irrelevant for the round-trip (the ISO is the source of truth) but is the human-readable text in a plain-markdown export.
- Fixture: `datetime.md` round-trip test.

### T3: Tiptap node
- `web/src/features/editor/DateTimeExtension.ts`: `Node.create({ name: "datetime", inline: true, group: "inline", atom: true, ... })`. NodeView renders a `<span class="datetime-chip">` with the local-time label, click-to-edit handler.
- Register in `extensions.ts`.

### T4: Suggestion plugin
- New file `web/src/features/editor/DateTimeSuggestion.ts`: copies the structure of the `@` mention suggestion. Trigger char `/`, second-char gate (only fire if the previous char is also `/` and the user typed both within ~200ms — avoids hijacking single-`/` for future slash commands). When triggered, opens the date+time popover anchored at the cursor.
- On select: replace the trigger range with a `datetime` node carrying the ISO.

### T5: Picker UI component
- `web/src/components/DateTimePopover.tsx`: lazy-imports `react-day-picker`, renders calendar + `<input type="time">` + Clear/Apply buttons. Returns ISO UTC.
- Tab/Esc handling, focus-trap inside popover. a11y label "Choose date and time".

### T6: Edit affordance
- Click on a `datetime` chip in the editor reopens the popover prefilled with `iso`. Save replaces the node's `iso` attr in a single transaction.

### T7: Toolbar entry point
- Add a `Calendar` icon button in `EditorToolbar.tsx` next to the link/table buttons, opens the same popover and inserts a chip at the current selection. Power users can type `//`; everyone else can click.

### T8: e2e
- `web/tests/datetime.spec.ts`: type `//`, popover opens, pick a date+time, chip renders. Click chip, popover reopens prefilled. Export markdown, verify `knot://time/<iso>` sentinel. Import that markdown back, chip survives.

### T9: Outcome doc

---

## Open questions resolved before T1

- **Single `/` collision with future slash commands.** Decision: double-`/` trigger, with a settings escape hatch if we ever want single-`/`.
- **What if `react-day-picker` is too heavy?** Lazy import means it's not in the main bundle. If it still bloats the editor chunk, fall back to native `<input type="datetime-local">` (uglier but ~0 KB).
- **Past dates allowed?** Yes — users will paste meeting recaps. No client-side validation.

---
