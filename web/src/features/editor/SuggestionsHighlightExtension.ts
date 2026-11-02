/**
 * SuggestionsHighlightExtension — paints pending change suggestions in the
 * page, modelled on CommentsHighlightExtension.
 *
 *   - A replace or delete gets a blue inline tint over its range
 *     (`.suggestion-highlight[data-suggestion-id]`, `--active` when focused).
 *   - An insertion gets a small "+" widget at its point
 *     (`.suggestion-insert-marker[data-suggestion-id]`). The "+" is drawn by
 *     CSS and the widget has no text node, so it never shows up in
 *     `textBetween`, copy or an e2e `docText`.
 *   - The range in the open composer (the draft) keeps a selection-style
 *     tint, or a marker when it is an insertion point, so the user can see
 *     what they are changing. It carries no suggestion id.
 *   - Like classifySuggestion, a range that collapsed or spans blocks, or a
 *     point outside a textblock, draws nothing, so the page and the card
 *     agree on what is still there.
 *
 * Nothing here writes to the document: decorations are view-only, so pending
 * suggestions never enter the Y.Doc.
 *
 * The suggestion list, the active id and the draft are pushed in from React
 * via `editor.extensionStorage.suggestionsHighlight`, followed by
 * `refreshSuggestionHighlights`. The plugin also rebuilds on every peer
 * change and maps through local ones, so the highlight moves with the
 * anchored text.
 * Clicking a highlight or marker calls `storage.onSelect(id)`; on text that
 * also carries a comment, highlightClick decides which one the click is for.
 */

import { Extension, type Editor } from "@tiptap/core";
import type { Node as ProseMirrorNode } from "@tiptap/pm/model";
import { Plugin, PluginKey, type Transaction } from "@tiptap/pm/state";
import { Decoration, DecorationSet } from "@tiptap/pm/view";
import { ySyncPluginKey } from "@tiptap/y-tiptap";
import type * as Y from "yjs";

import type { SuggestionKind } from "../../lib/suggestions.api";
import { decodeSuggestionRange } from "../suggestions/anchor";
import { highlightAt } from "./highlightClick";

/** The part of a suggestion the highlight needs. */
export type HighlightedSuggestion = {
  id: string;
  kind: SuggestionKind;
  position_y: string;
  position_y_end: string;
};

/**
 * The range the open composer is about. `kind` says what was selected: an
 * insertion point draws a marker, while a replace whose text has since been
 * deleted draws nothing rather than passing for an insertion.
 */
export type DraftRange = { kind: "replace" | "insert"; position_y: string; position_y_end: string };

export type SuggestionsHighlightOptions = {
  doc: Y.Doc;
  suggestions: HighlightedSuggestion[];
  activeSuggestionId: string | null;
  draft: DraftRange | null;
  onSelect: (id: string) => void;
};

// Storage shape held on the extension. React updates this object via
// `editor.extensionStorage.suggestionsHighlight.X = ...` and then calls
// refreshSuggestionHighlights to trigger a re-decoration.
type SuggestionsHighlightStorage = {
  suggestions: HighlightedSuggestion[];
  activeSuggestionId: string | null;
  draft: DraftRange | null;
  doc: Y.Doc | null;
  onSelect: ((id: string) => void) | null;
};

declare module "@tiptap/core" {
  // See CommentsHighlightExtension: React reaches into this slot directly.
  interface Storage {
    suggestionsHighlight: SuggestionsHighlightStorage;
  }
}

const PLUGIN_KEY = new PluginKey<DecorationSet>("suggestionsHighlight");

export const SuggestionsHighlightExtension = Extension.create<SuggestionsHighlightOptions, SuggestionsHighlightStorage>({
  name: "suggestionsHighlight",

  addOptions() {
    return {
      doc: null as unknown as Y.Doc,
      suggestions: [],
      activeSuggestionId: null,
      draft: null,
      onSelect: () => {},
    };
  },

  // Seeded from the options here rather than in onCreate: Tiptap 3 emits
  // "create" on a timeout, so an onCreate copy would land after React's
  // first push into storage and overwrite it with the empty defaults.
  addStorage() {
    return {
      suggestions: this.options.suggestions,
      activeSuggestionId: this.options.activeSuggestionId,
      draft: this.options.draft,
      doc: this.options.doc,
      onSelect: this.options.onSelect,
    };
  },

  addProseMirrorPlugins() {
    const storage = this.storage;
    return [
      new Plugin<DecorationSet>({
        key: PLUGIN_KEY,
        state: {
          init: (_config, state) => buildDecorations(state.doc, storage),
          apply: (tr, oldSet, _oldState, newState) => {
            const refresh = tr.getMeta(PLUGIN_KEY) as unknown;
            if (refresh === "refresh" || (tr.docChanged && isRemoteSync(tr))) {
              return buildDecorations(newState.doc, storage);
            }
            // A local edit reaches the Y.Doc only after this state is applied
            // (ySync writes it in its view update), so decoding now would read
            // the new document against the old mapping and paint the wrong
            // characters. Map instead; the Y.Doc "update" that follows
            // triggers a refresh (KnotEditor) that decodes afresh.
            return oldSet.map(tr.mapping, tr.doc);
          },
        },
        props: {
          decorations: (state) => PLUGIN_KEY.getState(state) ?? DecorationSet.empty,
          handleClickOn: (view, _pos, _node, _nodePos, event) => {
            // Shared with the comment highlights, which run first, so a click
            // on text under both goes to the more specific one.
            const hit = highlightAt(view.dom, event.target);
            if (hit?.kind !== "suggestion") return false;
            storage.onSelect?.(hit.id);
            return true;
          },
        },
      }),
    ];
  },
});

/** True for a transaction y-tiptap dispatched to mirror a Y.Doc change (a
 *  peer's edit, or a Yjs undo/redo); its mapping is already current. */
function isRemoteSync(tr: Transaction): boolean {
  const meta = tr.getMeta(ySyncPluginKey) as { isChangeOrigin?: boolean } | undefined;
  return meta?.isChangeOrigin === true;
}

/**
 * A `<span>` for an insertion point. No text node: the "+" comes from CSS
 * `::before`, so the document's text and the clipboard never see it.
 */
function insertMarker(className: string, id: string | null): HTMLElement {
  const span = document.createElement("span");
  span.className = className;
  span.setAttribute("contenteditable", "false");
  if (id !== null) {
    span.dataset.suggestionId = id;
    span.setAttribute("aria-label", "Suggested insertion");
  }
  return span;
}

function buildDecorations(pmDoc: ProseMirrorNode, storage: SuggestionsHighlightStorage): DecorationSet {
  const ydoc = storage.doc;
  if (!ydoc || (storage.suggestions.length === 0 && !storage.draft)) return DecorationSet.empty;
  // The plugin has no editor reference of its own; see editorRefHolder.
  const editor = editorRefHolder.editor;
  if (!editor) return DecorationSet.empty;

  const docSize = pmDoc.content.size;
  // Mirrors classifySuggestion, so the page never paints what the card calls
  // removed: a point must sit in a textblock, and a range must be non-empty
  // and inside a single textblock.
  const decodePoint = (startB64: string, endB64: string): number | null => {
    const range = decodeSuggestionRange(editor, ydoc, startB64, endB64);
    if (!range) return null;
    const from = Math.max(0, Math.min(range.from, docSize));
    return pmDoc.resolve(from).parent.isTextblock ? from : null;
  };
  const decodeRange = (startB64: string, endB64: string): { from: number; to: number } | null => {
    const range = decodeSuggestionRange(editor, ydoc, startB64, endB64);
    if (!range) return null;
    const from = Math.max(0, Math.min(range.from, docSize));
    const to = Math.max(0, Math.min(range.to, docSize));
    if (from >= to) return null;
    const parent = pmDoc.resolve(from).parent;
    if (!parent.isTextblock || pmDoc.resolve(to).parent !== parent) return null;
    return { from, to };
  };

  const decos: Decoration[] = [];

  for (const s of storage.suggestions) {
    const isActive = storage.activeSuggestionId === s.id;

    if (s.kind === "insert") {
      const point = decodePoint(s.position_y, s.position_y_end);
      if (point === null) continue;
      const cls = `suggestion-insert-marker${isActive ? " suggestion-insert-marker--active" : ""}`;
      decos.push(
        Decoration.widget(point, () => insertMarker(cls, s.id), {
          // The anchor ties to the character before the point, so the marker
          // stays with it when text is typed at the point.
          side: -1,
          key: `suggestion-insert:${s.id}:${isActive ? "active" : ""}`,
        }),
      );
      continue;
    }

    // A replace or delete whose range collapsed (its text was deleted) or
    // now spans blocks has nothing to tint; its card shows it as removed.
    const range = decodeRange(s.position_y, s.position_y_end);
    if (!range) continue;
    decos.push(
      Decoration.inline(range.from, range.to, {
        class: `suggestion-highlight${isActive ? " suggestion-highlight--active" : ""}`,
        "data-suggestion-id": s.id,
      }),
    );
  }

  const draft = storage.draft;
  if (draft?.kind === "insert") {
    const point = decodePoint(draft.position_y, draft.position_y_end);
    if (point !== null) {
      decos.push(
        Decoration.widget(point, () => insertMarker("suggestion-insert-marker suggestion-draft-marker", null), {
          side: -1,
          key: "suggestion-draft",
        }),
      );
    }
  } else if (draft) {
    // Same guard as a pending replace: a draft whose text was deleted while
    // the composer is open draws nothing; the composer shows its live state.
    const range = decodeRange(draft.position_y, draft.position_y_end);
    if (range) decos.push(Decoration.inline(range.from, range.to, { class: "suggestion-draft" }));
  }

  return DecorationSet.create(pmDoc, decos);
}

/**
 * The plugin can't reach the Tiptap `editor` instance from inside its state
 * callbacks (it would need the editor at construction time). React updates
 * this holder via setSuggestionsEditorRef when the editor mounts. See
 * KnotEditor.
 */
const editorRefHolder: { editor: Editor | null } = { editor: null };

/** Give the plugin the editor it decodes anchors against (null on unmount). */
export function setSuggestionsEditorRef(editor: Editor | null) {
  editorRefHolder.editor = editor;
}

/** Trigger a re-decoration after updating storage on the editor. */
export function refreshSuggestionHighlights(editor: Editor) {
  editor.view.dispatch(editor.state.tr.setMeta(PLUGIN_KEY, "refresh"));
}
