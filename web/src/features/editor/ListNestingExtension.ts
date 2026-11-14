/**
 * ListNestingExtension — nesting list items the ways people try besides Tab
 * (#78, "nesting of bullet points does not work, they are all on top level").
 *
 * Tab and Shift-Tab always nested and lifted (ListItem's keymap). What did
 * not:
 *
 * - Typing the Markdown way. StarterKit's bullet rule (`/^\s*([-+*])\s$/`)
 *   reads leading spaces as nothing. At the start of an item it cannot wrap
 *   at all — a list_item must open with a paragraph — so "  - two" stayed in
 *   the item as literal text; in a paragraph just after a list it wrapped and
 *   joined that list, putting "two" back at the top level.
 * - Pasting lists that mark depth with `aria-level` on flat items (Word
 *   Online, some web pages): every item landed on the top level. See
 *   pastedListLevels.ts.
 *
 * Only an INDENTED marker nests, and the indent names a level (two spaces per
 * level, as Markdown is written, until the typing shows a wider level; see
 * IndentUnit), not a step deeper than the line above. A bare "- " typed out
 * of habit on every line, or the same indent on every nested line, would
 * otherwise put each line one level deeper than the last.
 *
 * Nothing here adds to the schema or the Y.Doc: the rules produce the same
 * nested list_item / bullet_list structure Tab does.
 */

import { Extension, InputRule } from "@tiptap/core";
import type { Node as ProseMirrorNode, NodeType, ResolvedPos } from "@tiptap/pm/model";
import { Plugin, PluginKey, TextSelection, type EditorState, type Transaction } from "@tiptap/pm/state";
import { canJoin } from "@tiptap/pm/transform";

import { nestPastedListsByLevel } from "./pastedListLevels";
import { checkedState } from "./TaskListExtension";

const LIST_ITEM = "list_item";
const BULLET_LIST = "bullet_list";
const ORDERED_LIST = "ordered_list";

/**
 * At least one space (the indent, group 1) before a bullet ("-", "+", "*")
 * or ordered ("1." or "1)") marker (group 2), then the space that completes
 * it. Anchored at the start of the textblock: Tiptap matches against the
 * text before the caret.
 */
const INDENTED_LIST_MARKER = /^(\s+)([-+*]|\d+[.)])\s$/;

type WantedList = { type: NodeType; attrs: Record<string, unknown> | null };

/**
 * How many columns of typed indent make one list level, per editor. Two to
 * begin with: the editor's own Markdown export indents that way, and most
 * people type it so.
 *
 * Others type four spaces a level, or CommonMark's three under "1. ". Read
 * at two a level, "- one", "    - two" asks for level 3; there is no
 * level-2 item above "two" to nest it under, so it stops at level 2 — and
 * then "    - three" asks for level 3 again and, with "two" above it now,
 * sinks under it. The line that came up short is the evidence: an indent of
 * c columns that could only reach level L means a level is about c / (L - 1)
 * columns wide, and reading the next line that way keeps the same indent at
 * the same level. See learnIndentUnit for when the unit changes.
 *
 * A level skipped further down ("  - two", "      - three" at two a level)
 * comes up short too and widens the unit (to three there), so a later
 * "    - x" lands at level 2 where Markdown would put it at 3. Trusting the
 * default there instead would sink every later "      - " line one level
 * deeper than the last, the very bug this is for; the typo is the rarer case.
 */
type IndentUnit = { columns: number };

const DEFAULT_INDENT_UNIT = 2;

/**
 * The columns an indent spans. A tab is one level whatever a level's width,
 * so it counts as one unit of columns.
 */
function indentColumns(indent: string, unit: IndentUnit): number {
  let columns = 0;
  for (const character of indent) columns += character === "\t" ? unit.columns : 1;
  return columns;
}

/**
 * The list level an indent names, the top-level list being level 1: one
 * level per unit of columns. Less than a unit (one stray space, or two after
 * a list typed at four a level) still means level 2: any indent at all asks
 * for nesting.
 *
 * Reading the indent as a level, not as "one deeper than here", is what keeps
 * "- one", "  - two", "  - three" two siblings: after Enter, "three" is
 * already at level 2, and sinking it again would hang it under "two".
 */
function levelForIndent(indent: string, unit: IndentUnit): number {
  return 1 + Math.max(1, Math.floor(indentColumns(indent, unit) / unit.columns));
}

/**
 * Updates the indent unit from where a typed indent put its item: `wanted`
 * is the level the indent named, `reached` the level the item ended at.
 *
 * - Short of the wanted level (nothing deeper to nest under), the unit is
 *   wider than assumed: c columns that made level `reached` give
 *   floor(c / (reached - 1)) columns a level, the widest unit at which the
 *   same indent names the same level again.
 * - At level 2 with less indent than a unit, the indent is the unit: a list
 *   typed at two a level after one typed at four nests at two again, and
 *   "    - c" under "  - b" goes under "b". Never below the default, so one
 *   stray space does not make every later space a level.
 *
 * Indents with a tab in them teach nothing: a tab is a level, not a width.
 */
function learnIndentUnit(unit: IndentUnit, indent: string, wanted: number, reached: number) {
  if (reached < 2 || indent.includes("\t")) return;
  const columns = indentColumns(indent, unit);
  if (reached < wanted) {
    unit.columns = Math.max(unit.columns, Math.floor(columns / (reached - 1)));
  } else if (reached === 2 && columns < unit.columns) {
    unit.columns = Math.max(DEFAULT_INDENT_UNIT, columns);
  }
}

/**
 * How deeply the list item at `itemDepth` is nested, its outermost list
 * being level 1. Counted only through lists and their items: a list in a
 * blockquote or table cell inside an item starts again at 1, as its Markdown
 * would.
 */
function listLevel($position: ResolvedPos, itemDepth: number): number {
  let level = 0;
  for (let depth = itemDepth; depth > 0; depth--) {
    const node = $position.node(depth);
    if (node.type.name === LIST_ITEM) level++;
    else if (!isList(node)) break;
  }
  return level;
}

/** The list type a typed marker asks for, with the attributes to make one. */
function listTypeForMarker(state: EditorState, marker: string): WantedList {
  const { nodes } = state.schema;
  if (/^[-+*]$/.test(marker)) return { type: nodes[BULLET_LIST]!, attrs: null };
  // `start` has a schema minimum of 1; "0. " is still clearly a numbered list.
  return { type: nodes[ORDERED_LIST]!, attrs: { start: Math.max(1, Number.parseInt(marker, 10)) } };
}

function isList(node: ProseMirrorNode | null | undefined): node is ProseMirrorNode {
  return node?.type.name === BULLET_LIST || node?.type.name === ORDERED_LIST;
}

/**
 * Gives the list around the caret's item the typed marker's type, when the
 * item is alone in it. sinkListItem keeps the type of the list the item came
 * from, so "  1. " in a bullet list would otherwise make a nested bullet
 * list. A list that holds other items keeps its type: changing it would
 * renumber (or unnumber) those too.
 *
 * sinkListItem only joins a sublist of its own list's type, so the item may
 * sit in a new list right after one of the type it is turning into ("  1. b"
 * under an item that already has "1. a"). The two are joined: side by side
 * they would number from 1 twice, and come back from Markdown as one list.
 */
function matchListToMarker(tr: Transaction, wanted: WantedList) {
  // The caret is in the item's first paragraph: paragraph, item, then list.
  const $caret = tr.selection.$from;
  const listDepth = $caret.depth - 2;
  const list = $caret.node(listDepth);
  if (list.type === wanted.type || list.childCount !== 1) return;
  const listStart = $caret.before(listDepth);
  tr.setNodeMarkup(listStart, wanted.type, wanted.attrs);
  // canJoin alone would also join a bullet list to an ordered one: their
  // content is the same.
  if (tr.doc.resolve(listStart).nodeBefore?.type === wanted.type && canJoin(tr.doc, listStart)) {
    tr.join(listStart);
  }
}

/**
 * "  - " at the start of an item's first paragraph moves the item to the
 * level the indent names (see levelForIndent): sunk under the item before
 * it, as Tab would, lifted, as Shift-Tab would, or, when it is already
 * there, left in place with only the marker removed.
 *
 * Sinking stops where there is nothing to sink under, as in Markdown: an
 * indent two levels deeper than the item above still nests only one. On the
 * first item of a list it cannot sink at all. The rule then returns null,
 * which matters: the marker's deletion is already in the shared transaction,
 * and null makes Tiptap drop it, so the typed text stays as it was.
 */
function indentedMarkerInItemRule(unit: IndentUnit): InputRule {
  return new InputRule({
    find: INDENTED_LIST_MARKER,
    handler: ({ state, range, match, chain }) => {
      const { $from } = state.selection;
      const itemDepth = $from.depth - 1;
      if (itemDepth < 1) return null;
      if ($from.node(itemDepth).type.name !== LIST_ITEM || $from.index(itemDepth) !== 0) return null;
      const indent = match[1] ?? " ";
      const wantedLevel = levelForIndent(indent, unit);

      // One chain per step: each starts from the shared transaction as it
      // stands, and whether a step worked decides the next. Reading
      // `state.tr` likewise brings `state`'s caret up to date (the chainable
      // state only catches up when `tr` is read).
      const caretLevel = () => {
        const $caret = state.tr.selection.$from;
        return listLevel($caret, $caret.depth - 1);
      };
      chain().deleteRange(range).run();
      if (caretLevel() < wantedLevel && !chain().sinkListItem(LIST_ITEM).run()) return null;
      // Each sink or lift moves the item exactly one level, so these end.
      while (caretLevel() < wantedLevel && chain().sinkListItem(LIST_ITEM).run()) {
        // sinking toward the wanted level
      }
      while (caretLevel() > wantedLevel && chain().liftListItem(LIST_ITEM).run()) {
        // lifting toward the wanted level
      }

      matchListToMarker(state.tr, listTypeForMarker(state, match[2] ?? "-"));
      learnIndentUnit(unit, indent, wantedLevel, caretLevel());
    },
  });
}

/**
 * "  - " at the start of a paragraph right after a list nests the paragraph
 * in that list at the level the indent names — "- one", Enter, Enter,
 * "  - two" puts "two" under "one" — instead of joining the list at the top
 * level, which is what the stock bullet rule does with the indent ignored. A
 * bare "- " there still joins the list: that rule is left to handle it.
 *
 * The new item goes under the list's last item, or for a deeper indent under
 * that item's sublist's last item, and so on, stopping where there is no
 * deeper sublist (as sinking does). If the item it goes under already ends
 * with a sublist of the marker's type, it joins that sublist. Like Enter and
 * Tab, it makes a task item under a task item.
 *
 * Not inside a list item: there, the paragraph follows a sublist of the item
 * it belongs to, and the stock rule's join (a sibling of that sublist's
 * items) is already the nesting the indent asks for.
 */
function indentedMarkerAfterListRule(unit: IndentUnit): InputRule {
  return new InputRule({
    find: INDENTED_LIST_MARKER,
    handler: ({ state, range, match }) => {
      const { $from } = state.selection;
      if ($from.parent.type.name !== "paragraph") return null;
      const containerDepth = $from.depth - 1;
      const container = $from.node(containerDepth);
      if (container.type.name === LIST_ITEM) return null;
      const paragraphIndex = $from.index(containerDepth);
      if (paragraphIndex === 0) return null;
      const previousList = container.child(paragraphIndex - 1);
      if (!isList(previousList)) return null;

      // The item to nest under sits one level above the wanted one.
      const indent = match[1] ?? " ";
      const wantedLevel = levelForIndent(indent, unit);
      let parentItem = previousList.lastChild;
      let parentLevel = 1;
      while (parentItem && parentLevel < wantedLevel - 1) {
        const deeperList = parentItem.lastChild;
        if (!isList(deeperList) || !deeperList.lastChild) break;
        parentItem = deeperList.lastChild;
        parentLevel++;
      }
      if (!parentItem) return null;

      const { tr } = state;
      const listItemType = state.schema.nodes[LIST_ITEM]!;
      const wanted = listTypeForMarker(state, match[2] ?? "-");
      // The parent item is the last thing at its level of the previous list,
      // so it closes right where that list does, at the paragraph's start,
      // less one closing token per list and item around it: its content ends
      // two positions further in per level. Everything below happens at or
      // before the paragraph's start, so these stay valid as the marker and
      // then the paragraph are removed.
      const paragraphStart = $from.before();
      const parentContentEnd = paragraphStart - 2 * parentLevel;

      tr.delete(range.from, range.to);
      const paragraph = tr.doc.nodeAt(paragraphStart);
      if (!paragraph) return null;
      tr.delete(paragraphStart, paragraphStart + paragraph.nodeSize);

      // Imported checklists carry `checked` as "true" / "false" strings.
      const parentIsTask = checkedState(parentItem.attrs.checked) !== null;
      const newItem = listItemType.create(parentIsTask ? { checked: false } : null, paragraph);
      const existingSublist = parentItem.lastChild;
      let caret: number;
      if (parentItem.childCount > 1 && existingSublist?.type === wanted.type) {
        const sublistContentEnd = parentContentEnd - 1;
        tr.insert(sublistContentEnd, newItem);
        caret = sublistContentEnd + 2; // into the item, then its paragraph
      } else {
        tr.insert(parentContentEnd, wanted.type.create(wanted.attrs, newItem));
        caret = parentContentEnd + 3; // into the list, the item, the paragraph
      }
      // The marker sat at the paragraph's start, so the caret was there too.
      tr.setSelection(TextSelection.create(tr.doc, caret));
      learnIndentUnit(unit, indent, wantedLevel, parentLevel + 1);
    },
  });
}

export const ListNestingExtension = Extension.create<Record<string, never>, IndentUnit>({
  name: "knotListNesting",

  // Tiptap tries input rules plugin by plugin, highest priority first, and
  // stops at the first rule that changes the document. The stock bullet
  // rule (default priority) also matches "  - " after a list and would join
  // it at the top level, so these must be asked first. They return null
  // everywhere else, which hands the input on to the stock rules.
  priority: 101,

  // The indent unit lives as long as the editor: one person's typing habit,
  // learned once, carries over to the next list in the document.
  addStorage() {
    return { columns: DEFAULT_INDENT_UNIT };
  },

  addInputRules() {
    return [indentedMarkerInItemRule(this.storage), indentedMarkerAfterListRule(this.storage)];
  },

  addProseMirrorPlugins() {
    return [
      new Plugin({
        key: new PluginKey("knotPastedListLevels"),
        props: {
          transformPastedHTML: (html) => nestPastedListsByLevel(html),
        },
      }),
    ];
  },
});
