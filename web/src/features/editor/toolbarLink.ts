/**
 * The toolbar's URL link logic, kept out of EditorToolbar so it can be tested
 * against the shipped extension set.
 *
 * The toolbar answered two questions with one call. `isActive("link")` asks
 * what the next keystroke would be marked with; Apply (`extendMarkRange`) and
 * Remove (`unsetLink`) act on the link touching the caret on either side.
 * They already disagreed at a link's start, and with non-inclusive links (see
 * KnotLink) they disagree at its end too — exactly where pasting a URL leaves
 * the caret. There the button showed an empty URL field while an innocent
 * empty Apply removed the link for everyone. The toolbar now shows the link
 * it is about to act on.
 */
import { getMarkRange, type Editor } from "@tiptap/core";
import type { MarkType, ResolvedPos } from "@tiptap/pm/model";
import { Selection } from "@tiptap/pm/state";

export type ToolbarLinkState = { active: boolean; href: string | null };

export function toolbarLinkState(editor: Editor): ToolbarLinkState {
  const { selection, schema } = editor.state;
  const linkType = schema.marks.link;
  if (!linkType) return { active: false, href: null };
  if (!selection.empty) {
    const href = editor.getAttributes("link").href as string | undefined;
    return { active: editor.isActive("link"), href: href ?? null };
  }
  const href = linkTouchingCaret(selection.$from, linkType)?.attrs.href as string | undefined;
  return { active: href !== undefined, href: href ?? null };
}

/**
 * Link the selection, or the link touching the caret, to `href`, then leave
 * the caret at the link's end so the next keystroke cannot replace it.
 *
 * With nothing to link, the URL itself becomes the link text. The old
 * behaviour stored a pending mark for the next keystroke, which only ever
 * linked that one character once links stopped being inclusive.
 *
 * Goes through `setLink` in every case so its `isAllowedUri` check still
 * rejects `javascript:` and friends; returns false, changing nothing, when it
 * does.
 */
export function applyLinkUrl(editor: Editor, href: string): boolean {
  if (!editor.can().setLink({ href })) return false;
  const { selection, schema } = editor.state;
  const linkType = schema.marks.link;
  const chain = editor.chain().focus();
  if (selection.empty && linkType && !getMarkRange(selection.$from, linkType)) {
    const insertAt = selection.from;
    chain
      .insertContentAt(insertAt, { type: "text", text: href })
      .setTextSelection({ from: insertAt, to: insertAt + href.length });
  } else {
    chain.extendMarkRange("link");
  }
  return chain
    .setLink({ href })
    .command(({ tr }) => {
      // `near`, not a TextSelection at `to`: after select-all that is not
      // inside a textblock (see caretAfterPastedLink).
      tr.setSelection(Selection.near(tr.doc.resolve(tr.selection.to), -1));
      return true;
    })
    .run();
}

/** The link on the text after the caret, else before it — the order
 *  `getMarkRange` uses, so this is the link Apply and Remove will act on. */
function linkTouchingCaret($caret: ResolvedPos, linkType: MarkType) {
  const after = $caret.parent.childAfter($caret.parentOffset).node;
  const before = $caret.parent.childBefore($caret.parentOffset).node;
  return linkType.isInSet(after?.marks ?? []) ?? linkType.isInSet(before?.marks ?? []);
}
