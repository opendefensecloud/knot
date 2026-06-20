import { describe, expect, it } from "vitest";

import { sanitizeSvg } from "./sanitize";

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
