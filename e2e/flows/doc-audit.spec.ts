// Page audit log: every file export is recorded with the person who made it
// and shows on the audit log of each page the file contains. A viewer can
// export but can't open the log; an editor opens it from the page header or
// the sidebar's context menu and sees who exported the page, how and when.
//
// The branch adds a migration (20261113120000_audit_doc_index), so run it
// locally against a per-run database (KNOT_E2E_DB, see support/reset.ts),
// not the shared `knot` one.
import { expect, test, type Locator, type Page } from "@playwright/test";

import { reset } from "../support/reset";

import { seedPages, setupOwner, writeHeaders } from "./tree-helpers";

test.beforeAll(reset);

const PARENT_TITLE = "Audit Handbook";
const CHILD_TITLE = "Audit Chapter";

const EDITOR_EMAIL = "eddie@audit.test";
const EDITOR_NAME = "Eddie";
const EDITOR_PASSWORD = "eddie-hunter22";
const VIEWER_EMAIL = "vera@audit.test";
const VIEWER_NAME = "Vera";
const VIEWER_PASSWORD = "vera-hunter22";

async function signIn(page: Page, email: string, password: string): Promise<void> {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(email);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("doc-tree")).toBeVisible({ timeout: 10_000 });
}

async function invite(owner: Page, email: string, displayName: string, role: string, password: string) {
  const memberRows = owner.locator("[data-testid^='member-']");
  await owner.goto("/members");
  await expect(memberRows.first()).toBeVisible({ timeout: 5_000 });
  const membersBefore = await memberRows.count();
  await owner.getByTestId("invite-email").fill(email);
  await owner.getByTestId("invite-display-name").fill(displayName);
  await owner.getByTestId("invite-role").selectOption(role);
  await owner.getByTestId("invite-password").fill(password);
  await owner.getByTestId("invite-submit").click();
  // A hard navigation next would abort the invite's request; wait for the
  // members list to show it first.
  await expect(memberRows).toHaveCount(membersBefore + 1, { timeout: 5_000 });
}

/** Opens a page and waits for its collab socket, which the exports wait for. */
async function openConnected(page: Page, docId: string): Promise<void> {
  await page.goto(`/doc/${docId}`);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
}

/** Picks an item of the page header's export menu and waits for the file. */
async function exportFromMenu(page: Page, item: "export-docx" | "export-md-subtree"): Promise<string> {
  await page.getByTestId("doc-export").click();
  const downloaded = page.waitForEvent("download");
  await page.getByTestId(item).click();
  return (await downloaded).suggestedFilename();
}

/** A stat tile's value (its `<dd>`, without the label) is exactly `expected`. */
async function expectTileNumber(tile: Locator, expected: number): Promise<void> {
  await expect(tile).toHaveText(String(expected));
}

test("exports are recorded with who made them and editors read them on the page's audit log", async ({
  browser,
}) => {
  test.setTimeout(120_000);

  // The owner sets up the workspace, seeds a page with a subpage and invites
  // an editor and a viewer.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await setupOwner(owner);
  const ids = await seedPages(owner.request, [
    { title: PARENT_TITLE },
    { title: CHILD_TITLE, parent: PARENT_TITLE },
  ]);
  const parentId = ids[PARENT_TITLE];
  const childId = ids[CHILD_TITLE];
  const imported = await owner.request.post(`/api/docs/${parentId}/markdown?mode=replace`, {
    headers: { ...(await writeHeaders(owner.request)), "Content-Type": "text/markdown" },
    data: `# ${PARENT_TITLE}\n\nWhat every new member reads first.\n`,
  });
  expect(imported.status()).toBe(204);
  await invite(owner, EDITOR_EMAIL, EDITOR_NAME, "editor", EDITOR_PASSWORD);
  await invite(owner, VIEWER_EMAIL, VIEWER_NAME, "viewer", VIEWER_PASSWORD);

  // The viewer exports the page as Word. The export menu tells them exports
  // are recorded; the page has no audit log entry point for them, and its URL
  // explains that only editors and owners can read it.
  const viewerCtx = await browser.newContext();
  const viewer = await viewerCtx.newPage();
  await signIn(viewer, VIEWER_EMAIL, VIEWER_PASSWORD);
  await openConnected(viewer, parentId);
  await expect(viewer.getByTestId("open-audit")).toHaveCount(0);
  await viewer.getByTestId("doc-export").click();
  await expect(viewer.getByTestId("export-audit-note")).toHaveText(
    "Exports are recorded in the page's audit log.",
  );
  await viewer.keyboard.press("Escape");
  await expect(viewer.getByRole("menu")).toHaveCount(0);
  expect(await exportFromMenu(viewer, "export-docx")).toBe(`${PARENT_TITLE}.docx`);

  await viewer.getByTestId(`doc-row-${parentId}`).click({ button: "right" });
  await expect(viewer.getByTestId("ctx-audit")).toHaveCount(0);
  await viewer.keyboard.press("Escape");

  await viewer.goto(`/doc/${parentId}/audit`);
  await expect(viewer.getByTestId("audit-forbidden")).toBeVisible({ timeout: 10_000 });
  await expect(viewer.getByTestId("audit-events")).toHaveCount(0);

  // The editor opens the log from the page header: one export, by the
  // viewer, as Word.
  const editorCtx = await browser.newContext();
  const editor = await editorCtx.newPage();
  await signIn(editor, EDITOR_EMAIL, EDITOR_PASSWORD);
  await editor.goto(`/doc/${parentId}`);
  await expect(editor.getByTestId("doc-page")).toBeVisible({ timeout: 10_000 });
  await editor.getByTestId("open-audit").click();
  await editor.waitForURL(new RegExp(`/doc/${parentId}/audit$`));
  await expect(editor.getByTestId("doc-audit-page")).toBeVisible();
  await expect(editor.getByTestId("audit-stats")).toBeVisible({ timeout: 10_000 });
  await expectTileNumber(editor.getByTestId("audit-stat-exports"), 1);
  await expectTileNumber(editor.getByTestId("audit-stat-people"), 1);
  await expectTileNumber(editor.getByTestId("audit-stat-30d"), 1);
  await expect(editor.getByTestId("audit-stat-last")).toContainText(VIEWER_NAME);
  await expect(editor.getByTestId("audit-exporters")).toContainText(VIEWER_NAME);
  await expect(editor.getByTestId("audit-exports-empty")).toHaveCount(0);

  const viewerExport = editor.locator("[data-testid='audit-event'][data-action='doc.export']");
  await expect(viewerExport).toHaveCount(1);
  await expect(viewerExport).toContainText(VIEWER_NAME);
  await expect(viewerExport).toContainText("Word");
  // The page's creation is on the log too, under the owner's name.
  await expect(editor.locator("[data-testid='audit-event'][data-action='doc.create']")).toHaveCount(1);
  await expect(editor.getByTestId("audit-scope-note")).toContainText("Not recorded");

  // The owner exports the page with its subpages as Markdown.
  await openConnected(owner, parentId);
  expect(await exportFromMenu(owner, "export-md-subtree")).toBe(`${PARENT_TITLE}.zip`);

  // The editor reopens the log, this time from the sidebar's context menu:
  // two exports by two people. The Exports filter lists only exports, the
  // owner's as the page and its subpages.
  await editor.goto(`/doc/${parentId}`);
  await expect(editor.getByTestId("doc-page")).toBeVisible({ timeout: 10_000 });
  await editor.getByTestId(`doc-row-${parentId}`).click({ button: "right" });
  await editor.getByTestId("ctx-audit").click();
  await editor.waitForURL(new RegExp(`/doc/${parentId}/audit$`));
  await expect(editor.getByTestId("audit-stats")).toBeVisible({ timeout: 10_000 });
  await expectTileNumber(editor.getByTestId("audit-stat-exports"), 2);
  await expectTileNumber(editor.getByTestId("audit-stat-people"), 2);

  const exportsFilter = editor.getByTestId("audit-filter-exports");
  await exportsFilter.click();
  await expect(exportsFilter).toHaveAttribute("aria-pressed", "true");
  await expect(editor.getByTestId("audit-event")).toHaveCount(2);
  await expect(editor.locator("[data-testid='audit-event']:not([data-action='doc.export'])")).toHaveCount(0);
  await expect(editor.getByTestId("audit-event").first()).toContainText("subpages");
  await expect(editor.getByTestId("audit-load-more")).toHaveCount(0);

  // The subpage's log has the owner's export as part of the parent's, with
  // a link back to it.
  await editor.goto(`/doc/${childId}`);
  await expect(editor.getByTestId("doc-page")).toBeVisible({ timeout: 10_000 });
  await editor.getByTestId("open-audit").click();
  await editor.waitForURL(new RegExp(`/doc/${childId}/audit$`));
  const subtreeExport = editor.locator("[data-testid='audit-event'][data-action='doc.export']");
  await expect(subtreeExport).toHaveCount(1, { timeout: 10_000 });
  await expect(subtreeExport).toContainText("as part of");
  await expect(subtreeExport.getByRole("link", { name: PARENT_TITLE })).toHaveAttribute(
    "href",
    `/doc/${parentId}`,
  );
  await expectTileNumber(editor.getByTestId("audit-stat-exports"), 1);

  await ownerCtx.close();
  await viewerCtx.close();
  await editorCtx.close();
});
