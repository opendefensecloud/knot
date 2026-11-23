/**
 * The y-tiptap side of the `pm_json` parity fixtures
 * (`crates/knot-history/tests/pm_json.rs` is the Rust side).
 *
 * - `pm_json_golden.json`: hand-built cases, written here by y-tiptap itself
 *   (the editor's shape), with y-tiptap's `yXmlFragmentToProsemirrorJSON` of
 *   each as `expected`. Without `KNOT_UPDATE_GOLDEN=1` the test recomputes
 *   them and requires the file to be equal, so a y-tiptap upgrade that
 *   changes the shape fails here.
 * - `pm_json_corpus.json`: a sample of the knot-markdown corpus, whose
 *   updates the Rust test writes; this test fills in y-tiptap's `expected`.
 *
 * Regenerate (from the repo root):
 *
 * ```text
 * cd web && KNOT_UPDATE_GOLDEN=1 pnpm vitest run src/features/docs/history/pmJsonGolden.test.ts -t golden
 * cd .. && KNOT_UPDATE_GOLDEN=1 cargo nextest run -p knot-history --test pm_json -E 'test(the_corpus_fixture_is_the_current_corpus)'
 * cd web && KNOT_UPDATE_GOLDEN=1 pnpm vitest run src/features/docs/history/pmJsonGolden.test.ts
 * ```
 *
 * A case's `divergence` (a difference `pm_json` cannot avoid, explained in
 * words) is kept whenever either file is rewritten.
 */

import { Buffer } from "node:buffer";
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { env } from "node:process";

import { getSchema, type JSONContent } from "@tiptap/core";
import { prosemirrorJSONToYXmlFragment, yXmlFragmentToProsemirrorJSON } from "@tiptap/y-tiptap";
import { Awareness } from "y-protocols/awareness";
import { describe, expect, it } from "vitest";
import * as Y from "yjs";

import { createExtensions } from "../../editor/extensions";

const FIXTURES = join(import.meta.dirname, "../../../../../crates/knot-history/tests/fixtures");
const GOLDEN = join(FIXTURES, "pm_json_golden.json");
const CORPUS = join(FIXTURES, "pm_json_corpus.json");
const UPDATE = env.KNOT_UPDATE_GOLDEN === "1";

interface FixtureCase {
  name: string;
  update: string;
  expected: unknown;
  divergence?: string;
}

const BOARD_ID = "11111111-2222-3333-4444-555555555555";
const BLOB_URL = "/api/blobs/22222222-3333-4444-5555-666666666666";
const DOC_URL = "knot://doc/33333333-4444-5555-6666-777777777777";
const USER_URL = "knot://user/44444444-5555-6666-7777-888888888888";
const TIME_URL = "knot://time/2026-06-04T17:00:00Z";

const text = (s: string, marks?: JSONContent["marks"]): JSONContent =>
  marks ? { type: "text", text: s, marks } : { type: "text", text: s };
const p = (...content: JSONContent[]): JSONContent =>
  content.length ? { type: "paragraph", content } : { type: "paragraph" };
const item = (content: JSONContent[], attrs?: Record<string, unknown>): JSONContent =>
  attrs ? { type: "list_item", attrs, content } : { type: "list_item", content };
const link = (href: string, title?: string) => ({
  type: "link",
  attrs: title === undefined ? { href } : { href, title },
});

/** Cases written through the live editor schema, as the editor writes them. */
const SCHEMA_CASES: [string, JSONContent[]][] = [
  [
    "marks",
    [
      p(
        text("bold", [{ type: "bold" }]),
        text(" "),
        text("italic", [{ type: "italic" }]),
        text(" "),
        text("code", [{ type: "code" }]),
        text(" "),
        text("strike", [{ type: "strike" }]),
        text(" "),
        text("underline", [{ type: "underline" }]),
        text(" "),
        text("link", [link("https://example.com", "Example")]),
      ),
    ],
  ],
  [
    "internal-links",
    [
      p(
        text("Page", [link(DOC_URL)]),
        text(" "),
        text("@Alice", [link(USER_URL)]),
        text(" "),
        text("Jun 4", [link(TIME_URL)]),
      ),
    ],
  ],
  [
    "headings",
    [1, 2, 3].map((level) => ({ type: "heading", attrs: { level }, content: [text(`level ${level}`)] })),
  ],
  [
    "blockquote-nested",
    [{ type: "blockquote", content: [p(text("outer")), { type: "blockquote", content: [p(text("inner"))] }] }],
  ],
  ["code-block", [{ type: "code_block", attrs: { language: "rust" }, content: [text("fn main() {\n    let x = 1;\n}")] }]],
  ["horizontal-rule", [p(text("above")), { type: "horizontal_rule" }, p(text("below"))]],
  [
    "lists-nested",
    [
      {
        type: "ordered_list",
        attrs: { start: 3 },
        content: [
          item([p(text("three")), { type: "bullet_list", content: [item([p(text("bullet"))])] }]),
          item([p(text("four"))]),
        ],
      },
    ],
  ],
  [
    "task-list",
    [
      {
        type: "bullet_list",
        content: [item([p(text("done"))], { checked: true }), item([p(text("todo"))], { checked: false })],
      },
    ],
  ],
  [
    "list-hard-break",
    [{ type: "bullet_list", content: [item([p(text("line one"), { type: "hard_break" }, text("line two"))])] }],
  ],
  [
    "table",
    [
      {
        type: "table",
        content: [
          {
            type: "table_row",
            content: [
              { type: "table_header", content: [p(text("A"))] },
              { type: "table_header", content: [p(text("B"))] },
              { type: "table_header", content: [p(text("C"))] },
            ],
          },
          {
            type: "table_row",
            content: [
              { type: "table_cell", attrs: { colspan: 2, align: "center" }, content: [p(text("wide"))] },
              { type: "table_cell", content: [p(text("c"))] },
            ],
          },
        ],
      },
    ],
  ],
  ["image", [{ type: "image", attrs: { src: "https://example.com/a.png", alt: "An image", title: "A title" } }]],
  [
    "attachment",
    [{ type: "attachment", attrs: { url: BLOB_URL, name: "report.pdf", size: 2048, contentType: "application/pdf" } }],
  ],
  ["excalidraw-board", [{ type: "excalidraw_board", attrs: { board_id: BOARD_ID, label: "Architecture" } }]],
  ["table-of-contents", [{ type: "table_of_contents", attrs: { maxLevel: 2 } }]],
  ["empty-paragraph", [p()]],
  ["empty-doc", []],
];

type Attributes = Record<string, unknown>;

/** A paragraph under the fragment holding one `XmlText` run with `attrs`. */
function rawRun(attrs: Attributes) {
  return (frag: Y.XmlFragment) => {
    const para = new Y.XmlElement("paragraph");
    frag.insert(0, [para]);
    const t = new Y.XmlText();
    para.insert(0, [t]);
    t.insert(0, "x", attrs);
  };
}

/** Cases the live schema cannot write: stored shapes `pm_json` must handle. */
const RAW_CASES: [string, (frag: Y.XmlFragment) => void][] = [
  ["hashed-mark", rawRun({ "bold--AbCd1234": {} })],
  [
    "unknown-tag",
    (frag) => {
      const callout = new Y.XmlElement("callout");
      frag.insert(0, [callout]);
      const para = new Y.XmlElement("paragraph");
      callout.insert(0, [para]);
      const t = new Y.XmlText();
      para.insert(0, [t]);
      t.insert(0, "inside");
    },
  ],
  ["unknown-mark", rawRun({ comment: { id: "c1" } })],
  ["mark-true", rawRun({ bold: true })],
  // The importer's inline code runs (`from_markdown.rs`): `code` stored as
  // `true` next to the run's other marks.
  ["import-code-link", rawRun({ link: { href: "https://example.com" }, code: true })],
  ["import-code-bold", rawRun({ bold: true, code: true })],
];

function newDoc(): Y.Doc {
  const doc = new Y.Doc();
  // Deterministic update bytes.
  doc.clientID = 1;
  return doc;
}

function finish(name: string, doc: Y.Doc): FixtureCase {
  return {
    name,
    update: Buffer.from(Y.encodeStateAsUpdate(doc)).toString("base64"),
    expected: yXmlFragmentToProsemirrorJSON(doc.getXmlFragment("default")),
  };
}

function goldenCases(): FixtureCase[] {
  const cases: FixtureCase[] = [];
  for (const [name, content] of SCHEMA_CASES) {
    const doc = newDoc();
    const schema = getSchema(
      createExtensions({ doc, awareness: new Awareness(doc), user: { name: "t", color: "#000000" } }),
    );
    prosemirrorJSONToYXmlFragment(schema, { type: "doc", content }, doc.getXmlFragment("default"));
    cases.push(finish(name, doc));
  }
  for (const [name, write] of RAW_CASES) {
    const doc = newDoc();
    doc.transact(() => write(doc.getXmlFragment("default")));
    cases.push(finish(name, doc));
  }
  return cases;
}

function readCases(path: string): FixtureCase[] {
  return JSON.parse(readFileSync(path, "utf8")) as FixtureCase[];
}

/** `value` with every object's keys sorted, as serde_json writes them. */
function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value !== null && typeof value === "object") {
    const obj = value as Record<string, unknown>;
    return Object.fromEntries(
      Object.keys(obj)
        .sort()
        .map((k) => [k, sortKeys(obj[k])]),
    );
  }
  return value;
}

describe("pm_json parity fixtures", () => {
  it("golden: y-tiptap's JSON of the hand-built cases", () => {
    const cases = goldenCases();
    let previous: FixtureCase[] = [];
    try {
      previous = readCases(GOLDEN);
    } catch (e) {
      if (!UPDATE) throw e;
    }
    for (const c of cases) {
      const divergence = previous.find((old) => old.name === c.name)?.divergence;
      if (divergence !== undefined) c.divergence = divergence;
    }
    expect(cases).toHaveLength(22);
    if (UPDATE) {
      writeFileSync(GOLDEN, JSON.stringify(cases, null, 2) + "\n");
      return;
    }
    expect(previous).toEqual(cases);
  });

  it("y-tiptap reads the knot-markdown corpus", () => {
    const entries = readCases(CORPUS);
    expect(entries.length).toBeGreaterThan(0);
    for (const entry of entries) {
      const doc = new Y.Doc();
      Y.applyUpdate(doc, new Uint8Array(Buffer.from(entry.update, "base64")));
      const actual = yXmlFragmentToProsemirrorJSON(doc.getXmlFragment("default"));
      if (UPDATE) {
        entry.expected = actual;
        continue;
      }
      expect(entry.expected, entry.name).not.toBeNull();
      expect(entry.expected, entry.name).toEqual(actual);
    }
    if (UPDATE) writeFileSync(CORPUS, JSON.stringify(sortKeys(entries)) + "\n");
  });
});
