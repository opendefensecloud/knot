// Restricted pages: an owner restricts a page from its permissions dialog,
// and from then on only the people granted access can see it and its
// subpages, whatever their workspace role. A member granted only a subpage
// finds it under "Shared with you", without ever learning the restricted
// parent's title.
import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

import { seedPages, setupOwner, showTree } from "./tree-helpers";

test.beforeAll(reset);

const PARENT_TITLE = "Board Minutes";
const CHILD_TITLE = "Hiring Plan";
const NO_ACCESS_COPY = "This page doesn't exist or you don't have access to it.";

type ApiMember = { user_id: string; email: string };

async function signIn(page: Page, email: string, password: string): Promise<void> {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(email);
  await page.getByTestId("login-password").fill(password);
  await page.getByTestId("login-submit").click();
  await expect(page.getByTestId("doc-tree")).toBeVisible({ timeout: 10_000 });
}

test("restricting hides a page from workspace editors; a subpage grant shows under Shared with you", async ({
  browser,
}) => {
  // The owner sets up the workspace, seeds P with child C and invites an
  // editor.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await setupOwner(owner);
  const ids = await seedPages(owner.request, [
    { title: PARENT_TITLE },
    { title: CHILD_TITLE, parent: PARENT_TITLE },
  ]);

  await owner.goto("/members");
  await owner.getByTestId("invite-email").fill("ed@e.com");
  await owner.getByTestId("invite-role").selectOption("editor");
  await owner.getByTestId("invite-password").fill("editor-hunter22");
  await owner.getByTestId("invite-submit").click();
  await expect(owner.locator("[data-testid^='member-']")).toHaveCount(2, { timeout: 5_000 });
  const membersRes = await owner.request.get("/api/workspace/members");
  expect(membersRes.status()).toBe(200);
  const editorMember = ((await membersRes.json()) as ApiMember[]).find(
    (member) => member.email === "ed@e.com",
  );
  expect(editorMember, "the invited editor is a member").toBeTruthy();
  const editorPrincipal = `user:${editorMember!.user_id}`;

  // Before the restriction, the editor sees both pages through their
  // workspace role.
  const editorCtx = await browser.newContext();
  const editor = await editorCtx.newPage();
  await signIn(editor, "ed@e.com", "editor-hunter22");
  await showTree(editor, [ids[PARENT_TITLE]]);

  // The owner restricts P from its permissions dialog, confirming the prompt.
  await owner.goto(`/doc/${ids[PARENT_TITLE]}/permissions`);
  await expect(owner.getByTestId("permissions-dialog")).toBeVisible();
  const toggle = owner.getByTestId("restricted-toggle");
  await expect(toggle).toHaveAttribute("aria-checked", "false");
  await expect(toggle).toBeEnabled();
  owner.once("dialog", (dialog) => void dialog.accept());
  const restrictRes = owner.waitForResponse(
    (r) =>
      r.request().method() === "PUT" &&
      new URL(r.url()).pathname === `/api/docs/${ids[PARENT_TITLE]}/restricted`,
  );
  await toggle.click();
  expect((await restrictRes).status()).toBe(200);
  await expect(toggle).toHaveAttribute("aria-checked", "true");
  // Restricting made the owner an owner of the page, listed under "People
  // with access".
  await expect(owner.locator("[data-testid^='access-user:']")).toHaveCount(1);
  await owner.goto(`/doc/${ids[PARENT_TITLE]}`);
  await expect(owner.getByTestId("restricted-chip")).toBeVisible({ timeout: 10_000 });
  await expect(
    owner.getByTestId(`tree-row-${ids[PARENT_TITLE]}`).getByRole("img", { name: "Restricted", exact: true }),
  ).toBeVisible();

  // The editor's sidebar no longer lists P or C, and P's URL answers like a
  // page that doesn't exist.
  await editor.goto("/");
  await expect(editor.getByTestId("doc-tree")).toBeVisible({ timeout: 10_000 });
  await expect(editor.getByTestId(`doc-row-${ids[PARENT_TITLE]}`)).toHaveCount(0);
  await expect(editor.getByTestId(`doc-row-${ids[CHILD_TITLE]}`)).toHaveCount(0);
  await expect(editor.getByTestId("tree-shared")).toHaveCount(0);
  await editor.goto(`/doc/${ids[PARENT_TITLE]}`);
  await expect(editor.getByText(NO_ACCESS_COPY)).toBeVisible({ timeout: 10_000 });
  await expect(editor.locator("body")).not.toContainText(PARENT_TITLE);

  // The owner grants the editor Viewer on C alone, without inheritance.
  await owner.goto(`/doc/${ids[CHILD_TITLE]}/permissions`);
  await expect(owner.getByTestId("permissions-dialog")).toBeVisible();
  await owner.getByTestId("grant-user").selectOption(editorMember!.user_id);
  await owner.getByTestId("grant-role").selectOption("viewer");
  await owner.getByLabel("Inherit").uncheck();
  const grantRes = owner.waitForResponse(
    (r) =>
      r.request().method() === "PUT" &&
      new URL(r.url()).pathname.startsWith(`/api/docs/${ids[CHILD_TITLE]}/grants/`),
  );
  await owner.getByTestId("grant-add").click();
  expect((await grantRes).status()).toBe(204);
  await expect(owner.getByTestId(`access-${editorPrincipal}`)).toBeVisible({ timeout: 5_000 });

  // The editor finds C under "Shared with you", P nowhere: not in the
  // sidebar, and not its title anywhere on the page.
  await editor.goto("/");
  const shared = editor.getByTestId("tree-shared");
  await expect(shared).toBeVisible({ timeout: 10_000 });
  await expect(shared).toContainText("Shared with you");
  await expect(shared.getByTestId(`doc-row-${ids[CHILD_TITLE]}`)).toBeVisible();
  await expect(editor.getByTestId(`doc-row-${ids[PARENT_TITLE]}`)).toHaveCount(0);
  await expect(editor.locator("body")).not.toContainText(PARENT_TITLE);

  // Opening C shows it read-only as restricted, still without P's title.
  await shared.getByTestId(`doc-row-${ids[CHILD_TITLE]}`).click();
  await editor.waitForURL(new RegExp(`/doc/${ids[CHILD_TITLE]}$`));
  await expect(editor.getByTestId("doc-title")).toHaveValue(CHILD_TITLE, { timeout: 10_000 });
  await expect(editor.getByTestId("restricted-chip")).toBeVisible();
  await expect(editor.locator("body")).not.toContainText(PARENT_TITLE);

  await ownerCtx.close();
  await editorCtx.close();
});
