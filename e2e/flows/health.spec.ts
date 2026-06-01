import { test, expect, request } from "@playwright/test";

test("health endpoints respond", async () => {
  const ctx = await request.newContext({ baseURL: "http://localhost:3000" });

  const healthz = await ctx.get("/api/healthz");
  expect(healthz.status()).toBe(200);
  expect(await healthz.text()).toBe("ok");

  const readyz = await ctx.get("/api/readyz");
  expect(readyz.status()).toBe(200);

  const version = await ctx.get("/api/version");
  expect(version.status()).toBe(200);
  const body = await version.json();
  expect(body.version).toBeTruthy();
  expect(body.commit).toBeTruthy();
});
