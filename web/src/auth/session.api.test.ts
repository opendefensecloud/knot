import { afterEach, describe, expect, it, vi } from "vitest";

import { authApi } from "./session.api";

const BASE = {
  user_id: "u1",
  email: "a@example.org",
  display_name: "A",
  workspace_id: "w1",
  role: "editor",
};

function serverSends(body: Record<string, unknown>) {
  vi.spyOn(globalThis, "fetch").mockResolvedValue(
    new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } }),
  );
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("authApi.session", () => {
  // Catches the two fields being made required: older servers send neither.
  it("defaults auth_kind and the key lifetime cap when they are absent", async () => {
    serverSends(BASE);
    await expect(authApi.session()).resolves.toMatchObject({
      ok: { auth_kind: "local", max_api_key_ttl_days: null },
    });
  });

  it("keeps the values the server sends", async () => {
    serverSends({ ...BASE, auth_kind: "oidc", max_api_key_ttl_days: 90 });
    await expect(authApi.session()).resolves.toMatchObject({
      ok: { auth_kind: "oidc", max_api_key_ttl_days: 90 },
    });
  });

  it("keeps an explicit null cap, which means unlimited", async () => {
    serverSends({ ...BASE, auth_kind: "local", max_api_key_ttl_days: null });
    await expect(authApi.session()).resolves.toMatchObject({
      ok: { max_api_key_ttl_days: null },
    });
  });

  // A session that fails to parse sends a signed-in user to /login with no
  // message, and logging in returns them there. Catches these two fields being
  // made strict: neither is worth locking anyone out over.
  it("keeps the session when auth_kind is unknown", async () => {
    serverSends({ ...BASE, auth_kind: "saml" });
    await expect(authApi.session()).resolves.toMatchObject({
      ok: { user_id: "u1", auth_kind: "local" },
    });
  });

  it("keeps the session when the key lifetime cap isn't a number", async () => {
    serverSends({ ...BASE, max_api_key_ttl_days: "90" });
    await expect(authApi.session()).resolves.toMatchObject({
      ok: { user_id: "u1", max_api_key_ttl_days: null },
    });
  });
});
