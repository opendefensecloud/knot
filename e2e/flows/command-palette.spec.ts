import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations",
    "audit_events",
    "doc_markdown_cache",
    "doc_snapshots",
    "doc_updates",
    "document_grants",
    "documents",
    "sessions",
    "workspace_members",
    "users",
    "workspaces",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}
test.beforeAll(reset);

test("Ctrl+K opens palette, search filters, Enter navigates", async ({
  page,
}) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const input = page.locator("[data-testid='doc-title']");
  await expect(input).toHaveValue("Untitled");
  const patch = page.waitForResponse(
    (r) => r.url().includes("/api/docs/") && r.request().method() === "PATCH",
  );
  await input.fill("Findable");
  await input.blur();
  await patch;

  await page.keyboard.press("Control+k");
  await expect(page.getByTestId("cmdk")).toBeVisible();
  await page.getByTestId("cmdk-input").fill("findable");

  const items = page.locator("[data-testid^='cmdk-item-doc:']");
  await expect(items).toHaveCount(1, { timeout: 5_000 });

  await page.keyboard.press("Enter");
  await expect(page.getByTestId("cmdk")).toHaveCount(0);
});
