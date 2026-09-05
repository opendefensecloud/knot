import { test, expect, request } from "@playwright/test";

import { reset } from "../support/reset";
const SERVER = "http://localhost:3000";

test.beforeAll(reset);

async function adminCtx() {
  const ctx = await request.newContext({ baseURL: SERVER });
  const setup = await ctx.post("/auth/setup", {
    data: {
      email: "owner@example.com",
      password: "owner-hunter22",
      display_name: "Owner",
    },
  });
  expect(setup.status()).toBe(201);
  return ctx;
}

async function csrfTokenFor(ctx: any): Promise<string> {
  const cookies = await ctx.storageState();
  const csrf = cookies.cookies.find((c: any) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  return csrf.value;
}

test("markdown import + export round trip via room actor", async () => {
  const ctx = await adminCtx();
  const csrf = await csrfTokenFor(ctx);
  const writeHeaders = { "X-CSRF-Token": csrf };

  // Create a doc.
  const created = await ctx.post("/api/docs", {
    headers: writeHeaders,
    data: { title: "MD" },
  });
  expect(created.status()).toBe(201);
  const doc = await created.json();

  // Import some markdown.
  const md = "# Hello\n\nworld.\n";
  const imp = await ctx.post(`/api/docs/${doc.id}/markdown`, {
    headers: { ...writeHeaders, "Content-Type": "text/markdown" },
    data: md,
  });
  expect(imp.status()).toBe(204);

  // Export — must round-trip the heading + paragraph.
  const exp = await ctx.get(`/api/docs/${doc.id}/markdown`);
  expect(exp.status()).toBe(200);
  const text = await exp.text();
  expect(text).toContain("# Hello");
  expect(text).toContain("world.");
});
