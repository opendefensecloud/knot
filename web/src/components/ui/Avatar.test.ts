/**
 * `colorFor` feeds two very different consumers: a CSS `background` on the
 * avatar, which accepts any colour notation, and the collaboration awareness
 * payload, which does not.
 *
 * y-prosemirror validates peer colours against /^#[0-9a-fA-F]{6}$/ and warns
 * on anything else (`cursor-plugin.js`: "A user uses an unsupported color
 * format"). It still renders the caret in v2, so `hsl()` merely logged once
 * per peer per session — but the constraint is real, and the successor
 * extension replaces a non-matching colour with `transparent` instead of
 * warning, which makes remote carets invisible.
 *
 * So the format is a contract, not a style choice, and it is asserted here.
 */

import { describe, expect, it } from "vitest";

import { colorFor } from "./Avatar";

/** The format y-prosemirror accepts. Do not loosen this. */
const SIX_DIGIT_HEX = /^#[0-9a-f]{6}$/i;

describe("colorFor", () => {
  it("returns a 6-digit hex colour", () => {
    expect(colorFor("6f1c9a3e-0000-4000-8000-000000000001")).toMatch(SIX_DIGIT_HEX);
  });

  it("returns a 6-digit hex colour for every input shape it is given", () => {
    // The real call sites pass a UUID or the literal "anon".
    for (const seed of ["anon", "", "a", "user-1", "ZZZ", "6f1c9a3e-dead-beef"]) {
      expect(colorFor(seed), `seed ${JSON.stringify(seed)}`).toMatch(SIX_DIGIT_HEX);
    }
  });

  it("is deterministic — a peer keeps the same colour across sessions", () => {
    expect(colorFor("user-1")).toBe(colorFor("user-1"));
  });

  it("separates different users", () => {
    const seeds = ["user-1", "user-2", "user-3", "user-4", "user-5", "user-6"];
    const colors = new Set(seeds.map(colorFor));
    expect(colors.size).toBeGreaterThan(1);
  });

  it("keeps the palette mid-tone, so white label text stays legible", () => {
    // The caret label renders `color: white` over this fill (prose.css).
    // The previous hsl(h, 70%, 45%) is what made that safe; assert the
    // property rather than the notation, so a future palette change has to
    // stay legible too.
    for (const seed of ["user-1", "user-2", "user-3", "anon"]) {
      const hex = colorFor(seed);
      const r = parseInt(hex.slice(1, 3), 16);
      const g = parseInt(hex.slice(3, 5), 16);
      const b = parseInt(hex.slice(5, 7), 16);
      // Rec. 709 relative luminance.
      const luminance = (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;
      expect(luminance, `${seed} -> ${hex}`).toBeLessThan(0.75);
    }
  });
});
