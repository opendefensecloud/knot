import { type Browser, type BrowserContext, type Page, expect, test } from "@playwright/test";

import { reset } from "../support/reset";

test.beforeAll(reset);
// Each test builds on the pages and the link the previous one made.
test.describe.configure({ mode: "serial" });

const ORIGIN = "http://localhost:5173";
const SHORT_LINK = /^http:\/\/localhost:5173\/[2-9a-kmnp-zA-HJ-NP-Z]{7}$/;

let pageA = "";
let pageB = "";
let shortLink = "";

async function newClipboardContext(browser: Browser): Promise<BrowserContext> {
  const ctx = await browser.newContext();
  await ctx.grantPermissions(["clipboard-read", "clipboard-write"], { origin: ORIGIN });
  return ctx;
}

async function writeHeaders(page: Page) {
  const csrf = (await page.context().cookies()).find((c) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  return { "X-CSRF-Token": csrf.value };
}

async function signInOwner(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill("owner@example.com");
  await page.getByTestId("login-password").fill("owner-hunter22");
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

/** Dex SSO from the knot login page; alice is auto-provisioned as a viewer. */
async function ssoAsAlice(page: Page) {
  await page.waitForURL(/\/login/);
  await page.click("text=Continue with SSO");
  await page.waitForURL(/:5556\/dex/);
  await page.locator('input[name="login"]').fill("alice@example.com");
  await page.locator('input[name="password"]').fill("password");
  await page.locator('button[type="submit"]').click();
  const grant = page.locator("text=Grant Access");
  if (await grant.isVisible({ timeout: 1000 }).catch(() => false)) {
    await grant.click();
  }
}

async function copyLink(page: Page): Promise<string> {
  await page.getByTestId("copy-link").click();
  await expect(page.getByTestId("toast-info")).toContainText("Copied!");
  return page.evaluate(() => navigator.clipboard.readText());
}

test("a reader copies a short link and gets the same one twice", async ({ browser }) => {
  // The owner sets up the workspace and two pages.
  const ownerCtx = await browser.newContext();
  const owner = await ownerCtx.newPage();
  await owner.goto("/setup");
  await owner.getByTestId("setup-email").fill("owner@example.com");
  await owner.getByTestId("setup-display-name").fill("Owner");
  await owner.getByTestId("setup-password").fill("owner-hunter22");
  await owner.getByTestId("setup-submit").click();
  await owner.waitForURL(/\/(?:doc\/.+)?$/);
  for (const title of ["Linked page", "Never linked"]) {
    const res = await owner.request.post("/api/docs", {
      headers: await writeHeaders(owner),
      data: { title },
    });
    expect(res.status()).toBe(201);
    const id = ((await res.json()) as { id: string }).id;
    if (!pageA) pageA = id;
    else pageB = id;
  }
  await ownerCtx.close();

  // Alice, a viewer, opens the page and copies its link twice.
  const ctx = await newClipboardContext(browser);
  const alice = await ctx.newPage();
  await alice.goto(`/doc/${pageA}`);
  await ssoAsAlice(alice);
  await alice.waitForURL(`${ORIGIN}/doc/${pageA}`, { timeout: 15_000 });
  const first = await copyLink(alice);
  expect(first).toMatch(SHORT_LINK);
  expect(await copyLink(alice)).toBe(first);
  shortLink = first;
  await ctx.close();
});

test("a signed-out colleague opens a short link and lands on the page after SSO", async ({ browser }) => {
  const ctx = await browser.newContext();
  const colleague = await ctx.newPage();
  await colleague.goto(new URL(shortLink).pathname);
  await ssoAsAlice(colleague);
  await colleague.waitForURL(`${ORIGIN}/doc/${pageA}`, { timeout: 15_000 });
  await expect(colleague.getByTestId("doc-title")).toHaveValue("Linked page");
  await ctx.close();
});

test("an archived page copies its existing link, and one without a code copies the long link", async ({ browser }) => {
  const ctx = await newClipboardContext(browser);
  const owner = await ctx.newPage();
  await signInOwner(owner);
  for (const id of [pageA, pageB]) {
    const res = await owner.request.delete(`/api/docs/${id}`, { headers: await writeHeaders(owner) });
    expect(res.ok()).toBe(true);
  }

  await owner.goto(`/doc/${pageA}`);
  await expect(owner.getByTestId("archived-banner")).toBeVisible();
  expect(await copyLink(owner)).toBe(shortLink);

  await owner.goto(`/doc/${pageB}`);
  await expect(owner.getByTestId("archived-banner")).toBeVisible();
  expect(await copyLink(owner)).toBe(`${ORIGIN}/doc/${pageB}`);
  // Copying on the archived page created no code for it.
  const res = await owner.request.get(`/api/docs/${pageB}/short-link`);
  expect(res.status()).toBe(404);
  await ctx.close();
});
