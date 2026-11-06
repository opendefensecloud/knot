// Scoped API keys end to end: create in Settings, use as a Bearer token,
// revoke, lose on password change, bot keys, and the TTL floor.
//
// The scenarios share one signed-in owner and run in order. Plaintext keys
// only ever live in local variables: tracing and video are off for this file
// (the reveal dialog shows the key on screen and in the DOM), and assertions
// never pass a key to `expect`, so a failure message cannot echo one.
import {
  expect,
  test,
  type APIResponse,
  type Browser,
  type BrowserContext,
  type Locator,
  type Page,
} from "@playwright/test";

import { reset } from "../support/reset";

const SERVER = "http://localhost:3000";
const OWNER_EMAIL = "owner@example.com";
const FIRST_PASSWORD = "owner-hunter22";
const SECOND_PASSWORD = "owner-hunter33";

test.use({ trace: "off", video: "off" });
test.describe.configure({ mode: "serial" });

let ctx: BrowserContext;
let page: Page;

async function newOwnerPage(browser: Browser): Promise<void> {
  // A context of our own, outlives single tests; it records nothing.
  ctx = await browser.newContext({ baseURL: "http://localhost:5173" });
  page = await ctx.newPage();
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill(OWNER_EMAIL);
  await page.getByTestId("setup-display-name").fill("Owner");
  await page.getByTestId("setup-password").fill(FIRST_PASSWORD);
  await page.getByTestId("setup-submit").click();
  await expect(page.getByTestId("sidebar")).toBeVisible({ timeout: 10_000 });
}

async function writeHeaders(): Promise<Record<string, string>> {
  const csrf = (await ctx.cookies()).find((c) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  return { "X-CSRF-Token": csrf.value };
}

function bearer(key: string): Record<string, string> {
  return { Authorization: `Bearer ${key}` };
}

async function errorCode(res: APIResponse): Promise<string | undefined> {
  const body = (await res.json().catch(() => null)) as { error?: { code?: string } } | null;
  return body?.error?.code;
}

/**
 * Fill and submit the create-key form inside `scope`, then read the
 * plaintext from the one-time reveal dialog and close it.
 */
async function createKeyInUi(
  scope: Locator,
  name: string,
  scopes: ReadonlyArray<"read" | "create" | "update" | "delete">,
  expiry?: string,
): Promise<string> {
  await scope.getByTestId("api-key-name").fill(name);
  for (const s of ["read", "create", "update", "delete"] as const) {
    await scope.getByTestId(`api-key-scope-${s}`).setChecked(scopes.includes(s));
  }
  if (expiry) await scope.getByTestId("api-key-expiry").selectOption(expiry);
  await scope.getByTestId("api-key-submit").click();

  const dialog = page.getByTestId("api-key-reveal");
  await expect(dialog).toBeVisible({ timeout: 5_000 });
  const key = await dialog.getByLabel("API key").inputValue();
  // Checked without handing the key to `expect`, so no failure output holds it.
  expect(key.startsWith("knot_"), "revealed key has the knot_ prefix").toBe(true);
  await dialog.getByRole("button", { name: "Done" }).click();
  await expect(dialog).toHaveCount(0);
  return key;
}

test.beforeAll(async ({ browser }) => {
  reset();
  await newOwnerPage(browser);
});

test.afterAll(async () => {
  await ctx?.close();
});

let readKey = "";

test("read-only key reads but cannot create", async ({ request }) => {
  await page.goto("/settings");
  const section = page.getByTestId("api-keys-section");
  await expect(section).toBeVisible();
  readKey = await createKeyInUi(section, "reader", ["read"]);
  await expect(section.getByTestId("api-key-row")).toHaveCount(1);

  // Standalone `request`: no browser cookies, the key alone authenticates.
  const list = await request.get(`${SERVER}/api/docs`, { headers: bearer(readKey) });
  expect(list.status()).toBe(200);

  const create = await request.post(`${SERVER}/api/docs`, {
    headers: bearer(readKey),
    data: { title: "Should not exist" },
  });
  expect(create.status()).toBe(403);
  expect(await errorCode(create)).toBe("api_key.scope_denied");
});

test("revoked key is rejected", async ({ request }) => {
  await page.goto("/settings");
  const row = page.getByTestId("api-keys-section").getByTestId("api-key-row").first();
  await row.getByTestId("api-key-revoke").click();
  await row.getByTestId("api-key-revoke-confirm").click();
  await expect(row).toContainText("Revoked", { timeout: 5_000 });

  const res = await request.get(`${SERVER}/api/docs`, { headers: bearer(readKey) });
  expect(res.status()).toBe(401);
  expect(await errorCode(res)).toBe("api_key.invalid");
});

test("changing the password revokes the user's keys (S5)", async ({ request }) => {
  await page.goto("/settings");
  const section = page.getByTestId("api-keys-section");
  const key = await createKeyInUi(section, "before-password-change", ["read"]);

  const before = await request.get(`${SERVER}/api/docs`, { headers: bearer(key) });
  expect(before.status()).toBe(200);

  await page.getByTestId("pw-current").fill(FIRST_PASSWORD);
  await page.getByTestId("pw-new").fill(SECOND_PASSWORD);
  await page.getByTestId("pw-submit").click();
  await expect(page.getByTestId("pw-ok")).toBeVisible({ timeout: 5_000 });

  const after = await request.get(`${SERVER}/api/docs`, { headers: bearer(key) });
  expect(after.status()).toBe(401);
  expect(await errorCode(after)).toBe("api_key.invalid");
});

test("bot key works but cannot reach owner-only actions (S4)", async ({ request }) => {
  // A doc to aim the owner-only calls at, created with the owner's session.
  const created = await page.request.post("/api/docs", {
    headers: await writeHeaders(),
    data: { title: "Bot target" },
  });
  expect(created.status()).toBe(201);
  const docId = ((await created.json()) as { id: string }).id;

  await page.goto("/members");
  await page.getByTestId("invite-email").fill("bot@example.com");
  await page.getByTestId("invite-kind-bot").check();
  await page.getByTestId("invite-role").selectOption("editor");
  await page.getByTestId("invite-submit").click();
  await expect(page.locator("[data-testid^='member-']").getByTestId("member-bot-badge")).toHaveCount(1, {
    timeout: 5_000,
  });

  await page.getByTestId("member-api-keys").click();
  const panel = page.locator("[data-testid^='member-api-keys-panel-']");
  await expect(panel).toBeVisible();
  // Every scope, so each refusal below is authorization, not the scope check.
  const botKey = await createKeyInUi(panel, "bot-key", ["read", "create", "update", "delete"], "never");
  await expect(panel.getByTestId("api-key-row")).toContainText("Never");

  const tasks = await request.get(`${SERVER}/api/workspace/tasks`, { headers: bearer(botKey) });
  expect(tasks.status()).toBe(200);

  const refusals: Array<[string, Promise<APIResponse>]> = [
    [
      "POST /api/workspace/members",
      request.post(`${SERVER}/api/workspace/members`, {
        headers: bearer(botKey),
        data: { email: "escalate@example.com", role: "owner", password: "hunter22hunter22" },
      }),
    ],
    [
      "PUT /api/docs/{id}/restricted",
      request.put(`${SERVER}/api/docs/${docId}/restricted`, {
        headers: bearer(botKey),
        data: { restricted: true },
      }),
    ],
    // Purge is a POST route (scope override: delete).
    ["POST /api/docs/{id}/purge", request.post(`${SERVER}/api/docs/${docId}/purge`, { headers: bearer(botKey) })],
  ];
  for (const [label, pending] of refusals) {
    const res = await pending;
    expect(res.status(), label).toBe(403);
    expect(await errorCode(res), label).not.toBe("api_key.scope_denied");
  }
});

test("TTL below 30 days is refused by the API", async () => {
  // page.request shares the browser's cookie jar: a session principal.
  const res = await page.request.post("/api/auth/api-keys", {
    headers: await writeHeaders(),
    data: { name: "too-short", scopes: ["read"], expires_in_days: 10 },
  });
  expect(res.status()).toBe(400);
  expect(await errorCode(res)).toBe("api_key.ttl_too_short");
});
