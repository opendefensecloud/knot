/**
 * Suggestion highlights against a live ySync binding.
 *
 * The extension resolves suggestion anchors through the binding's mapping,
 * so a fake editor would only ever exercise the empty branches. These tests
 * mount the shipped extension set, push suggestions into storage the way
 * KnotEditor does, and read the decorations back out of the editor's DOM.
 */

import { afterEach, describe, expect, it, vi } from "vitest";
import * as Y from "yjs";

import { mountBoundEditor, type BoundEditor } from "../../test/boundEditor";
import { encodeAnchorRange } from "../comments/anchor";
import { decodeSuggestionRange, encodeSuggestionAnchors } from "../suggestions/anchor";
import { refreshHighlights, setEditorRef } from "./CommentsHighlightExtension";
import {
  type DraftRange,
  refreshSuggestionHighlights,
  setSuggestionsEditorRef,
  type HighlightedSuggestion,
} from "./SuggestionsHighlightExtension";

describe("suggestion highlights", () => {
  let bound: BoundEditor | null = null;

  afterEach(() => {
    setSuggestionsEditorRef(null);
    setEditorRef(null);
    bound?.destroy();
    bound = null;
  });

  /** Mount with `text` in the first paragraph and register the editor ref,
   *  which `mountBoundEditor` does not do. */
  function withText(text: string): BoundEditor {
    bound = mountBoundEditor();
    bound.editor.commands.insertContent(text);
    setSuggestionsEditorRef(bound.editor);
    return bound;
  }

  /** A highlighted suggestion over [from, to). */
  function suggestion(id: string, kind: HighlightedSuggestion["kind"], from: number, to: number): HighlightedSuggestion {
    const anchors = encodeSuggestionAnchors(bound!.editor, bound!.ydoc, from, to);
    expect(anchors).not.toBeNull();
    return { id, kind, position_y: anchors!.start, position_y_end: anchors!.end };
  }

  function show(suggestions: HighlightedSuggestion[], activeSuggestionId: string | null = null) {
    const storage = bound!.editor.extensionStorage.suggestionsHighlight;
    storage.suggestions = suggestions;
    storage.activeSuggestionId = activeSuggestionId;
    refreshSuggestionHighlights(bound!.editor);
  }

  /** A draft of `kind` over [from, to), pushed into storage. */
  function showDraft(kind: DraftRange["kind"], from: number, to: number) {
    const anchors = encodeSuggestionAnchors(bound!.editor, bound!.ydoc, from, to);
    expect(anchors).not.toBeNull();
    bound!.editor.extensionStorage.suggestionsHighlight.draft = {
      kind,
      position_y: anchors!.start,
      position_y_end: anchors!.end,
    };
    refreshSuggestionHighlights(bound!.editor);
  }

  /** Run the click props as ProseMirror would, with `target` as the event
   *  target. jsdom has no layout, so a real mouse event maps to no position. */
  function click(target: Element): boolean {
    const { editor } = bound!;
    const event = new MouseEvent("click", { bubbles: true });
    Object.defineProperty(event, "target", { value: target });
    const paragraph = editor.state.doc.child(0);
    return editor.view.someProp("handleClickOn", (handler) => handler(editor.view, 8, paragraph, 0, event, true)) ?? false;
  }

  function dom(): HTMLElement {
    return bound!.editor.view.dom;
  }

  /** The one element matching `selector`; fails the test if there are more. */
  function only(selector: string): HTMLElement {
    const found = dom().querySelectorAll<HTMLElement>(selector);
    expect(found).toHaveLength(1);
    return found.item(0);
  }

  it("renders_inline_highlight_for_replace", () => {
    withText("Hello brave world");
    show([suggestion("s1", "replace", 7, 12)]);

    const span = only(".suggestion-highlight");
    expect(span.dataset.suggestionId).toBe("s1");
    expect(span.textContent).toBe("brave");
    expect(span.classList.contains("suggestion-highlight--active")).toBe(false);
  });

  it("renders a delete like a replace", () => {
    withText("Hello brave world");
    show([suggestion("s1", "delete", 7, 12)]);

    const span = dom().querySelector<HTMLElement>(".suggestion-highlight[data-suggestion-id='s1']");
    expect(span?.textContent).toBe("brave");
  });

  it("renders_widget_for_insert", () => {
    withText("Hello world");
    show([suggestion("s2", "insert", 6, 6)]);

    const marker = only(".suggestion-insert-marker");
    expect(marker.dataset.suggestionId).toBe("s2");
    expect(marker.getAttribute("contenteditable")).toBe("false");
    expect(marker.getAttribute("aria-label")).toBe("Suggested insertion");
    // The "+" is drawn by CSS, so nothing reaches textBetween, copy or docText.
    expect(marker.childNodes).toHaveLength(0);
    expect(bound!.editor.state.doc.textContent).toBe("Hello world");
    expect(dom().querySelector(".suggestion-highlight")).toBeNull();

    // The widget sits right after "Hello".
    expect(marker.previousSibling?.textContent).toBe("Hello");
  });

  it("renders_draft_range", () => {
    withText("Hello brave world");
    showDraft("replace", 7, 12);

    const draft = only(".suggestion-draft");
    expect(draft.textContent).toBe("brave");
    expect(draft.hasAttribute("data-suggestion-id")).toBe(false);
    expect(dom().querySelector(".suggestion-highlight")).toBeNull();
  });

  it("renders an insert draft as an insertion marker", () => {
    const { editor } = withText("Hello world");
    showDraft("insert", 6, 6);

    const marker = dom().querySelector<HTMLElement>(".suggestion-draft-marker");
    expect(marker).not.toBeNull();
    expect(marker!.classList.contains("suggestion-insert-marker")).toBe(true);
    expect(marker!.hasAttribute("data-suggestion-id")).toBe(false);
    expect(marker!.getAttribute("contenteditable")).toBe("false");
    expect(marker!.childNodes).toHaveLength(0);

    // Clearing the draft removes it.
    editor.extensionStorage.suggestionsHighlight.draft = null;
    refreshSuggestionHighlights(editor);
    expect(dom().querySelector(".suggestion-draft-marker")).toBeNull();
  });

  it("draws nothing for a replace draft whose text was deleted", () => {
    const { editor } = withText("Hello brave world");
    showDraft("replace", 7, 12);
    expect(only(".suggestion-draft").textContent).toBe("brave");

    editor.view.dispatch(editor.state.tr.delete(7, 12));
    refreshSuggestionHighlights(editor);

    expect(dom().querySelector(".suggestion-draft")).toBeNull();
    expect(dom().querySelector(".suggestion-draft-marker")).toBeNull();
  });

  it("active_class_follows_storage", () => {
    withText("Hello brave world");
    const replace = suggestion("s1", "replace", 7, 12);
    const insert = suggestion("s2", "insert", 6, 6);

    show([replace, insert], "s1");
    const span = () => dom().querySelector<HTMLElement>(".suggestion-highlight[data-suggestion-id='s1']");
    expect(span()?.classList.contains("suggestion-highlight--active")).toBe(true);
    expect(dom().querySelectorAll(".suggestion-highlight--active")).toHaveLength(1);

    show([replace, insert], "s2");
    expect(span()?.classList.contains("suggestion-highlight--active")).toBe(false);
    const marker = dom().querySelector<HTMLElement>(".suggestion-insert-marker[data-suggestion-id='s2']");
    expect(marker?.classList.contains("suggestion-insert-marker--active")).toBe(true);

    show([replace, insert], null);
    expect(dom().querySelector(".suggestion-highlight--active")).toBeNull();
    expect(dom().querySelector(".suggestion-insert-marker--active")).toBeNull();
  });

  it("click_calls_onSelect", () => {
    const { editor } = withText("Hello brave world");
    const onSelect = vi.fn();
    editor.extensionStorage.suggestionsHighlight.onSelect = onSelect;
    show([suggestion("s1", "replace", 7, 12), suggestion("s2", "insert", 1, 1)]);

    const span = dom().querySelector(".suggestion-highlight[data-suggestion-id='s1']")!;
    expect(click(span)).toBe(true);
    expect(onSelect).toHaveBeenLastCalledWith("s1");

    const marker = dom().querySelector(".suggestion-insert-marker[data-suggestion-id='s2']")!;
    expect(click(marker)).toBe(true);
    expect(onSelect).toHaveBeenLastCalledWith("s2");

    // A click on plain text is left to the editor.
    onSelect.mockClear();
    expect(click(dom().querySelector("p")!)).toBe(false);
    expect(onSelect).not.toHaveBeenCalled();
  });

  it("collapsed_replace_is_skipped", () => {
    const { editor, ydoc } = withText("Hello brave world");
    // s1: a replace whose anchors are one point from the start.
    const sameAnchor = suggestion("s1", "insert", 7, 7);
    // s2: a replace over "brave", which is then deleted.
    const removed = suggestion("s2", "replace", 7, 12);

    editor.view.dispatch(editor.state.tr.delete(7, 12));
    // s2 really is collapsed rather than undecodable, so the test exercises
    // the from >= to guard and not the null branch.
    const decoded = decodeSuggestionRange(editor, ydoc, removed.position_y, removed.position_y_end);
    expect(decoded).not.toBeNull();
    expect(decoded!.from).toBe(decoded!.to);

    show([{ ...sameAnchor, kind: "replace" }, removed]);

    expect(dom().querySelector(".suggestion-highlight")).toBeNull();
    expect(dom().querySelector(".suggestion-insert-marker")).toBeNull();
  });

  it("skips a replace whose range spans two blocks", () => {
    // A split inside the range does not get here: y-tiptap writes the new
    // block as new Yjs items, so the end anchor falls back into the first
    // block. The guard is defensive, so the range is encoded across the
    // blocks directly: "Hello brave" | " world", over "brave" and " w".
    const { editor, ydoc } = withText("Hello brave world");
    editor.view.dispatch(editor.state.tr.split(12));
    expect(editor.state.doc.childCount).toBe(2);
    const s = suggestion("s1", "replace", 7, 16);
    const decoded = decodeSuggestionRange(editor, ydoc, s.position_y, s.position_y_end);
    expect(decoded).toEqual({ from: 7, to: 16 });

    show([s]);
    expect(dom().querySelector(".suggestion-highlight")).toBeNull();

    // The same guard applies to a draft.
    showDraft("replace", 7, 16);
    expect(dom().querySelector(".suggestion-draft")).toBeNull();
  });

  describe("a click on text under both a comment and a suggestion", () => {
    function withComment(from: number, to: number) {
      const { editor, ydoc } = bound!;
      setEditorRef(editor);
      const anchors = encodeAnchorRange(editor, ydoc, from, to);
      const storage = editor.extensionStorage.commentsHighlight;
      // Copied from the options in onCreate, which Tiptap 3 runs on a timeout.
      storage.doc = ydoc;
      storage.comments = [{ id: "c1", thread_id: "t1", position_y: anchors.start!, position_y_end: anchors.end! }];
      refreshHighlights(editor);
    }

    function select() {
      const onComment = vi.fn();
      const onSuggestion = vi.fn();
      bound!.editor.extensionStorage.commentsHighlight.onSelect = onComment;
      bound!.editor.extensionStorage.suggestionsHighlight.onSelect = onSuggestion;
      return { onComment, onSuggestion };
    }

    /** The span ProseMirror merged both decorations into, over "brave". */
    function both(): HTMLElement {
      const span = only("[data-comment-id][data-suggestion-id]");
      expect(span.textContent).toBe("brave");
      return span;
    }

    it("goes to the suggestion when it is the narrower one", () => {
      withText("Hello brave world");
      withComment(1, 18);
      show([suggestion("s1", "replace", 7, 12)]);
      const { onComment, onSuggestion } = select();

      expect(click(both())).toBe(true);
      expect(onSuggestion).toHaveBeenCalledWith("s1");
      expect(onComment).not.toHaveBeenCalled();

      // Text under the comment alone still opens the comment.
      const commentOnly = [...dom().querySelectorAll<HTMLElement>("[data-comment-id]")].find(
        (el) => !el.hasAttribute("data-suggestion-id"),
      )!;
      expect(click(commentOnly)).toBe(true);
      expect(onComment).toHaveBeenCalledWith("t1");
    });

    it("goes to the comment when it is the narrower one", () => {
      withText("Hello brave world");
      withComment(7, 12);
      show([suggestion("s1", "replace", 1, 18)]);
      const { onComment, onSuggestion } = select();

      expect(click(both())).toBe(true);
      expect(onComment).toHaveBeenCalledWith("t1");
      expect(onSuggestion).not.toHaveBeenCalled();
    });
  });

  it("follows the anchored text through a later edit", () => {
    const { editor } = withText("Hello brave world");
    show([suggestion("s1", "replace", 7, 12)]);

    editor.view.dispatch(editor.state.tr.insertText("Oh, ", 1));

    const span = () => dom().querySelector<HTMLElement>(".suggestion-highlight[data-suggestion-id='s1']");
    expect(span()?.textContent).toBe("brave");

    // KnotEditor refreshes on the Y.Doc update that follows; decoding then
    // lands on the same characters.
    refreshSuggestionHighlights(editor);
    expect(span()?.textContent).toBe("brave");
  });

  it("follows the anchored text through a peer's edit", () => {
    const { ydoc } = withText("Hello brave world");
    show([suggestion("s1", "replace", 7, 12)]);

    const peer = new Y.Doc();
    Y.applyUpdate(peer, Y.encodeStateAsUpdate(ydoc));
    const paragraph = peer.getXmlFragment("default").get(0) as Y.XmlElement;
    (paragraph.get(0) as Y.XmlText).insert(0, "Oh, ");
    Y.applyUpdate(ydoc, Y.encodeStateAsUpdate(peer, Y.encodeStateVector(ydoc)));

    expect(bound!.editor.state.doc.textContent).toBe("Oh, Hello brave world");
    const span = dom().querySelector<HTMLElement>(".suggestion-highlight[data-suggestion-id='s1']");
    expect(span?.textContent).toBe("brave");
    peer.destroy();
  });

  it("draws nothing without the editor ref", () => {
    withText("Hello brave world");
    const s = suggestion("s1", "replace", 7, 12);
    setSuggestionsEditorRef(null);
    show([s]);

    expect(dom().querySelector(".suggestion-highlight")).toBeNull();
  });

  it("has its storage while the editor is alive", () => {
    const { editor } = withText("Hello");
    expect(editor.extensionStorage.suggestionsHighlight).toMatchObject({
      suggestions: [],
      activeSuggestionId: null,
      draft: null,
    });
  });
});
