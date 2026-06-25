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

const TOXIPROXY = "http://localhost:8474";

async function setProxyEnabled(enabled: boolean): Promise<void> {
  // Toggling enabled=false on a proxy force-closes all live connections
  // and rejects new ones until re-enabled. Cleaner than reset_peer for
  // testing WS lifecycle since reset_peer only affects NEW connections.
  const r = await fetch(`${TOXIPROXY}/proxies/knot`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ enabled }),
  });
  if (!r.ok) throw new Error(`toxiproxy enabled=${enabled}: ${r.status} ${await r.text()}`);
}

test.beforeAll(async () => {
  reset();
  await setProxyEnabled(true);
});
test.afterEach(async () => { await setProxyEnabled(true); });

test("editor reconnects after a forced WS flap; content preserved", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("Before the flap.");
  await page.waitForTimeout(300);

  // Disable the proxy: closes every live connection immediately.
  await setProxyEnabled(false);
  // KnotProvider's onclose fires within a few ms.
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "offline", { timeout: 5_000 });

  // Restore. KnotProvider.scheduleReconnect kicks in.
  await setProxyEnabled(true);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  // Y.Doc never lost state.
  await expect(editor).toContainText("Before the flap.");
});
