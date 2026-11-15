/**
 * Pins the history colours: every skin defines `--color-history-added`
 * at 4.5:1 against its background and surface (the rule tokens.css's header
 * states for destructive), and the diff styles colour their decorations with
 * the plain tokens, so the end-to-end spec can compare the computed
 * decoration colours with the tokens.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { SKINS } from "./skins";

const TOKENS = readFileSync(join(import.meta.dirname, "tokens.css"), "utf8");
const PROSE = readFileSync(join(import.meta.dirname, "prose.css"), "utf8");

/** Each CSS rule as `{selector, body}`, comments stripped. Flat files only:
 *  neither stylesheet nests rules except inside `@media`/`@keyframes`, whose
 *  inner rules this still finds. */
function rules(css: string): { selector: string; body: string }[] {
  const clean = css.replace(/\/\*[\s\S]*?\*\//g, "");
  return [...clean.matchAll(/([^{}]+)\{([^{}]*)\}/g)].map(([, selector = "", body = ""]) => ({
    selector: selector.trim(),
    body,
  }));
}

/** The `--color-*` hex values each `[data-skin="…"]` block declares. */
function skinColors(): Map<string, Map<string, string>> {
  const skins = new Map<string, Map<string, string>>();
  for (const { selector, body } of rules(TOKENS)) {
    for (const [, id = ""] of selector.matchAll(/\[data-skin="([^"]+)"\]/g)) {
      const colors = skins.get(id) ?? new Map<string, string>();
      for (const [, name = "", hex = ""] of body.matchAll(/--color-([\w-]+)\s*:\s*(#[0-9a-fA-F]{6})\b/g)) {
        colors.set(name, hex);
      }
      skins.set(id, colors);
    }
  }
  return skins;
}

function luminance(hex: string): number {
  const channel = (i: number) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
}

/** WCAG 2 contrast ratio of two `#rrggbb` colours. */
function contrast(a: string, b: string): number {
  const hi = Math.max(luminance(a), luminance(b));
  const lo = Math.min(luminance(a), luminance(b));
  return (hi + 0.05) / (lo + 0.05);
}

/** The declarations of the one rule whose selector ends in `suffix`. */
function declarations(suffix: string): Map<string, string> {
  const matching = rules(PROSE).filter(({ selector }) =>
    selector.split(",").some((s) => s.trim().endsWith(suffix)),
  );
  expect(matching, `prose.css has one rule for ${suffix}`).toHaveLength(1);
  return new Map(
    matching[0]!.body
      .split(";")
      .map((d) => d.trim())
      .filter(Boolean)
      .map((d) => {
        const at = d.indexOf(":");
        return [d.slice(0, at).trim(), d.slice(at + 1).trim()] as [string, string];
      }),
  );
}

describe("history colours", () => {
  it("every skin defines history-added at 4.5:1 against bg and surface", () => {
    const skins = skinColors();
    for (const { id } of SKINS) {
      const colors = skins.get(id);
      expect(colors, `tokens.css has no [data-skin="${id}"] block`).toBeDefined();
      const added = colors!.get("history-added");
      expect(added, `skin ${id} has no --color-history-added`).toBeDefined();
      for (const against of ["bg", "surface"]) {
        const other = colors!.get(against);
        expect(other, `skin ${id} has no --color-${against}`).toBeDefined();
        const ratio = contrast(added!, other!);
        expect(
          ratio >= 4.5,
          `skin ${id}: history-added ${added} is ${ratio.toFixed(2)}:1 against ${against} ${other}, under 4.5:1`,
        ).toBe(true);
      }
    }
  });

  it("the diff styles colour their decorations with the plain tokens", () => {
    const insert = declarations(".history-insert");
    const del = declarations(".history-delete");
    expect(insert.get("text-decoration-color")).toBe("var(--color-history-added)");
    expect(del.get("text-decoration-color")).toBe("var(--color-destructive)");
    expect(del.get("opacity")).toBe("0.8");
    for (const value of [insert.get("text-decoration-color"), del.get("text-decoration-color")]) {
      expect(value).not.toMatch(/color-mix|rgba?\(|,/);
    }
  });
});
