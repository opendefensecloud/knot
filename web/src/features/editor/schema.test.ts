/**
 * Asserts the live Tiptap/ProseMirror schema's node + mark names match the
 * canonical snake_case set declared in `tools/schema.json` (generated to
 * `schema.ts`). Without this, individual extensions can ship in camelCase
 * by default and we only notice when markdown export blows up at runtime
 * (cf. the `UnsupportedNode("bulletList")` regression).
 */

import { describe, expect, it } from "vitest";
import { Editor, getSchema } from "@tiptap/core";
import * as Y from "yjs";
import { Awareness } from "y-protocols/awareness";

import canonical from "../../../../tools/schema.json";

import { viewerSchema } from "../docs/history/viewerContent";

import { createExtensions, createSchemaExtensions } from "./extensions";
import { NODE_KINDS, MARK_KINDS } from "./schema";

function mount(): Editor {
  const doc = new Y.Doc();
  const awareness = new Awareness(doc);
  return new Editor({
    extensions: createExtensions({
      doc,
      awareness,
      user: { name: "test", color: "#000" },
    }),
  });
}

/**
 * Every extension `createExtensions()` registers, including the ones Tiptap
 * adds implicitly.
 *
 * The node/mark checks below cannot see an extension that contributes no node
 * and no mark — a keymap, an input-rule set, a ProseMirror plugin. Those are
 * exactly the extensions a StarterKit version bump turns on silently, and some
 * of them write to the document (a trailing-node extension appends a
 * paragraph; a duplicate Link registration overrides `openOnClick`). Pinning
 * the whole list means an addition has to be acknowledged rather than
 * discovered later from a corrupted Y.Doc.
 *
 * Update deliberately, never to make the test pass.
 */
const REGISTERED_EXTENSIONS = [
  "attachment",
  "blockquote",
  "bold",
  "bullet_list",
  "clipboardTextSerializer",
  "code",
  "code_block",
  "collaboration",
  // v3: CollaborationCursor has no v3 release; this is its successor.
  "collaborationCaret",
  "commands",
  "commentsHighlight",
  // v3 core: emits an editor "delete" event when nodes are removed. Skips
  // transactions carrying y-sync meta, so it never reacts to remote updates,
  // and mutates nothing.
  "delete",
  "doc",
  "drop",
  "dropCursor",
  "editable",
  "excalidraw_board",
  "focusEvents",
  "gapCursor",
  "hard_break",
  "heading",
  "horizontal_rule",
  "image",
  "italic",
  "keymap",
  "knotDateTime",
  "knotInternalLink",
  // Input rules that nest list items on a typed indent, and a paste transform
  // that nests aria-level list items. Both produce ordinary nested lists.
  "knotListNesting",
  "knotMention",
  // Plugin-only: node decorations giving each <ol> its widest marker's digit
  // count, for the stylesheet's marker gutter. View-only; no document change.
  "knotOrderedListMarkerGutter",
  "knotTaskList",
  // Keymap-only: End/Home move the DOM selection on a Mac; no document change.
  "lineBoundaryKeys",
  "link",
  "list_item",
  // v3: ListItem registers this keymap itself, named after the node. It
  // only matches the renamed lists because KnotListItem names them in its
  // bulletListTypeName / orderedListTypeName options.
  "list_itemBranchingDeleteKeymap",
  "ordered_list",
  "paragraph",
  "paste",
  "starterKit",
  "strike",
  // Plugin-only: blue decorations for pending change suggestions. Decorations
  // are view-only, so nothing it does reaches the document.
  "suggestionsHighlight",
  "tabindex",
  "table",
  "table_cell",
  "table_header",
  "table_of_contents",
  "table_row",
  "text",
  // v3 core. addGlobalAttributes() returns [] unless `direction` is
  // configured, and it is not, so this adds no attribute to any node — as
  // the attribute-parity test below independently confirms.
  "textDirection",
  "underline",
];

/**
 * Attributes the live editor carries that `tools/schema.json` does not
 * declare. Each is render- or interaction-time only and never reaches
 * `to_markdown`, so the omission is correct — but it has to be stated,
 * otherwise the parity check below cannot tell a deliberate extra from a new
 * one that arrived with a dependency bump.
 */
const EDITOR_ONLY_ATTRS: Record<string, string[]> = {
  // A column's relative width (see columnWidths.ts). It does ride in the
  // CRDT (one collaborator's drag is visible to everyone), but markdown has
  // no column-width concept, so the canonical schema omits it.
  table_cell: ["colwidth"],
  table_header: ["colwidth"],
  // Tiptap's OrderedList ships the HTML `type` attribute (1/a/A/i/I). Nothing
  // in knot sets it and `to_markdown` has no marker-style concept.
  ordered_list: ["type"],
  // `rel` and `target` come from Link.configure({ HTMLAttributes }); `class`
  // is Tiptap's own. All three are rendering concerns.
  link: ["class", "rel", "target"],
};

/**
 * Canonical attributes the editor does not implement. Empty, and meant to
 * stay that way — an entry here is a one-way data-loss path, because
 * ProseMirror silently drops what its schema does not declare.
 */
const UNIMPLEMENTED_ATTRS: Record<string, string[]> = {};

const canonicalAttrs = (entries: { kind: string; attrs?: { name: string }[] }[]) =>
  new Map(entries.map((e) => [e.kind, (e.attrs ?? []).map((a) => a.name)]));

describe("editor schema alignment", () => {
  it("every Tiptap node maps to a snake_case kind from tools/schema.json", () => {
    const doc = new Y.Doc();
    const awareness = new Awareness(doc);
    const editor = new Editor({
      extensions: createExtensions({
        doc,
        awareness,
        user: { name: "test", color: "#000" },
      }),
    });
    // Every node the schema generator declared must be present at the same
    // name on the live PM schema.
    for (const kind of NODE_KINDS) {
      expect(editor.schema.nodes[kind], `node ${kind} missing from PM schema`).toBeDefined();
    }
    // And every PM node must be one of the declared kinds — no camelCase
    // leak-through from a yet-to-be-renamed extension. `doc` and `text` are
    // always present even when not in NODE_KINDS in some shapes; check both
    // directions but allow the implicit ProseMirror builtins.
    const expected = new Set<string>(NODE_KINDS);
    expected.add("doc");
    expected.add("text");
    for (const name of Object.keys(editor.schema.nodes)) {
      expect(expected.has(name), `unexpected PM node "${name}" — not in tools/schema.json`).toBe(true);
    }
    editor.destroy();
  });

  it("can build a representative tree without violating any content expression", () => {
    // The presence checks above only catch missing nodes — they don't
    // catch a content-expression like `content: 'tableRow+'` that
    // references a node by the wrong name. This test instantiates a
    // canonical sample of each container node via `editor.schema.nodeFromJSON`
    // and asserts the result `check()`s clean.
    const doc = new Y.Doc();
    const awareness = new Awareness(doc);
    const editor = new Editor({
      extensions: createExtensions({
        doc,
        awareness,
        user: { name: "test", color: "#000" },
      }),
    });
    const sample = {
      type: "doc",
      content: [
        { type: "paragraph", content: [{ type: "text", text: "hi" }] },
        { type: "heading", attrs: { level: 1 }, content: [{ type: "text", text: "h" }] },
        { type: "blockquote", content: [{ type: "paragraph", content: [{ type: "text", text: "q" }] }] },
        { type: "code_block", content: [{ type: "text", text: "x" }] },
        { type: "horizontal_rule" },
        { type: "table_of_contents", attrs: { maxLevel: 2 } },
        {
          type: "bullet_list",
          content: [
            {
              type: "list_item",
              content: [{ type: "paragraph", content: [{ type: "text", text: "b" }] }],
            },
            {
              type: "list_item",
              attrs: { checked: false },
              content: [{ type: "paragraph", content: [{ type: "text", text: "task" }] }],
            },
          ],
        },
        {
          type: "ordered_list",
          attrs: { start: 1 },
          content: [
            {
              type: "list_item",
              content: [{ type: "paragraph", content: [{ type: "text", text: "n" }] }],
            },
          ],
        },
        {
          type: "table",
          content: [
            {
              type: "table_row",
              content: [
                {
                  type: "table_header",
                  content: [{ type: "paragraph", content: [{ type: "text", text: "h" }] }],
                },
              ],
            },
            {
              type: "table_row",
              content: [
                {
                  type: "table_cell",
                  content: [{ type: "paragraph", content: [{ type: "text", text: "v" }] }],
                },
              ],
            },
          ],
        },
      ],
    };
    // `nodeFromJSON(...).check()` throws on schema violations such as a
    // table whose content expression doesn't accept table_row.
    expect(() => editor.schema.nodeFromJSON(sample).check()).not.toThrow();
    editor.destroy();
  });

  it("every Tiptap mark maps to a snake_case kind from tools/schema.json", () => {
    const doc = new Y.Doc();
    const awareness = new Awareness(doc);
    const editor = new Editor({
      extensions: createExtensions({
        doc,
        awareness,
        user: { name: "test", color: "#000" },
      }),
    });
    for (const kind of MARK_KINDS) {
      expect(editor.schema.marks[kind], `mark ${kind} missing from PM schema`).toBeDefined();
    }
    const expected = new Set<string>(MARK_KINDS);
    for (const name of Object.keys(editor.schema.marks)) {
      expect(expected.has(name), `unexpected PM mark "${name}" — not in tools/schema.json`).toBe(true);
    }
    editor.destroy();
  });

  it("registers exactly the expected extension set", () => {
    const editor = mount();
    const names = editor.extensionManager.extensions.map((e) => e.name).sort();
    expect(
      names,
      "the registered extension set changed. If a dependency bump added this, "
        + "check what it does to the document before accepting it — a plugin-only "
        + "extension is invisible to every other assertion in this file.",
    ).toEqual([...REGISTERED_EXTENSIONS].sort());
    editor.destroy();
  });

  it("carries the attributes tools/schema.json declares, and no undeclared extras", () => {
    const editor = mount();
    const declaredNodes = canonicalAttrs(canonical.nodes);
    const declaredMarks = canonicalAttrs(canonical.marks);

    const check = (
      kind: "node" | "mark",
      name: string,
      live: string[],
      declared: string[] | undefined,
    ) => {
      if (declared === undefined) return; // name-level checks above own this
      const allowedExtra = EDITOR_ONLY_ATTRS[name] ?? [];
      const unimplemented = UNIMPLEMENTED_ATTRS[name] ?? [];

      for (const attr of declared) {
        if (unimplemented.includes(attr)) continue;
        expect(
          live,
          `${kind} "${name}" is missing the declared attribute "${attr}" — `
            + "content carrying it will be dropped on the next edit",
        ).toContain(attr);
      }
      for (const attr of live) {
        if (declared.includes(attr) || allowedExtra.includes(attr)) continue;
        expect.fail(
          `${kind} "${name}" grew an undeclared attribute "${attr}". It will be `
            + "stored in the CRDT and silently lost by to_markdown. Either declare "
            + "it in tools/schema.json or record it in EDITOR_ONLY_ATTRS.",
        );
      }
    };

    for (const [name, type] of Object.entries(editor.schema.nodes)) {
      check("node", name, Object.keys(type.spec.attrs ?? {}), declaredNodes.get(name));
    }
    for (const [name, type] of Object.entries(editor.schema.marks)) {
      check("mark", name, Object.keys(type.spec.attrs ?? {}), declaredMarks.get(name));
    }
    editor.destroy();
  });
});

/**
 * `createExtensions`' own list, in order, as it was before
 * `createSchemaExtensions` was split out of it. Extension order decides
 * plugin order (and, between equal priorities, keymap and paste-rule
 * order), so the split must leave it exactly as it was.
 */
const CREATE_EXTENSIONS_ORDER = [
  "starterKit",
  "bullet_list",
  "ordered_list",
  "list_item",
  "knotListNesting",
  "knotOrderedListMarkerGutter",
  "hard_break",
  "horizontal_rule",
  "code_block",
  "underline",
  "link",
  "collaboration",
  "collaborationCaret",
  "image",
  "attachment",
  "excalidraw_board",
  "table_of_contents",
  "knotTaskList",
  "knotInternalLink",
  "knotMention",
  "knotDateTime",
  "lineBoundaryKeys",
  "table",
  "table_row",
  "table_cell",
  "table_header",
  "commentsHighlight",
  "suggestionsHighlight",
];

function liveExtensions() {
  const doc = new Y.Doc();
  return createExtensions({
    doc,
    awareness: new Awareness(doc),
    user: { name: "test", color: "#000000" },
  });
}

const sortedKeys = (o: object) => Object.keys(o).sort();

describe("schema-only extensions", () => {
  it("createExtensions keeps its extension order", () => {
    expect(liveExtensions().map((e) => e.name)).toEqual(CREATE_EXTENSIONS_ORDER);
  });

  it("the schema extensions have the live schema's nodes and marks and no collaboration", () => {
    const live = getSchema(liveExtensions());
    const extensions = createSchemaExtensions({});
    const schema = getSchema(extensions);
    expect(sortedKeys(schema.nodes)).toEqual(sortedKeys(live.nodes));
    expect(sortedKeys(schema.marks)).toEqual(sortedKeys(live.marks));
    const names = extensions.map((e) => e.name);
    for (const name of [
      "collaboration",
      "collaborationCaret",
      "commentsHighlight",
      "suggestionsHighlight",
    ]) {
      expect(names).not.toContain(name);
    }
  });

  it("the viewer schema differs from the live one only in code", () => {
    const live = getSchema(liveExtensions());
    const viewer = viewerSchema();
    expect(sortedKeys(viewer.nodes)).toEqual(sortedKeys(live.nodes));
    expect(sortedKeys(viewer.marks)).toEqual(
      [...sortedKeys(live.marks), "historyDelete", "historyInsert"].sort(),
    );
    expect(viewer.nodes.code_block?.spec.marks).toBe("historyInsert historyDelete");
    expect(viewer.marks.code?.spec.excludes).toBe("code");
    // The live editor's schema is untouched.
    expect(live.nodes.code_block?.spec.marks).toBe("");
    expect(live.marks.code?.spec.excludes).toBe("_");
  });
});
