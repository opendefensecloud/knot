/**
 * Pasted lists that say how deep an item is with an attribute, not by
 * nesting it.
 *
 * Word Online, and some web pages built for screen readers, put every item
 * of a multi-level list on one level and mark its depth with `aria-level`
 * (Word Online: `data-aria-level`, and a separate `<ul>` or `<ol>` per item,
 * each in its own wrapper `<div>`). ProseMirror's parser reads only the
 * element structure, so every item pasted at the top level (#78).
 *
 * This rebuilds the nesting those attributes describe before the parser runs:
 * an item deeper than the one before it moves into a sublist of that item,
 * of its own list's type. Lists that already nest by structure are left as
 * they are: their items' levels are only compared with their siblings'.
 *
 * Word desktop's clipboard HTML (paragraphs with `mso-list` styles, no list
 * elements at all) is a different problem and is not handled here.
 */

const LEVEL_ATTRIBUTES = ["aria-level", "data-aria-level"] as const;
const ITEM_WITH_LEVEL = LEVEL_ATTRIBUTES.map((name) => `li[${name}]`).join(", ");

/** HTML in, HTML out, for ProseMirror's `transformPastedHTML`. Returns the
 *  input untouched unless there was nesting to rebuild. */
export function nestPastedListsByLevel(html: string): string {
  // Most pastes carry no level at all; skip the parse for those.
  if (!/aria-level/i.test(html)) return html;
  // A template parses inertly: no script runs and no image loads.
  const template = document.createElement("template");
  template.innerHTML = html;
  return nestListItemsByLevel(template.content) ? template.innerHTML : html;
}

/**
 * Moves list items under the item they are a level below, in place.
 * Returns whether anything moved.
 */
export function nestListItemsByLevel(root: ParentNode): boolean {
  // The parents of every list (or list wrapper) holding a levelled item, in
  // document order. Lists next to each other in one parent form a run: Word
  // Online gives each item its own list, so depth has to carry across them.
  const containers = new Set<Node>();
  for (const item of Array.from(root.querySelectorAll(ITEM_WITH_LEVEL))) {
    const list = item.parentElement;
    if (!list || !isList(list)) continue;
    const block = singleListWrapper(list.parentElement) ?? list;
    if (block.parentNode) containers.add(block.parentNode);
  }
  let changed = false;
  for (const container of containers) {
    for (const run of runsOfLevelledLists(container)) {
      if (nestRun(run)) changed = true;
    }
  }
  return changed;
}

type ListBlock = { block: Element; list: Element };

function isList(element: Element): boolean {
  return element.tagName === "UL" || element.tagName === "OL";
}

function isBlank(node: Node): boolean {
  return (
    node.nodeType === Node.COMMENT_NODE ||
    (node.nodeType === Node.TEXT_NODE && (node.textContent ?? "").trim() === "")
  );
}

/** `element` when it is a `<div>` holding one list and nothing else (Word
 *  Online's `ListContainerWrapper`), so the list can be found through it. */
function singleListWrapper(element: Element | null): Element | null {
  if (!element || element.tagName !== "DIV") return null;
  const children = Array.from(element.childNodes).filter((child) => !isBlank(child));
  const only = children[0];
  return children.length === 1 && only instanceof Element && isList(only) ? element : null;
}

function listInBlock(element: Element): Element | null {
  if (isList(element)) return element;
  if (singleListWrapper(element)) return element.firstElementChild;
  return null;
}

function directItems(list: Element): Element[] {
  return Array.from(list.children).filter((child) => child.tagName === "LI");
}

function levelOf(item: Element): number | null {
  for (const name of LEVEL_ATTRIBUTES) {
    const level = Number.parseInt(item.getAttribute(name) ?? "", 10);
    if (Number.isFinite(level) && level >= 1) return level;
  }
  return null;
}

/** Consecutive lists in `container` whose items carry levels, separated by
 *  nothing but whitespace or comments. */
function runsOfLevelledLists(container: Node): ListBlock[][] {
  const runs: ListBlock[][] = [];
  let run: ListBlock[] = [];
  for (const node of Array.from(container.childNodes)) {
    if (isBlank(node)) continue;
    const list = node instanceof Element ? listInBlock(node) : null;
    if (list && directItems(list).some((item) => levelOf(item) !== null)) {
      run.push({ block: node as Element, list });
      continue;
    }
    if (run.length > 0) runs.push(run);
    run = [];
  }
  if (run.length > 0) runs.push(run);
  return runs;
}

/**
 * Walks the run's items in order with a stack of open items. An item goes
 * under the nearest earlier item with a lower level; with none, it stays on
 * the top level, joining the run's current top-level list when the types
 * match (one list per item, as Word Online sends it, becomes one list).
 * An item without a level counts as level 1.
 */
function nestRun(run: ListBlock[]): boolean {
  // Google Docs writes `aria-level` too, but it also nests: the sublist is a
  // `<ul>` placed straight inside the parent's list, after the parent `<li>`,
  // which ProseMirror's parser already turns into a nested list. Rebuilding
  // from the levels as well would hang a later item inside the parent `<li>`
  // — ahead of that sublist — and reorder the items. Such a run is left as
  // it is.
  if (run.some(({ list }) => Array.from(list.children).some(isList))) return false;
  const allItems = run.flatMap(({ list }) => directItems(list).map((item) => ({ item, list })));
  if (!allItems.some(({ item }) => (levelOf(item) ?? 1) > 1)) return false;

  const openItems: { item: Element; level: number }[] = [];
  let topLevelList: Element | null = null;
  let moved = false;
  for (const { item, list } of allItems) {
    const level = levelOf(item) ?? 1;
    while (openItems.length > 0 && openItems[openItems.length - 1]!.level >= level) openItems.pop();
    const parent = openItems[openItems.length - 1];
    if (parent) {
      sublistOf(parent.item, list.tagName).appendChild(item);
      moved = true;
    } else if (topLevelList && topLevelList !== list && topLevelList.tagName === list.tagName) {
      topLevelList.appendChild(item);
      moved = true;
    } else {
      topLevelList = list;
    }
    openItems.push({ item, level });
  }

  // Lists (and their wrappers) whose every item moved elsewhere.
  for (const { block, list } of run) {
    if (directItems(list).length === 0) block.remove();
  }
  return moved;
}

/** The list of `tagName` that ends `item`, made if `item` does not end in one. */
function sublistOf(item: Element, tagName: string): Element {
  const last = item.lastElementChild;
  if (last && last.tagName === tagName) return last;
  const sublist = item.ownerDocument.createElement(tagName.toLowerCase());
  item.appendChild(sublist);
  return sublist;
}
