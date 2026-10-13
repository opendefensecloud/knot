import { describe, expect, it } from "vitest";

import { archiveConfirmText, archiveErrorText, daysUntil, purgeConfirmText } from "./archive";

describe("archiveConfirmText", () => {
  it("names the doc, its subpages and the retention period", () => {
    expect(archiveConfirmText("Folder", 2, 30)).toBe(
      'Move "Folder" to the archive?\n\n' +
        "Its 2 subpages go with it. " +
        "It will be permanently deleted in 30 days unless restored.",
    );
  });

  it("uses the singular for one subpage and one day", () => {
    expect(archiveConfirmText("Folder", 1, 1)).toBe(
      'Move "Folder" to the archive?\n\n' +
        "Its 1 subpage goes with it. " +
        "It will be permanently deleted in 1 day unless restored.",
    );
  });

  it("leaves out the subpage sentence for a page without subpages", () => {
    expect(archiveConfirmText("Page", 0, 30)).toBe(
      'Move "Page" to the archive?\n\n' +
        "It will be permanently deleted in 30 days unless restored.",
    );
  });
});

describe("archiveErrorText", () => {
  it("explains permission and state errors instead of a bare failure", () => {
    expect(archiveErrorText("acl.editor_required")).toBe("You don't have permission to do that.");
    expect(archiveErrorText("acl.owner_required")).toBe("You don't have permission to do that.");
    expect(archiveErrorText("doc.archived")).toBe("It is already in the archive.");
    expect(archiveErrorText("internal")).toBe("Couldn't move it to the archive.");
  });
});

describe("daysUntil", () => {
  const now = new Date("2026-10-13T12:00:00Z");

  it("rounds up partial days so a doc never shows 0 days while it still exists", () => {
    expect(daysUntil("2026-10-14T11:00:00Z", now)).toBe(1);
    expect(daysUntil("2026-11-12T12:00:00Z", now)).toBe(30);
  });

  it("never goes negative for a doc the purge job hasn't reached yet", () => {
    expect(daysUntil("2026-10-12T12:00:00Z", now)).toBe(0);
  });
});

describe("purgeConfirmText", () => {
  it("names what goes and that it can't be undone", () => {
    expect(purgeConfirmText("Folder", 2)).toBe(
      'Permanently delete "Folder" and its 2 subpages?\n\nThis can\'t be undone.',
    );
    expect(purgeConfirmText("Page", 0)).toBe(
      'Permanently delete "Page"?\n\nThis can\'t be undone.',
    );
  });
});
