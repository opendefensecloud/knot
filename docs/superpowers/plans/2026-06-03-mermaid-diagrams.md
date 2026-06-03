# Mermaid Diagrams Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development or executing-plans.

**Goal:** Render Mermaid diagrams inline in the editor by detecting fenced code blocks with `language === "mermaid"` and replacing their visual with the rendered SVG via a Tiptap NodeView. Source remains editable via a toggle. Markdown round-trip is free because it's still a code block on the wire.

**Architecture:** A new Tiptap extension (`MermaidCodeBlock`) replaces StarterKit's `codeBlock` with a subclass that adds a NodeView. The NodeView shows either the rendered SVG (default when `language=="mermaid"`) or a textarea-style editor for the source (toggle). Mermaid is dynamically imported inside the NodeView so it lands in its own chunk and only loads when the user inserts/views a diagram. Editor toolbar gains an "Insert diagram" `Network` icon button. ProseMirror schema is unchanged.

**Tech Stack:** `mermaid@^11` (dynamic import), existing Tiptap + Yjs stack.

---

## Task 1: Install mermaid + MermaidCodeBlock extension

**Files:**
- Modify: `web/package.json` (add `mermaid`)
- Create: `web/src/features/editor/nodes/MermaidCodeBlock.tsx`
- Modify: `web/src/features/editor/extensions.ts`

The extension extends Tiptap's CodeBlock, keeping language attr, but adds a NodeView for the `mermaid` language. NodeView state: `mode: "preview" | "source"`, `svg`, `error`. On mount + on attr/text change, dynamically import mermaid and call `mermaid.render()`.

## Task 2: Editor toolbar "Insert diagram" button

**Files:**
- Modify: `web/src/features/editor/EditorToolbar.tsx`

Add a `Network` Lucide icon button between the Sep after `Code2` and the link button. `data-testid="toolbar-mermaid"`. On click: insert a code block with `language: "mermaid"` and a placeholder graph.

## Task 3: NodeView styling + reduced motion

**Files:**
- Modify: `web/src/styles/prose.css`

Container card with `bg-surface`, border, rounded; toolbar inside the card with mode toggle (`Eye`/`Pencil`). Error state renders inside the same card with `text-destructive` and the raw source below.

## Task 4: Playwright e2e

**Files:**
- Create: `e2e/flows/mermaid.spec.ts`

Setup → new doc → click `toolbar-mermaid` → assert SVG renders → toggle to source → edit → toggle back → assert SVG re-renders.

## Task 5: Outcome doc + README row

**Files:**
- Create: `docs/superpowers/research/2026-06-03-mermaid-outcome.md`
- Modify: `docs/superpowers/README.md`

---

## Carryforward

- Excalidraw (separate plan) — custom NodeView, Yjs sub-document for board state, markdown round-trip via sentinel-fenced JSON or `data:image/svg+xml` snapshot in blob storage.
- Mermaid theming for dark mode — `mermaid.initialize({ theme: ... })` based on `useUi.theme`.
- Configurable max-height + zoom for very large diagrams.
