import { expect, test } from "@playwright/test";

import { reset } from "../support/reset";

const EMAIL = "owner@tokens.test";
const PASSWORD = "hunter22!tokens";

test.beforeAll(async () => {
  reset();
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

test("mint, use and revoke an API token from Settings", async ({ page, request }) => {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  await page.goto("/settings");
  const panel = page.getByTestId("api-tokens");
  await expect(panel).toBeVisible();
  await expect(page.getByTestId("api-tokens-empty")).toBeVisible();

  // A name and at least one permission are required before minting.
  const create = page.getByTestId("api-token-create");
  await expect(create).toBeDisabled();
  await page.getByTestId("api-token-name").fill("e2e-bot");
  await expect(create).toBeEnabled();
  await create.click();

  // The secret is shown once; grab it before dismissing the box.
  const secretBox = page.getByTestId("api-token-secret");
  await expect(secretBox).toBeVisible();
  const secret = (await secretBox.locator("code").first().textContent())!.trim();
  expect(secret).toMatch(/^knot_/);

  // It really works: create a page over the API with no cookies at all.
  const created = await request.post("http://localhost:3000/api/docs", {
    headers: { authorization: `Bearer ${secret}` },
    data: { title: "Made by the bot" },
  });
  expect(created.status()).toBe(201);

  // ... and it cannot delete (delete scope was not granted).
  const docId = ((await created.json()) as { id: string }).id;
  const refused = await request.delete(`http://localhost:3000/api/docs/${docId}`, {
    headers: { authorization: `Bearer ${secret}` },
  });
  expect(refused.status()).toBe(403);

  await secretBox.getByText("Done").click();
  await expect(secretBox).not.toBeVisible();

  // The list shows the token, its scopes and its last use (the list was
  // fetched before the calls above, so reload to pick up the timestamp).
  await page.reload();
  const row = panel.locator("li", { hasText: "e2e-bot" });
  await expect(row).toContainText("read, create, write");
  await expect(row).toContainText("last used");

  // Revoking asks first, then the token stops working.
  page.once("dialog", (dialog) => void dialog.accept());
  await row.getByRole("button", { name: "Revoke" }).click();
  await expect(row).toContainText("revoked");

  const afterRevoke = await request.get("http://localhost:3000/api/docs", {
    headers: { authorization: `Bearer ${secret}` },
  });
  expect(afterRevoke.status()).toBe(401);
});

test("a token minted over the API shows up in the list", async ({ page }) => {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);

  // The old way still works: the CSRF cookie is readable from the page.
  const minted = await page.evaluate(async () => {
    const csrf = document.cookie.split("; ").find((c) => c.startsWith("csrf="))?.split("=")[1];
    const r = await fetch("/api/tokens", {
      method: "POST",
      headers: { "content-type": "application/json", "X-CSRF-Token": csrf ?? "" },
      body: JSON.stringify({ name: "minted-by-curl", scopes: ["read"] }),
    });
    return { status: r.status, body: await r.json() };
  });
  expect(minted.status).toBe(201);

  await page.goto("/settings");
  const panel = page.getByTestId("api-tokens");
  const row = panel.locator("li", { hasText: "minted-by-curl" });
  await expect(row).toContainText("read");
  await expect(row).toContainText("never used");
});
