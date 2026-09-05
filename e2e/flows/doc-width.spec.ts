import { expect, test, type Page } from "@playwright/test";

import { reset } from "../support/reset";

// Reset once: the owner is created in the beforeAll below, and a per-test
// reset would delete it out from under the later tests.
test.beforeAll(reset);

const EMAIL = "owner@width.test";
const PASSWORD = "hunter22!width";

// /setup renders its form whether or not an owner exists, so probing the
// page to decide between setup and login doesn't work. Create the owner
// once against the API instead, and have every test sign in normally.
test.beforeAll(async () => {
  const r = await fetch("http://localhost:3000/auth/setup", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ email: EMAIL, password: PASSWORD, display_name: "Owner" }),
  });
  if (!r.ok) throw new Error(`setup failed: ${r.status} ${await r.text()}`);
});

/** Each test gets a fresh browser context, so every one needs a session. */
async function signIn(page: Page) {
  await page.goto("/login");
  await page.getByTestId("login-email").fill(EMAIL);
  await page.getByTestId("login-password").fill(PASSWORD);
  await page.getByTestId("login-submit").click();
  await page.waitForURL(/\/(?:doc\/.+)?$/);
}

async function newDoc(page: Page) {
  await page.getByTestId("new-doc").click();
  await page.waitForSelector("[data-testid='new-doc-modal']", { state: "visible", timeout: 5_000 });
  await page.getByTestId("new-doc-blank").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", {
    timeout: 10_000,
  });
  return page.url();
}

/** Width of the doc shell's border box. Polled by callers, because the
 *  shell animates its max-width over 160ms — measuring at t=0 reads the
 *  width it is leaving, not the one it is going to. */
async function shellWidth(page: Page) {
  const box = await page.getByTestId("doc-page").boundingBox();
  return box!.width;
}

test("toggling width mode widens the shell and survives a reload", async ({ page }) => {
  await signIn(page);
  await newDoc(page);

  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "fixed");
  expect(await shellWidth(page)).toBeCloseTo(760, 0);

  await page.getByTestId("toggle-doc-width").click();
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "wide");
  // 1280 viewport - 260 sidebar; the 1600px cap doesn't bind at this size.
  await expect.poll(() => shellWidth(page)).toBeGreaterThan(1000);

  await page.reload();
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "wide");
  await expect.poll(() => shellWidth(page)).toBeGreaterThan(1000);
});

test("width mode is global — it carries to a newly opened doc", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  await page.getByTestId("toggle-doc-width").click();
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "wide");

  await newDoc(page);
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "wide");
  await expect.poll(() => shellWidth(page)).toBeGreaterThan(1000);
});

test("the prose measure is identical in both modes", async ({ page }) => {
  // The contract test for the whole design: wide mode widens the container,
  // never the measure. If this fails, toggling reflows the user's text.
  await signIn(page);
  await newDoc(page);

  const prose = page.locator("[data-testid='editor-host'] .ProseMirror");
  await prose.click();
  await page.keyboard.type("A paragraph whose line breaks must not move when the layout changes.");

  const para = prose.locator("p").first();
  const fixed = (await para.boundingBox())!.width;

  await page.getByTestId("toggle-doc-width").click();
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "wide");
  await expect.poll(() => shellWidth(page)).toBeGreaterThan(1000); // transition settled
  const wide = (await para.boundingBox())!.width;

  expect(Math.abs(wide - fixed)).toBeLessThanOrEqual(1);
});

test("a table in wide mode does not give the app a horizontal scrollbar", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  await page.getByTestId("toggle-doc-width").click();

  const prose = page.locator("[data-testid='editor-host'] .ProseMirror");
  await prose.click();
  await page.getByTestId("toolbar-table").click();
  await expect(prose.locator("table")).toBeVisible();

  // <main> is the scroll port: its overflow-y:auto makes overflow-x compute
  // to auto, so it absorbs horizontal overflow as its own scrollbar rather
  // than the document's. Asserting on documentElement is a false negative.
  const overflow = await page.evaluate(() => {
    const m = document.querySelector("main")!;
    return m.scrollWidth - m.clientWidth;
  });
  expect(overflow).toBeLessThanOrEqual(1);
});

test("the comment sidebar insets the document instead of covering it", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  await page.getByTestId("toggle-doc-width").click();

  await page.getByTestId("open-comments").click();
  await expect(page.getByTestId("comment-sidebar")).toBeVisible();

  const railLeft = (await page.getByTestId("comment-sidebar").boundingBox())!.x;
  await expect
    .poll(async () => {
      const prose = (await page.locator("[data-testid='editor-host'] .ProseMirror").boundingBox())!;
      return prose.x + prose.width;
    })
    .toBeLessThanOrEqual(railLeft);
});

test("the width toggle is absent on mobile, where it would do nothing", async ({ page }) => {
  await signIn(page);
  await newDoc(page);
  await page.setViewportSize({ width: 375, height: 667 });
  await expect(page.getByTestId("toggle-doc-width")).toHaveCount(0);

  // The layout also falls back below the 1024px media query, independently
  // of the JS gate — so a stale "wide" preference can't leak onto a phone.
  await page.evaluate(() => localStorage.setItem("knot.docWidth", "wide"));
  await page.reload();
  const shell = (await page.getByTestId("doc-page").boundingBox())!;
  expect(shell.width).toBeLessThanOrEqual(375);
});

test("code blocks and diagram cards break out of the prose measure in wide mode", async ({ page }) => {
  // The breakout rules have to reach through Tiptap's own `div.react-renderer`,
  // which is the real direct child of .ProseMirror for any React node view.
  // Selectors written against the card itself match nothing, and the failure is
  // invisible: `.ProseMirror > *` still caps the wrapper at the prose measure,
  // so the card silently stays narrow instead of erroring.
  await signIn(page);
  await newDoc(page);
  await page.setViewportSize({ width: 1920, height: 1000 });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  // A paragraph to compare against, then a code block below it.
  await page.keyboard.type("prose holds the measure");
  await page.keyboard.press("Enter");
  await page.getByTestId("toolbar-code-block").click();
  await page.keyboard.type("const x = 1;");

  // Wide mode: the shell is far wider than the 712px prose measure.
  await page.getByTestId("toggle-doc-width").click();
  await expect(page.locator("html")).toHaveAttribute("data-doc-width", "wide");
  await expect.poll(() => shellWidth(page)).toBeGreaterThan(1000);

  // Measure the rendered widths rather than asserting on a selector: the
  // point is that the rule applies, and Playwright's selector engine does not
  // support every relative form the stylesheet uses.
  const widths = await editor.evaluate((el) => {
    const code = el.querySelector(":scope > .react-renderer.node-code_block");
    const para = el.querySelector(":scope > p");
    return {
      code: code ? code.getBoundingClientRect().width : null,
      paragraph: para ? para.getBoundingClientRect().width : null,
    };
  });

  expect(widths.paragraph, "no paragraph to compare against").not.toBeNull();
  expect(widths.code, "the code block's wrapper is not a direct child of .ProseMirror").not.toBeNull();
  // Prose holds the 712px measure; the code block gets --knot-code-measure.
  expect(widths.paragraph!).toBeCloseTo(712, -1);
  expect(widths.code!).toBeGreaterThan(widths.paragraph! + 100);
});
