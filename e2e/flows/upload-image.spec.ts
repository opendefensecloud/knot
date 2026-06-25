import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots",
    "doc_updates","document_grants","documents","sessions","workspace_members",
    "users","workspaces","blobs","blob_bytes",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}
test.beforeAll(reset);

// 70-byte transparent 1x1 PNG.
const TINY_PNG_HEX =
  "89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000d4944415478da6360600000000004000" +
  "15c5b66e30000000049454e44ae426082";

test("drop a PNG → renders as <img>, reload preserves", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  const url = page.url();
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });

  // Synthesize a drop on the ProseMirror surface via a DataTransfer.
  await page.evaluate(async (hex) => {
    const bytes = new Uint8Array(hex.match(/.{2}/g)!.map((h) => parseInt(h, 16)));
    const file = new File([bytes], "tiny.png", { type: "image/png" });
    const dt = new DataTransfer();
    dt.items.add(file);
    const editor = document.querySelector("[data-testid='editor-host'] .ProseMirror") as HTMLElement;
    const rect = editor.getBoundingClientRect();
    editor.dispatchEvent(new DragEvent("drop", {
      bubbles: true,
      cancelable: true,
      dataTransfer: dt,
      clientX: rect.left + 10,
      clientY: rect.top + 10,
    }));
  }, TINY_PNG_HEX);

  // Wait for the upload to round-trip and the editor to render the img.
  const img = page.locator("[data-testid='editor-host'] img").first();
  await expect(img).toBeVisible({ timeout: 8_000 });
  const src = await img.getAttribute("src");
  expect(src).toMatch(/^\/api\/blobs\//);

  // Give the writer task a beat to persist before reloading.
  await page.waitForTimeout(800);
  await page.goto(url);
  await expect(page.locator("[data-testid='editor-host'] img").first()).toBeVisible({
    timeout: 10_000,
  });
});
