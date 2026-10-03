import { expect, test, type Page } from "@playwright/test";

import { docText } from "../support/docText";
import { reset } from "../support/reset";

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

// Per test: each one bootstraps through /setup, which only works on an empty
// database.
test.beforeEach(async () => {
  reset();
  await setProxyEnabled(true);
});
test.afterEach(async () => { await setProxyEnabled(true); });

async function setupAndOpenBlankDoc(page: Page): Promise<void> {
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
}

test("editor reconnects after a forced WS flap; content preserved", async ({ page }) => {
  await setupAndOpenBlankDoc(page);

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

test("edits typed while disconnected reach the server after reconnect", async ({ page, context }) => {
  await setupAndOpenBlankDoc(page);
  const docUrl = page.url();

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("Before the outage.");
  await page.waitForTimeout(300);

  await setProxyEnabled(false);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "offline", { timeout: 5_000 });

  // The editor stays editable while offline; these keystrokes exist only in
  // this tab's Y.Doc until the connection comes back.
  await page.keyboard.type(" Typed offline.");
  await expect(editor).toContainText("Typed offline.");

  await setProxyEnabled(true);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  // A second tab loads the doc from the server: it only sees the offline
  // text if the reconnecting tab uploaded it.
  const second = await context.newPage();
  await second.goto(docUrl);
  const secondEditor = second.locator("[data-testid='editor-host'] .ProseMirror");
  await expect.poll(() => secondEditor.evaluate(docText), { timeout: 10_000 }).toMatch(/Before the outage\. Typed offline\./);
});
