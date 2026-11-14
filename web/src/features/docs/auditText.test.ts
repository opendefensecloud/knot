import { describe, expect, it } from "vitest";

import type { AuditAuthor, DocAuditEvent } from "../../lib/validators";

import {
  actorLabel,
  categoryOfAction,
  describeAuditEvent,
  exportFormatShortLabel,
  formatBreakdown,
  otherActivityBreakdown,
  scopeSummary,
} from "./auditText";

const ADA: AuditAuthor = { key: "u-ada", name: "Ada", former: false, bot: false };
const CY: AuditAuthor = { key: "u-cy", name: "Cy", former: false, bot: false };
const FORMER: AuditAuthor = { key: null, name: null, former: true, bot: false };
const CI_BOT: AuditAuthor = { key: "u-ci", name: "CI bot", former: false, bot: true };

function auditEvent(
  action: string,
  details: Record<string, unknown> = {},
  overrides: Partial<DocAuditEvent> = {},
): DocAuditEvent {
  return {
    id: "1b2c3d4e5f607182",
    at: "2026-11-13T09:12:44Z",
    action,
    category: categoryOfAction(action),
    actor_kind: "user",
    actor: ADA,
    details,
    ...overrides,
  };
}

describe("actorLabel", () => {
  it("names a person, and a former or missing one as a former member", () => {
    expect(actorLabel(ADA, "user")).toBe("Ada");
    expect(actorLabel(FORMER, "user")).toBe("Former member");
    expect(actorLabel({ key: "u-x", name: null, former: false, bot: false }, "user")).toBe("Former member");
    expect(actorLabel(null, "user")).toBe("Former member");
  });

  it("marks a bot after its name, never a former member", () => {
    expect(actorLabel(CI_BOT, "user")).toBe("CI bot (bot)");
    expect(actorLabel({ ...FORMER, bot: true }, "user")).toBe("Former member");
  });

  it("labels each actor kind without a person", () => {
    expect(actorLabel(null, "admin_cli")).toBe("An administrator (command line)");
    expect(actorLabel(null, "retention")).toBe("Retention job");
    expect(actorLabel(null, "system")).toBe("System");
    expect(actorLabel(null, "something_new")).toBe("System");
  });
});

describe("describeAuditEvent — exports", () => {
  it("says which format a page export was in", () => {
    expect(describeAuditEvent(auditEvent("doc.export", { format: "docx", scope: "page" }))).toEqual({
      actor: "Ada",
      text: "exported this page as Word (.docx)",
    });
    expect(describeAuditEvent(auditEvent("doc.export", { format: "odt", scope: "page" })).text).toBe(
      "exported this page as OpenDocument (.odt)",
    );
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "markdown_zip", scope: "page" })).text,
    ).toBe("exported this page as Markdown (zip)");
  });

  it("counts the diagrams a Word or OpenDocument export carried", () => {
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "docx", scope: "page", diagram_pictures: 2 })).text,
    ).toBe("exported this page as Word (.docx), with 2 diagrams");
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "odt", scope: "page", diagram_pictures: 1 })).text,
    ).toBe("exported this page as OpenDocument (.odt), with 1 diagram");
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "docx", scope: "page", diagram_pictures: 0 })).text,
    ).toBe("exported this page as Word (.docx)");
  });

  it("says a subtree export on its root took the subpages too", () => {
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "markdown_zip", scope: "subtree" })).text,
    ).toBe("exported this page and its subpages as Markdown (zip)");
  });

  it("links the root of a subtree export the reader can open", () => {
    const sentence = describeAuditEvent(
      auditEvent("doc.export", {
        format: "markdown_zip",
        scope: "subtree",
        root: { id: "d-root", title: "Handbook" },
        root_hidden: false,
      }),
    );
    expect(sentence).toEqual({
      actor: "Ada",
      text: "exported this page as part of",
      link: { to: "/doc/d-root", title: "Handbook" },
      textAfterLink: "and its subpages",
    });
  });

  it("names no root the reader can't open", () => {
    const sentence = describeAuditEvent(
      auditEvent("doc.export", { format: "markdown_zip", scope: "subtree", root: null, root_hidden: true }),
    );
    expect(sentence.text).toBe("exported this page as part of a page you can't open and its subpages");
    expect(sentence.link).toBeUndefined();
  });

  it("says a workspace export included the page", () => {
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "markdown_zip", scope: "workspace" })).text,
    ).toBe("exported the whole workspace, including this page");
  });

  it("still reads when the format is unknown", () => {
    expect(describeAuditEvent(auditEvent("doc.export", {})).text).toBe("exported this page");
  });

  it("says when an export was made with an API key", () => {
    const viaKey = { via_api_key: true };
    expect(describeAuditEvent(auditEvent("doc.export", { format: "docx", scope: "page", ...viaKey })).text).toBe(
      "exported this page as Word (.docx), with an API key",
    );
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "odt", scope: "page", diagram_pictures: 1, ...viaKey }))
        .text,
    ).toBe("exported this page as OpenDocument (.odt), with 1 diagram, with an API key");
    expect(describeAuditEvent(auditEvent("doc.export", { format: "markdown_zip", scope: "workspace", ...viaKey })).text).toBe(
      "exported the whole workspace, including this page, with an API key",
    );
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "markdown_zip", scope: "subtree", ...viaKey })).text,
    ).toBe("exported this page and its subpages as Markdown (zip), with an API key");
    expect(
      describeAuditEvent(
        auditEvent("doc.export", { scope: "subtree", root: null, root_hidden: true, ...viaKey }),
      ).text,
    ).toBe("exported this page as part of a page you can't open and its subpages, with an API key");
    expect(
      describeAuditEvent(
        auditEvent("doc.export", {
          format: "markdown_zip",
          scope: "subtree",
          root: { id: "d-root", title: "Handbook" },
          root_hidden: false,
          ...viaKey,
        }),
      ),
    ).toEqual({
      actor: "Ada",
      text: "exported this page as part of",
      link: { to: "/doc/d-root", title: "Handbook" },
      textAfterLink: "and its subpages, with an API key",
    });
  });

  it("says nothing about a key unless via_api_key is true", () => {
    for (const viaApiKey of [false, "yes", 1, null]) {
      expect(
        describeAuditEvent(auditEvent("doc.export", { format: "docx", scope: "page", via_api_key: viaApiKey })).text,
      ).toBe("exported this page as Word (.docx)");
    }
  });

  it("names a bot that exported as a bot", () => {
    expect(
      describeAuditEvent(auditEvent("doc.export", { format: "docx", scope: "page", via_api_key: true }, { actor: CI_BOT })),
    ).toEqual({ actor: "CI bot (bot)", text: "exported this page as Word (.docx), with an API key" });
  });
});

describe("describeAuditEvent — access", () => {
  it("names who got which access", () => {
    const userPrincipal = { kind: "user", person: CY };
    expect(
      describeAuditEvent(auditEvent("doc.grant", { principal: userPrincipal, role: "editor", inherit: true })).text,
    ).toBe("gave Cy editor access");
    expect(
      describeAuditEvent(auditEvent("doc.grant", { principal: userPrincipal, role: "viewer", inherit: false })).text,
    ).toBe("gave Cy viewer access (this page only)");
    expect(
      describeAuditEvent(
        auditEvent("doc.grant", { principal: { kind: "group", name: "ops" }, role: "editor", inherit: true }),
      ).text,
    ).toBe("gave group ops editor access");
    expect(
      describeAuditEvent(
        auditEvent("doc.grant", { principal: { kind: "user", person: FORMER }, role: "owner", inherit: true }),
      ).text,
    ).toBe("gave a former member owner access");
  });

  it("marks a bot that got or lost access", () => {
    const botPrincipal = { kind: "user", person: CI_BOT };
    expect(
      describeAuditEvent(auditEvent("doc.grant", { principal: botPrincipal, role: "editor", inherit: true })).text,
    ).toBe("gave CI bot (bot) editor access");
    expect(describeAuditEvent(auditEvent("doc.grant.delete", { principal: botPrincipal })).text).toBe(
      "removed the access of CI bot (bot)",
    );
    // A person from a server without the mark is no bot.
    expect(
      describeAuditEvent(
        auditEvent("doc.grant", {
          principal: { kind: "user", person: { key: "u-cy", name: "Cy", former: false } },
          role: "viewer",
          inherit: true,
        }),
      ).text,
    ).toBe("gave Cy viewer access");
  });

  it("reads old grant rows without data", () => {
    expect(describeAuditEvent(auditEvent("doc.grant")).text).toBe("changed access to this page");
    expect(describeAuditEvent(auditEvent("doc.grant.delete")).text).toBe("removed access to this page");
  });

  it("names whose access was removed", () => {
    expect(
      describeAuditEvent(auditEvent("doc.grant.delete", { principal: { kind: "user", person: CY } })).text,
    ).toBe("removed Cy's access");
    expect(
      describeAuditEvent(auditEvent("doc.grant.delete", { principal: { kind: "group", name: "ops" } })).text,
    ).toBe("removed the access of group ops");
  });

  it("marks a break-glass grant", () => {
    const sentence = describeAuditEvent(
      auditEvent(
        "admin.grant",
        { principal: { kind: "user", person: CY }, role: "owner", inherit: false, break_glass: true },
        { actor: null, actor_kind: "admin_cli" },
      ),
    );
    expect(sentence).toEqual({
      actor: "An administrator (command line)",
      text: "granted Cy owner access (break-glass)",
    });
  });

  it("describes restriction changes", () => {
    expect(describeAuditEvent(auditEvent("doc.restrict")).text).toBe("restricted this page");
    expect(describeAuditEvent(auditEvent("doc.unrestrict")).text).toBe("lifted the restriction");
  });
});

describe("describeAuditEvent — changes", () => {
  it.each([
    ["doc.create", "created this page"],
    ["doc.rename", "renamed this page"],
    ["doc.move", "moved this page"],
    ["doc.duplicate", "created this page as a copy"],
    ["doc.archive", "moved this page to the archive"],
    ["doc.restore", "restored this page"],
    ["doc.purge", "deleted this page for good"],
    ["doc.template.mark", "saved this page as a template"],
    ["doc.template.unmark", "removed it from templates"],
  ])("%s reads %s", (action, text) => {
    expect(describeAuditEvent(auditEvent(action)).text).toBe(text);
  });

  it("names the retention job on an automatic purge", () => {
    expect(describeAuditEvent(auditEvent("doc.purge", {}, { actor: null, actor_kind: "retention" }))).toEqual({
      actor: "Retention job",
      text: "deleted this page for good",
    });
  });

  it("shows an unknown action as it is", () => {
    expect(describeAuditEvent(auditEvent("doc.future_thing", {}, { actor_kind: "system", actor: null }))).toEqual({
      actor: "System",
      text: "doc.future_thing",
    });
  });
});

describe("stats text", () => {
  it("gives every recorded action one category", () => {
    expect(categoryOfAction("doc.export")).toBe("exports");
    for (const action of ["doc.grant", "doc.grant.delete", "admin.grant", "doc.restrict", "doc.unrestrict"]) {
      expect(categoryOfAction(action), action).toBe("access");
    }
    for (const action of [
      "doc.create",
      "doc.rename",
      "doc.move",
      "doc.duplicate",
      "doc.archive",
      "doc.restore",
      "doc.purge",
      "doc.template.mark",
      "doc.template.unmark",
    ]) {
      expect(categoryOfAction(action), action).toBe("changes");
    }
    expect(categoryOfAction("doc.future")).toBe("other");
  });

  it("labels the export formats briefly", () => {
    expect(exportFormatShortLabel("docx")).toBe("Word");
    expect(exportFormatShortLabel("odt")).toBe("OpenDocument");
    expect(exportFormatShortLabel("markdown_zip")).toBe("Markdown (zip)");
    expect(exportFormatShortLabel(null)).toBe("Unknown format");
  });

  it("lists formats in a fixed order and leaves zero counts out", () => {
    expect(formatBreakdown({ markdown_zip: 3, odt: 0, docx: 5 })).toEqual([
      { format: "docx", label: "Word", count: 5 },
      { format: "markdown_zip", label: "Markdown (zip)", count: 3 },
    ]);
    expect(formatBreakdown({ pdf: 1 })).toEqual([{ format: "pdf", label: "pdf", count: 1 }]);
  });

  it("says how many exports came with a subpage or workspace export, only when some did", () => {
    expect(scopeSummary({ page: 7 })).toBeNull();
    expect(scopeSummary({})).toBeNull();
    expect(scopeSummary({ page: 7, subtree: 1, workspace: 1 })).toBe(
      "1 as part of a subpage export · 1 in a workspace export",
    );
    expect(scopeSummary({ subtree: 2 })).toBe("2 as part of a subpage export");
  });

  it("sums the other activity by category", () => {
    expect(
      otherActivityBreakdown([
        { action: "doc.export", count: 9, last_at: "2026-11-13T09:12:44Z" },
        { action: "doc.grant", count: 2, last_at: "2026-11-13T09:12:44Z" },
        { action: "doc.restrict", count: 1, last_at: "2026-11-13T09:12:44Z" },
        { action: "doc.rename", count: 25, last_at: "2026-11-13T09:12:44Z" },
        { action: "doc.create", count: 1, last_at: "2026-11-13T09:12:44Z" },
        { action: "doc.future", count: 4, last_at: "2026-11-13T09:12:44Z" },
      ]),
    ).toEqual([
      { category: "access", label: "Access changes", count: 3 },
      { category: "changes", label: "Changes", count: 26 },
      { category: "other", label: "Other", count: 4 },
    ]);
    expect(otherActivityBreakdown([{ action: "doc.export", count: 9, last_at: "2026-11-13T09:12:44Z" }])).toEqual(
      [],
    );
  });
});
