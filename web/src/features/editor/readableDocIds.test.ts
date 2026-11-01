/**
 * The readable set behind the locked doc-link style: templates and archived
 * pages the user can open aren't in GET /api/docs, so they come from their
 * own lists, and a link to one isn't drawn as a page they can't open.
 */

import { describe, expect, it } from "vitest";

import { combineReadableDocIds } from "./readableDocIds";

const failed = { error: { code: "acl.editor_required", message: "", details: {}, status: 403 } };

describe("combineReadableDocIds", () => {
  it("counts templates and archived pages as readable", () => {
    const readable = combineReadableDocIds([
      { ok: [{ id: "page" }] },
      { ok: [{ id: "template" }] },
      { ok: [{ id: "archived" }] },
    ]);
    expect(readable).toEqual(new Set(["page", "template", "archived"]));
  });

  it("counts the archived subpages the archive says the user can open", () => {
    const readable = combineReadableDocIds([
      { ok: [] },
      { ok: [] },
      { ok: [{ id: "archived", subpage_ids: ["archived-child", "archived-grandchild"] }] },
    ]);
    expect(readable).toEqual(new Set(["archived", "archived-child", "archived-grandchild"]));
  });

  it("is unknown while a list is still on its way", () => {
    expect(combineReadableDocIds([{ ok: [{ id: "page" }] }, undefined, "skipped"])).toBeNull();
  });

  it("skips a list that failed or wasn't asked for", () => {
    expect(combineReadableDocIds([{ ok: [{ id: "page" }] }, failed, "skipped"])).toEqual(new Set(["page"]));
  });
});
