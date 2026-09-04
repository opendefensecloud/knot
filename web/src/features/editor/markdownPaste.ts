/**
 * Markdown-aware paste.
 *
 * `handlePaste` only ever intercepted clipboard *files*; Markdown source
 * pasted as text landed verbatim, so `## Heading` stayed `## Heading`. The
 * two pure pieces of the fix live here so they can be tested without an
 * editor instance.
 *
 * The heuristic is deliberately asymmetric. A false negative pastes plain
 * text — exactly what happened before this existed, so nothing is lost. A
 * false positive silently mangles what the user pasted. So it errs toward
 * not firing: a lone cue only counts when it is unambiguous (a fence, a GFM
 * table, a `##` subheading), and everything else needs corroboration.
 */
import { marked } from "marked";

import { sanitizeEditorFragment } from "../../lib/sanitize";

const HEADING = /^ {0,3}(#{1,6})[ \t]+\S/;
const BULLET = /^ {0,3}[-*+][ \t]+\S/;
const ORDERED = /^ {0,3}\d{1,9}[.)][ \t]+\S/;
const QUOTE = /^ {0,3}>[ \t]?\S/;
const FENCE = /^ {0,3}(?:```|~~~)/;
const RULE = /^ {0,3}(?:-{3,}|\*{3,}|_{3,})[ \t]*$/;
const TABLE_ROW = /^ {0,3}\|.*\|[ \t]*$/;
const TABLE_DELIM = /^ {0,3}\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)+\|?[ \t]*$/;
const LINK = /\[[^\]\n]+\]\([^)\s]+(?:[ \t]+"[^"\n]*")?\)/;
const IMAGE = /!\[[^\]\n]*\]\([^)\s]+\)/;
const EMPHASIS = /(\*\*|__)(?=\S)[\s\S]+?\S\1/;

/**
 * Does this text look like Markdown source rather than plain text?
 *
 * Headings only count at the start of the paste or after a blank line. That
 * single rule is what keeps a pasted Python or shell snippet from being read
 * as a pile of `#` headings, because its comments sit directly above the
 * code they describe.
 *
 * Known limitation: repeated level-1 headings are not enough on their own,
 * because that is exactly what a `#`-commented config file looks like. Such
 * a file needs a `##` or a second kind of cue before it converts, which
 * costs us the rare document that has two `#` headings and nothing else.
 */
export function looksLikeMarkdown(text: string): boolean {
  if (text.trim().length === 0) return false;

  const lines = text.split(/\r?\n/);
  let sawFence = false;
  let sawTable = false;
  let sawSubHeading = false;
  let headings = 0;
  let bullets = 0;
  let ordered = 0;
  let quotes = 0;
  let rules = 0;

  for (const [i, line] of lines.entries()) {
    if (FENCE.test(line)) sawFence = true;
    if (TABLE_ROW.test(line) && TABLE_DELIM.test(lines[i + 1] ?? "")) {
      sawTable = true;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      // Start-of-paste or after a blank line. Real Markdown separates its
      // headings; `#` comments in source code sit against their code.
      const atBlockStart = i === 0 || (lines[i - 1] ?? "").trim().length === 0;
      if (atBlockStart) {
        headings += 1;
        if ((heading[1] ?? "").length >= 2) sawSubHeading = true;
      }
      continue;
    }
    if (BULLET.test(line)) bullets += 1;
    else if (ORDERED.test(line)) ordered += 1;
    else if (QUOTE.test(line)) quotes += 1;
    else if (RULE.test(line)) rules += 1;
  }

  // Unambiguous on their own: no other text format uses these.
  if (sawFence || sawTable || sawSubHeading) return true;

  const kinds =
    (headings > 0 ? 1 : 0) +
    (bullets > 0 ? 1 : 0) +
    (ordered > 0 ? 1 : 0) +
    (quotes > 0 ? 1 : 0) +
    (rules > 0 ? 1 : 0) +
    (LINK.test(text) ? 1 : 0) +
    (IMAGE.test(text) ? 1 : 0) +
    (EMPHASIS.test(text) ? 1 : 0);
  if (kinds >= 2) return true;

  // A repeated block cue is a list or a quote block — structure, not prose.
  // Headings are excluded: repeated `#` is how most config formats comment.
  return bullets >= 2 || ordered >= 2 || quotes >= 2;
}

/**
 * Convert Markdown source to HTML the editor's schema can parse.
 *
 * Sanitize first, rewrite second: reparsing `marked`'s raw output into a
 * detached element would be enough for an `<img src=x onerror=…>` smuggled
 * through the Markdown to start loading.
 */
export function markdownToHtml(markdown: string): string {
  const raw = marked.parse(markdown, { async: false, gfm: true });
  const fragment = sanitizeEditorFragment(raw);
  promoteTaskItems(fragment);
  const host = document.createElement("div");
  host.appendChild(fragment);
  return host.innerHTML;
}

/**
 * `marked` renders GFM task items as
 * `<li><input type="checkbox" checked disabled>text</li>`, but knot's
 * `list_item` carries its state in `data-checked` (see `TaskListExtension`)
 * and has no `input` in its schema. Without this rewrite `- [x] done`
 * degrades to a plain bullet and disappears from `/tasks`.
 */
function promoteTaskItems(fragment: DocumentFragment): void {
  fragment.querySelectorAll("li > input[type='checkbox']").forEach((input) => {
    const li = input.parentElement;
    if (!li) return;
    li.setAttribute("data-checked", input.hasAttribute("checked") ? "true" : "false");
    input.remove();
  });
}
