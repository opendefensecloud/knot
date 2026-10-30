import { describe, expect, it } from "vitest";

import { Doc, DocAccessInfo, DocWithRole, parse } from "./validators";

/** A sidebar row as servers before restricted documents sent it. */
const LEGACY_DOC = {
  id: "d1",
  workspace_id: "w1",
  parent_id: null,
  title: "Plan",
  sort_key: "n",
  icon: null,
  created_by: "u1",
  archived: false,
};

describe("Doc", () => {
  it("parses a row without the restriction fields, with defaults", () => {
    const doc = parse(Doc, LEGACY_DOC);

    expect(doc.restricted).toBe(false);
    expect(doc.within_restricted).toBe(false);
    expect(doc.parent_hidden).toBe(false);
    expect(doc.effective_role).toBeNull();
  });

  it("keeps the restriction fields the server sends", () => {
    const doc = parse(Doc, {
      ...LEGACY_DOC,
      restricted: true,
      within_restricted: true,
      parent_hidden: true,
      effective_role: "editor",
    });

    expect(doc).toMatchObject({
      restricted: true,
      within_restricted: true,
      parent_hidden: true,
      effective_role: "editor",
    });
  });
});

describe("DocWithRole", () => {
  it("defaults the restriction fields", () => {
    const doc = parse(DocWithRole, { ...LEGACY_DOC, effective_role: "owner" });

    expect(doc.restricted).toBe(false);
    expect(doc.within_restricted).toBe(false);
    expect(doc.restriction_root_id).toBeNull();
  });
});

describe("DocAccessInfo", () => {
  it("parses the /access answer, hidden sources included", () => {
    const body = {
      restricted: false,
      restriction_root_id: null,
      restriction_root_title: null,
      people: [
        { principal: "user:u1", role: "owner", inherit: true, source_doc_id: null, source_title: null },
        { principal: "user:u2", role: "viewer", inherit: false, source_doc_id: "d1", source_title: "Plan" },
      ],
    };

    expect(parse(DocAccessInfo, body)).toEqual(body);
  });
});
