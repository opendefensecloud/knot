import { test, expect, request } from "@playwright/test";

// The SPA fallback serves index.html for any unmatched path, so "200 OK" alone
// proves nothing about a missing asset — every assertion below pins the
// content-type to make sure we got the image and not the HTML shell.

test("the browser tab identifies the app as knot", async ({ page }) => {
  await page.goto("/");
  await expect(page).toHaveTitle("knot");
});

test("the branded favicon assets are served as images", async () => {
  const ctx = await request.newContext();

  const svg = await ctx.get("/favicon.svg");
  expect(svg.status()).toBe(200);
  expect(svg.headers()["content-type"]).toContain("image/svg+xml");

  // Safari's SVG-favicon support is inconsistent; the 32px PNG is its fallback.
  const png = await ctx.get("/favicon-32.png");
  expect(png.status()).toBe(200);
  expect(png.headers()["content-type"]).toContain("image/png");

  const apple = await ctx.get("/apple-touch-icon.png");
  expect(apple.status()).toBe(200);
  expect(apple.headers()["content-type"]).toContain("image/png");

  await ctx.dispose();
});

test("index.html points browsers at those assets", async () => {
  const ctx = await request.newContext();
  const html = await (await ctx.get("/")).text();

  expect(html).toContain('href="/favicon.svg"');
  expect(html).toContain('href="/apple-touch-icon.png"');
  expect(html).toContain('href="/site.webmanifest"');

  await ctx.dispose();
});

test("the web manifest describes an installable knot app", async () => {
  const ctx = await request.newContext();

  const res = await ctx.get("/site.webmanifest");
  expect(res.status()).toBe(200);
  const manifest = JSON.parse(await res.text());

  expect(manifest.name).toBe("knot");
  expect(manifest.theme_color).toBe("#2563EB");

  // An installable icon set needs both the 192 launcher size and the 512
  // splash size, plus a maskable variant so Android does not crop the mark.
  const sizes = manifest.icons.map((i: { sizes: string }) => i.sizes);
  expect(sizes).toContain("192x192");
  expect(sizes).toContain("512x512");
  expect(
    manifest.icons.some((i: { purpose?: string }) => i.purpose?.includes("maskable")),
  ).toBe(true);

  for (const icon of manifest.icons) {
    const asset = await ctx.get(icon.src);
    expect(asset.status(), `${icon.src} should be served`).toBe(200);
    expect(asset.headers()["content-type"]).toContain("image/png");
  }

  await ctx.dispose();
});
