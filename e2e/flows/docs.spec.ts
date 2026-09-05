import { execSync } from "node:child_process";

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

// Helper: read the csrf cookie from the playwright context's StorageState
// (cookies persist within `request.newContext`'s session)
async function csrfTokenFor(ctx: any): Promise<string> {
  const cookies = await ctx.storageState();
  const csrf = cookies.cookies.find((c: any) => c.name === "csrf");
  if (!csrf) throw new Error("csrf cookie not found");
  return csrf.value;
}

test("docs CRUD + grant flow", async () => {
  const ctx = await adminCtx();
  const csrf = await csrfTokenFor(ctx);
  const writeHeaders = { "X-CSRF-Token": csrf };

  // List empty.
  const empty = await ctx.get("/api/docs");
  expect(empty.status()).toBe(200);
  expect((await empty.json()).length).toBe(0);

  // Create root.
  const created = await ctx.post("/api/docs", {
    headers: writeHeaders,
    data: { title: "Root" },
  });
  expect(created.status()).toBe(201);
  const root = await created.json();
  expect(root.title).toBe("Root");

  // Create child.
  const childCreated = await ctx.post("/api/docs", {
    headers: writeHeaders,
    data: { title: "Child", parent_id: root.id },
  });
  expect(childCreated.status()).toBe(201);
  const child = await childCreated.json();

  // Get child with effective_role.
  const got = await ctx.get(`/api/docs/${child.id}`);
  expect(got.status()).toBe(200);
  const body = await got.json();
  expect(body.title).toBe("Child");
  expect(body.effective_role).toBe("owner");

  // PATCH title.
  const renamed = await ctx.patch(`/api/docs/${child.id}`, {
    headers: writeHeaders,
    data: { title: "Renamed" },
  });
  expect(renamed.status()).toBe(200);
  expect((await renamed.json()).title).toBe("Renamed");

  // Move to top-level.
  const moved = await ctx.post(`/api/docs/${child.id}/move`, {
    headers: writeHeaders,
    data: { parent_id: null },
  });
  expect(moved.status()).toBe(200);
  expect((await moved.json()).parent_id).toBeNull();

  // Soft-delete and restore.
  const del = await ctx.delete(`/api/docs/${child.id}`, { headers: writeHeaders });
  expect(del.status()).toBe(204);
  const listAfterDel = await ctx.get("/api/docs");
  expect(
    (await listAfterDel.json()).find((d: any) => d.id === child.id),
  ).toBeUndefined();

  const restored = await ctx.post(`/api/docs/${child.id}/restore`, {
    headers: writeHeaders,
  });
  expect(restored.status()).toBe(204);

  // Pre-provision a second user via direct SQL (v0.1 has no email-invite UX).
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c ` +
      `"INSERT INTO users (email, display_name) VALUES ('bob@example.com', 'Bob')"`,
    { cwd: "..", stdio: "pipe" },
  );

  // Invite bob as viewer.
  const invite = await ctx.post("/api/workspace/members", {
    headers: writeHeaders,
    data: { email: "bob@example.com", role: "viewer" },
  });
  expect(invite.status()).toBe(201);
  const membersResp = await ctx.get("/api/workspace/members");
  const members = await membersResp.json();
  expect(members.length).toBe(2);
  const bob = members.find((m: any) => m.email === "bob@example.com");
  expect(bob).toBeTruthy();

  // Grant bob editor on root doc.
  const principal = `user:${bob.user_id}`;
  const put = await ctx.put(
    `/api/docs/${root.id}/grants/${encodeURIComponent(principal)}`,
    {
      headers: writeHeaders,
      data: { role: "editor", inherit: true },
    },
  );
  expect(put.status()).toBe(204);

  const grantsList = await ctx.get(`/api/docs/${root.id}/grants`);
  expect(grantsList.status()).toBe(200);
  const arr = await grantsList.json();
  expect(arr.length).toBe(1);
  expect(arr[0].role).toBe("editor");

  // Delete the grant.
  const delGrant = await ctx.delete(
    `/api/docs/${root.id}/grants/${encodeURIComponent(principal)}`,
    { headers: writeHeaders },
  );
  expect(delGrant.status()).toBe(204);
  const empty2 = await ctx.get(`/api/docs/${root.id}/grants`);
  expect((await empty2.json()).length).toBe(0);
});
