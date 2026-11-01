/**
 * User input for a bound editor (see boundEditor.ts), driven through the
 * same ProseMirror entry points the browser uses.
 */

import { TextSelection } from "@tiptap/pm/state";
import type { EditorView } from "@tiptap/pm/view";

import type { BoundEditor } from "./boundEditor";

/**
 * Type `text` one character at a time the way prosemirror-view applies a DOM
 * text change: `handleTextInput` props first (input rules), then
 * `tr.insertText` over the selection, which is what decides the new text's
 * marks. Suggestion plugins react to the resulting transaction.
 */
export function typeText(view: EditorView, text: string) {
  for (const character of text) {
    const { from, to } = view.state.selection;
    const insertCharacter = () => view.state.tr.insertText(character, from, to);
    const handled = view.someProp("handleTextInput", (handler) =>
      handler(view, from, to, character, insertCharacter),
    );
    if (!handled) view.dispatch(insertCharacter());
  }
}

/**
 * The event to hand `view.pasteText` / `view.pasteHTML`. jsdom has no
 * ClipboardEvent, which they would otherwise construct; they only pass the
 * event on to `handlePaste` props, and the text arrives as an argument.
 */
export function pasteEvent(): ClipboardEvent {
  return new Event("paste") as ClipboardEvent;
}

export function placeSelection(bound: BoundEditor, from: number, to: number = from) {
  const { state } = bound.editor;
  bound.editor.view.dispatch(state.tr.setSelection(TextSelection.create(state.doc, from, to)));
}

export type LinkRun = { text: string; href: string | null };

type Mark = { type: string; attrs?: Record<string, unknown> };
type Inline = { type: string; text?: string; marks?: Mark[] };

/** The first paragraph's text, one entry per run of equal link href. */
export function linkRuns(bound: BoundEditor): LinkRun[] {
  const documentJson = bound.editor.getJSON() as { content?: { content?: Inline[] }[] };
  const inlines = documentJson.content?.[0]?.content ?? [];
  const runs: LinkRun[] = [];
  for (const inline of inlines) {
    const linkMark = inline.marks?.find((mark) => mark.type === "link");
    const href = (linkMark?.attrs?.href as string | undefined) ?? null;
    const previousRun = runs[runs.length - 1];
    if (previousRun && previousRun.href === href) previousRun.text += inline.text ?? "";
    else runs.push({ text: inline.text ?? "", href });
  }
  return runs;
}
