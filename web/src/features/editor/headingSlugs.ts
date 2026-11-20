/**
 * Heading anchor ids, the way GitHub makes them: lower case, letters,
 * digits, spaces and `-` kept, whitespace runs become `-`, repeats are
 * numbered `x`, `x-1`, `x-2`, and an empty result is `section`. The server
 * has the same rule (`crates/knot-markdown/src/slug.rs`) for public pages;
 * `heading_slugs.json` holds both to it. Rule after dendronotes
 * (`web/src/lib/outline.ts`, Apache-2.0).
 */

/** Rust's `char::is_whitespace`: JavaScript's `\s` without U+FEFF and with
 *  U+0085, so both sides split on the same characters. */
const SPACE = "[\\s\\u0085]";
const DROP = new RegExp(`\\uFEFF|\\p{M}|(?!${SPACE})[^\\p{Alphabetic}\\p{N}-]`, "gu");
const SPACES = new RegExp(`${SPACE}+`, "gu");

function baseSlug(title: string): string {
  // Letters are Unicode Alphabetic without marks, as Rust's
  // `is_alphabetic` and `is_combining_mark` give them.
  const joined = title
    .normalize("NFKD")
    .toLowerCase()
    .replace(DROP, "")
    .replace(SPACES, " ")
    .trim()
    .replace(/ /g, "-");
  return joined === "" ? "section" : joined;
}

/** A slug function for the headings of one page, in order. */
export function slugger(): (title: string) => string {
  const seen = new Map<string, number>();
  return (title) => {
    const base = baseSlug(title);
    const n = seen.get(base) ?? 0;
    seen.set(base, n + 1);
    return n === 0 ? base : `${base}-${n}`;
  };
}
