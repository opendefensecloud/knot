/**
 * Walk only bare Text nodes inside a ProseMirror element, skipping any
 * collaboration-cursor label spans so cursor decorations don't pollute the
 * result.
 *
 * This matters more than it looks. A remote peer's caret renders its display
 * name as real text inside the editor, so `toContainText("PREFIX alpha")`
 * against a two-session document can read "PREFIX Owneralpha" and fail for a
 * reason that has nothing to do with the assertion. Run this inside
 * `locator.evaluate` instead.
 */
export function docText(el: Element): string {
  const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT, {
    acceptNode(node) {
      let p: Node | null = node.parentElement;
      while (p && p !== el) {
        if (p instanceof Element && p.classList.contains("collaboration-cursor__label")) {
          return NodeFilter.FILTER_REJECT;
        }
        p = p.parentNode;
      }
      return NodeFilter.FILTER_ACCEPT;
    },
  });
  const parts: string[] = [];
  let n: Node | null;
  while ((n = walker.nextNode())) parts.push(n.textContent ?? "");
  return parts.join("");
}
