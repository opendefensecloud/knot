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
