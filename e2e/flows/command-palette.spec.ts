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

  // Seed a doc via the API to avoid the DocPage stale-title race.
  await page.evaluate(async () => {
    function readCookie(name: string): string | null {
      const m = document.cookie.match(new RegExp(`(?:^|; )${name}=([^;]*)`));
      return m && m[1] ? decodeURIComponent(m[1]) : null;
    }
    const csrf = readCookie("csrf") ?? "";
    const r = await fetch("/api/docs", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
      body: JSON.stringify({ title: "Findable" }),
    });
    if (!r.ok) throw new Error(`create: ${r.status}`);
  });

  await page.keyboard.press("Control+k");
  await expect(page.getByTestId("cmdk")).toBeVisible();
  await page.getByTestId("cmdk-input").fill("findable");

  // Plan 14 server-side search returns doc hits as cmdk-item-doc:<uuid>.
  const items = page.locator("[data-testid^='cmdk-item-doc:']");
  await expect(items).toHaveCount(1, { timeout: 5_000 });

  await page.keyboard.press("Enter");
  await expect(page.getByTestId("cmdk")).toHaveCount(0);
});
