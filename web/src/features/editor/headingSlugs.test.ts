/**
 * The heading slug rule, checked against the fixture the Rust crate
 * (`knot-markdown/tests/slug.rs`) reads too, so editor and public page agree.
 */

import { readFileSync } from "node:fs";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { slugger } from "./headingSlugs";

type Case = { titles: string[]; slugs: string[] };

const FIXTURE = join(import.meta.dirname, "../../../../crates/knot-markdown/tests/fixtures/heading_slugs.json");

describe("heading slugs", () => {
  it("slugs match the shared fixture", () => {
    const cases = JSON.parse(readFileSync(FIXTURE, "utf8")) as Case[];
    for (const c of cases) {
      const slug = slugger();
      expect(c.titles.map((t) => slug(t)), JSON.stringify(c.titles)).toEqual(c.slugs);
    }
  });
});
