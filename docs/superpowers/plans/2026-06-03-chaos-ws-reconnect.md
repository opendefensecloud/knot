# Chaos: WS Reconnect via toxiproxy (Plan 12.5)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:executing-plans (this is a 4-task micro-plan).

**Goal:** Close the last skipped e2e from Plan 12 — `ws-reconnect.spec.ts`. Playwright's `context.setOffline(true)` blocks new connections but doesn't close existing WebSockets, so the test couldn't observe `status=offline`. Switching to a toxiproxy sidecar that sits between the browser and knot-server lets us deterministically tear down the in-flight WS via its admin API.

**Architecture:**

```
Browser  ──HTTP/WS──▶  Vite dev (:5173)
                       └ vite proxy ──▶  toxiproxy(:3001) ──▶  knot-server(:3000)
                                         ▲
                                         │ admin API on :8474
                                         │
                                  Playwright test
```

- Add `toxiproxy` to the dev compose stack on its own port. It exposes an HTTP admin API on `:8474` and proxies the actual traffic on a configurable listen port (`:3001`).
- During boot, the test pre-registers a `knot` proxy: `listen 0.0.0.0:3001 → upstream localhost:3000`.
- Update `e2e/playwright.config.ts` so the spawned `knot-server` is on `:3000` as today, but the Vite dev proxy targets `:3001` (the toxiproxy listener). All other test traffic goes through unchanged.
- The reconnect spec hits toxiproxy's `:8474/proxies/knot/toxics` to add a `timeout` toxic (`timeout: 0` = close immediately), waits for `status=offline`, deletes the toxic, asserts `status=connected`.

**Predecessor:** Plan 14.5 (HEAD `e9b577a`). The skipped spec is `e2e/flows/ws-reconnect.spec.ts` from Plan 12.

---

## Tasks

| # | Title | LOC ≈ |
|---|---|---|
| 1 | Add toxiproxy to dev compose + register knot proxy | 50 |
| 2 | Add make + Vite plumbing to route through toxiproxy when enabled | 60 |
| 3 | Rewrite ws-reconnect.spec.ts using toxiproxy admin API | 120 |
| 4 | Outcome doc | 0 |

---

## Task 1: toxiproxy in compose

**Files:**
- Modify: `deploy/compose/dev.yml`
- Create: `deploy/compose/toxiproxy/init.json` (proxy definition)

### Step 1.1: Add the service

Append to `deploy/compose/dev.yml`:

```yaml
  toxiproxy:
    image: ghcr.io/shopify/toxiproxy:2.12.0
    container_name: knot-dev-toxiproxy
    command: ["-host", "0.0.0.0", "-port", "8474", "-config", "/etc/toxiproxy/init.json"]
    network_mode: host
    volumes:
      - ./toxiproxy/init.json:/etc/toxiproxy/init.json:ro
    healthcheck:
      test: ["CMD-SHELL", "wget -q --spider http://localhost:8474/version || exit 1"]
      interval: 2s
      timeout: 5s
      retries: 30
```

> `network_mode: host` is necessary because toxiproxy needs to reach `localhost:3000` (the knot-server spawned by Playwright on the host). On Linux this is fine; on macOS use `host.docker.internal` instead.

### Step 1.2: Proxy definition

`deploy/compose/toxiproxy/init.json`:

```json
[
  {
    "name": "knot",
    "listen": "0.0.0.0:3001",
    "upstream": "127.0.0.1:3000",
    "enabled": true
  }
]
```

This file is loaded at startup so the `knot` proxy exists immediately without a separate registration step.

### Step 1.3: Verify

```bash
make compose.up
curl -s http://localhost:8474/version           # → version string
curl -s http://localhost:8474/proxies | jq .     # → knot proxy listed
```

### Step 1.4: Commit

```bash
git add deploy/compose/
git commit -m "feat(compose): toxiproxy sidecar with knot proxy on :3001"
```

---

## Task 2: Playwright + Vite plumbing

**Files:**
- Modify: `e2e/playwright.config.ts`

### Approach

The reconnect spec is the only test that needs traffic through toxiproxy. We don't want the entire test suite to go through it — the extra hop adds latency and toxiproxy intentionally introduces failure modes.

Cleanest split: a separate `webServer` (Vite) instance for chaos tests is overkill. Instead, the reconnect spec hits the toxiproxy admin API directly. The `/collab/:doc_id` WS goes through Vite's normal proxy to `:3000`; we make a parallel proxy entry that routes through `:3001` only when toggled by an env var:

```ts
// web/vite.config.ts
const COLLAB_TARGET = process.env.VITE_COLLAB_VIA_PROXY === "1"
  ? "ws://localhost:3001"
  : "ws://localhost:3000";
```

This keeps the non-chaos tests fast. The chaos test sets `VITE_COLLAB_VIA_PROXY=1` in `playwright.config.ts`'s webServer env for the chaos spec only.

Actually simpler — just always route through toxiproxy in the e2e environment. The proxy is a passthrough until a toxic is added. The fixed cost is ~1 ms per request, which doesn't materially affect the suite. Setting once in `playwright.config.ts`:

```ts
// in the existing /web webServer env:
env: {
  ...
  VITE_COLLAB_VIA_PROXY: "1",
},
```

### Step 2.1: Edit `web/vite.config.ts`

Find the existing proxy block:

```ts
proxy: {
  "/collab": { target: "ws://localhost:3000", ws: true },
  ...
}
```

Replace the `/collab` target with the env-driven one:

```ts
const collabTarget = process.env.VITE_COLLAB_VIA_PROXY === "1"
  ? "ws://localhost:3001"
  : "ws://localhost:3000";

// ...
proxy: {
  "/collab": { target: collabTarget, ws: true },
  // others unchanged
}
```

### Step 2.2: Edit `e2e/playwright.config.ts`

In the existing Vite `webServer` block, add to the env:

```ts
env: {
  ...
  VITE_COLLAB_VIA_PROXY: "1",
},
```

### Step 2.3: Verify

```bash
cd e2e
pnpm playwright test
```

Should still be 21 passed / 1 skipped (the existing ws-reconnect spec is still `test.skip`; we'll un-skip it in Task 3). No regression — the toxiproxy passthrough should be invisible.

### Step 2.4: Commit

```bash
git add e2e/playwright.config.ts web/vite.config.ts
git commit -m "test(e2e): route /collab through toxiproxy when VITE_COLLAB_VIA_PROXY=1"
```

---

## Task 3: Rewrite ws-reconnect spec

**Files:**
- Rewrite: `e2e/flows/ws-reconnect.spec.ts`

### Step 3.1: Spec

```ts
import { execSync } from "node:child_process";
import { expect, test } from "@playwright/test";

function reset() {
  const tables = [
    "acl_invalidations","audit_events","doc_markdown_cache","doc_snapshots","doc_updates",
    "document_grants","documents","sessions","workspace_members","users","workspaces",
    "blobs","blob_bytes",
  ].join(", ");
  execSync(
    `docker compose -f deploy/compose/dev.yml exec -T postgres psql -U knot -d knot -c "TRUNCATE TABLE ${tables} CASCADE"`,
    { cwd: "..", stdio: "pipe" },
  );
}

const TOXIPROXY = "http://localhost:8474";

async function setToxic(toxic: object): Promise<void> {
  const r = await fetch(`${TOXIPROXY}/proxies/knot/toxics`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(toxic),
  });
  if (!r.ok) throw new Error(`toxiproxy add: ${r.status} ${await r.text()}`);
}

async function clearToxics(): Promise<void> {
  const list = await fetch(`${TOXIPROXY}/proxies/knot/toxics`).then((r) => r.json()) as { name: string }[];
  for (const t of list) {
    const r = await fetch(`${TOXIPROXY}/proxies/knot/toxics/${t.name}`, { method: "DELETE" });
    if (!r.ok) throw new Error(`toxiproxy del ${t.name}: ${r.status}`);
  }
}

test.beforeAll(async () => {
  reset();
  await clearToxics();
});
test.afterEach(async () => { await clearToxics(); });

test("editor reconnects after a forced WS flap; content preserved", async ({ page }) => {
  await page.goto("/setup");
  await page.getByTestId("setup-email").fill("o@e.com");
  await page.getByTestId("setup-display-name").fill("O");
  await page.getByTestId("setup-password").fill("owner-hunter22");
  await page.getByTestId("setup-submit").click();
  await page.getByTestId("new-doc").click();
  await page.waitForURL(/\/doc\/.+/);
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  const editor = page.locator("[data-testid='editor-host'] .ProseMirror");
  await editor.click();
  await page.keyboard.type("Before the flap.");
  await page.waitForTimeout(300);

  // Force-close the upstream side of the WS via toxiproxy.
  await setToxic({
    name: "reset_peer",
    type: "reset_peer",
    stream: "upstream",
    attributes: { timeout: 0 },
  });
  // KnotProvider's onclose fires within a few ms.
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "offline", { timeout: 5_000 });

  // Restore network. KnotProvider.scheduleReconnect kicks in.
  await clearToxics();
  await expect(page.getByTestId("status-dot")).toHaveAttribute("data-status", "connected", { timeout: 10_000 });

  // Y.Doc never lost state.
  await expect(editor).toContainText("Before the flap.");
});
```

> The `reset_peer` toxic sends a TCP RST to the upstream — exactly what we want to test "server-side connection dropped". `timeout: 0` means apply immediately. Once the toxic is deleted, new TCP connections succeed and the provider's exponential backoff retry reconnects.

### Step 3.2: Run

```bash
cd e2e
pnpm playwright test ws-reconnect.spec.ts
```

If it flaps:
- Bump the offline-detection timeout to 8 s (KnotProvider keepalive might take a tick).
- Ensure `network_mode: host` actually works on the test host (Linux yes, macOS use `host.docker.internal` and a fixed published port instead).
- Check toxiproxy is healthy: `docker compose ps toxiproxy`.

Run the full suite to confirm no regression:

```bash
pnpm playwright test
```

Should be **22 passed, 0 skipped** (ws-reconnect now passes).

### Step 3.3: Commit

```bash
git add e2e/
git commit -m "test(e2e): WS reconnect via toxiproxy reset_peer toxic"
```

---

## Task 4: Outcome doc

Brief outcome. Add Plan 12.5 row to `docs/superpowers/README.md`.

```bash
git add docs/
git commit -m "docs: Plan 12.5 outcome — toxiproxy WS reconnect e2e"
```
