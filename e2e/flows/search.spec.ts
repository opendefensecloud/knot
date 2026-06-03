import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots","doc_updates",
    "document_grants","documents","sessions","workspace_members","users","workspaces",
    "blobs","blob_bytes",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}
test.beforeAll(reset);

test("Cmd+K search finds a doc by title and navigates to it", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  // Seed three docs by hitting the API directly so we avoid the DocPage
  // stale-title-state bug (real SPA issue tracked separately).
  await page.evaluate(async () => {
    function readCookie(name: string): string | null {
      const m = document.cookie.match(new RegExp(`(?:^|; )${name}=([^;]*)`));
      return m && m[1] ? decodeURIComponent(m[1]) : null;
    }
    const csrf = readCookie("csrf") ?? "";
    for (const title of ["Findable Alpha", "Other Beta", "Some Gamma"]) {
      const r = await fetch("/api/docs", {
        method: "POST",
        credentials: "include",
        headers: { "Content-Type": "application/json", "X-CSRF-Token": csrf },
        body: JSON.stringify({ title }),
      });
      if (!r.ok) throw new Error(`create ${title}: ${r.status}`);
    }
  });

  // Open the palette and search.
  await page.keyboard.press("Control+k");
  await expect(page.getByTestId("cmdk")).toBeVisible();
  await page.getByTestId("cmdk-input").fill("findable");

  // Server-driven hit appears via the searchApi round-trip.
  const hit = page.locator("[data-testid^='cmdk-item-doc:']").first();
  await expect(hit).toContainText("Findable", { timeout: 5_000 });

  await page.keyboard.press("Enter");
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("doc-title")).toHaveValue("Findable Alpha");
});
