import { describe, expect, it } from "vitest";

import { sanitizeEditorFragment, sanitizeSvg } from "./sanitize";

describe("sanitizeSvg", () => {
  it("strips <script> from board SVG", () => {
    const dirty = `<svg xmlns="http://www.w3.org/2000/svg"><script>alert(document.cookie)</script><rect width="10" height="10"/></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toContain("<script");
    expect(clean).not.toContain("alert(");
    // legitimate shape content survives
    expect(clean).toContain("rect");
  });

  it("strips inline event handlers", () => {
    const dirty = `<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10" onload="alert(1)"/></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toContain("onload");
    expect(clean).not.toContain("alert(1)");
  });

  it("strips foreignObject (HTML/JS smuggling vector)", () => {
    const dirty = `<svg xmlns="http://www.w3.org/2000/svg"><foreignObject><img src=x onerror=alert(1)></foreignObject></svg>`;
    const clean = sanitizeSvg(dirty);
    expect(clean).not.toContain("foreignObject");
    expect(clean).not.toContain("onerror");
  });

  it("preserves harmless SVG drawing markup", () => {
    const ok = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100"><path d="M0 0 L10 10" stroke="black"/></svg>`;
    const clean = sanitizeSvg(ok);
    expect(clean).toContain("path");
    expect(clean).toContain("stroke");
  });
});

describe("sanitizeEditorFragment", () => {
  function html(fragment: DocumentFragment): string {
    const host = document.createElement("div");
    host.appendChild(fragment);
    return host.innerHTML;
  }

  it("keeps the structure knot's schema can represent", () => {
    const out = html(
      sanitizeEditorFragment(
        '<h2>Title</h2><ul><li>one</li></ul><p><a href="https://x.test">link</a></p>',
      ),
    );
    expect(out).toContain("<h2>Title</h2>");
    expect(out).toContain("<li>one</li>");
    expect(out).toContain('href="https://x.test"');
  });

  it("keeps checkbox inputs so task items can be promoted", () => {
    const out = html(
      sanitizeEditorFragment('<ul><li><input type="checkbox" checked>done</li></ul>'),
    );
    expect(out).toContain("<input");
    expect(out).toContain('type="checkbox"');
  });

  it("keeps data-checked", () => {
    const out = html(sanitizeEditorFragment('<ul><li data-checked="true">done</li></ul>'));
    expect(out).toContain('data-checked="true"');
  });

  it("strips scripts and event handlers", () => {
    const out = html(
      sanitizeEditorFragment('<p>hi</p><script>alert(1)</script><img src=x onerror="alert(1)">'),
    );
    expect(out).not.toContain("script");
    expect(out).not.toContain("onerror");
  });

  it("strips javascript: URLs", () => {
    const out = html(sanitizeEditorFragment('<a href="javascript:alert(1)">x</a>'));
    expect(out).not.toContain("javascript:");
  });
});
