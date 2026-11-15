/**
 * Classifies a past version's ProseMirror JSON for the read-only viewer.
 *
 * The viewer shows what ProseMirror can *parse*, valid or not, as the live
 * editor does: y-tiptap builds the live editor's nodes with `schema.node()`
 * and never calls `check()`, so shapes the old importer stored (text
 * straight in a list item, a quote or a cell) and empty containers a
 * concurrent delete left open normally there while `check()` rejects them.
 *
 * - `strict`: parses and passes `check()`.
 * - `lenient`: parses, `check()` rejects it; shown as it is, as structurally
 *   odd as it is live.
 * - `invalid`: the viewer's schema cannot parse it (a node or mark it does
 *   not have); "This version can't be shown."
 *
 * The viewer's editor is created with `enableContentCheck: false` either way
 * (the check already ran here). Without this own parse, Tiptap would turn a
 * parse error into an empty doc with only a console warning: a blank
 * version.
 */

import { getSchema, type JSONContent } from "@tiptap/core";
import type { Schema } from "@tiptap/pm/model";

import { createSchemaExtensions } from "../../editor/extensions";

import { historyDiffExtensions } from "./HistoryDiffExtension";

export type ViewerContent =
  | { mode: "strict" | "lenient"; content: JSONContent }
  | { mode: "invalid" };

let schema: Schema | null = null;

/** The viewer's schema: the live one with `code` excluding only itself, the
 *  diff marks allowed in `code_block`, and the history marks and attribute.
 *  Built once. */
export function viewerSchema(): Schema {
  schema ??= getSchema([...createSchemaExtensions({ viewer: true }), ...historyDiffExtensions]);
  return schema;
}

/**
 * An empty version (`content: []`, which `pm_json` returns for an empty
 * page as y-tiptap does) gets one empty paragraph: `doc` needs `block+`.
 */
function withMinimalContent(doc: unknown): unknown {
  if (typeof doc !== "object" || doc === null || Array.isArray(doc)) return doc;
  const { content } = doc as { content?: unknown };
  if (content == null || (Array.isArray(content) && content.length === 0)) {
    return { ...doc, content: [{ type: "paragraph" }] };
  }
  return doc;
}

export function viewerContent(doc: unknown): ViewerContent {
  const content = withMinimalContent(doc);
  let node;
  try {
    node = viewerSchema().nodeFromJSON(content);
  } catch {
    return { mode: "invalid" };
  }
  // Anything else as the root would not be a page.
  if (node.type.name !== "doc") return { mode: "invalid" };
  try {
    node.check();
  } catch {
    return { mode: "lenient", content: content as JSONContent };
  }
  return { mode: "strict", content: content as JSONContent };
}
