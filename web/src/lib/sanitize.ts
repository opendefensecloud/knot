import DOMPurify from "dompurify";

/**
 * Sanitize untrusted SVG markup before injecting it via
 * `dangerouslySetInnerHTML`.
 *
 * Excalidraw board previews are authored by other workspace users and stored
 * verbatim. Without sanitization, a crafted board SVG (e.g. a `<script>` tag or
 * an `onload=` handler) is stored XSS running on the app origin for anyone who
 * views the document. We restrict to the SVG element profile and explicitly
 * drop `<script>`, `<foreignObject>` (which can smuggle arbitrary HTML/JS),
 * inline event handlers, and unsafe URL schemes (DOMPurify drops `javascript:`
 * and `on*` by default; the FORBID lists are belt-and-suspenders).
 */
export function sanitizeSvg(svg: string): string {
  return DOMPurify.sanitize(svg, {
    USE_PROFILES: { svg: true, svgFilters: true },
    FORBID_TAGS: ["script", "foreignObject"],
    FORBID_ATTR: ["onload", "onerror", "onclick"],
  });
}

/** Tags the knot editor schema can actually represent, plus `input` — which
 *  is not a schema node, but must survive sanitization long enough for
 *  `markdownToHtml` to turn `marked`'s task-list checkboxes into
 *  `data-checked` on the `<li>`. */
const EDITOR_ALLOWED_TAGS = [
  "p", "br", "hr",
  "h1", "h2", "h3", "h4", "h5", "h6",
  "blockquote", "pre", "code",
  "ul", "ol", "li",
  "strong", "em", "u", "s", "del",
  "a", "img",
  "table", "thead", "tbody", "tr", "th", "td",
  "input",
];

const EDITOR_ALLOWED_ATTR = [
  "href", "title", "alt", "src", "class", "start",
  "colspan", "rowspan", "align",
  "type", "checked", "data-checked",
];

/**
 * Sanitize HTML derived from pasted Markdown before it reaches the editor.
 *
 * Markdown passes raw HTML through untouched, so `<script>` or
 * `<img src=x onerror=…>` in a pasted document would otherwise be handed
 * straight to Tiptap's DOM parser.
 *
 * Returns a `DocumentFragment` rather than a string so callers can
 * post-process the result without an `innerHTML` round-trip — reparsing
 * unsanitized markup into a detached element is enough to start an `<img>`
 * load and fire its `onerror`.
 */
export function sanitizeEditorFragment(html: string): DocumentFragment {
  return DOMPurify.sanitize(html, {
    ALLOWED_TAGS: EDITOR_ALLOWED_TAGS,
    ALLOWED_ATTR: EDITOR_ALLOWED_ATTR,
    RETURN_DOM_FRAGMENT: true,
  });
}
