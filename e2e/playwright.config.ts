import { defineConfig, devices } from "@playwright/test";

const chromiumPath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE_PATH;

export default defineConfig({
  testDir: "./flows",
  fullyParallel: false,
  // All specs share a single Postgres backend; each spec's beforeAll truncates
  // the auth/docs tables. Running spec files in parallel causes truncate races
  // and unique-constraint violations on the default workspace slug.
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 0,
  reporter: [["list"]],
  use: {
    baseURL: "http://localhost:5173",
    trace: "on-first-retry",
    video: "retain-on-failure",
    launchOptions: chromiumPath ? { executablePath: chromiumPath } : {},
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        // E2E specs were written assuming the editor is immediately editable.
        // Production now defaults docs to view mode (Plan view-mode toggle);
        // setting this localStorage flag flips that default for tests so we
        // don't have to thread a `toggle-edit-mode` click through every spec.
        storageState: {
          cookies: [],
          origins: [
            {
              origin: "http://localhost:5173",
              localStorage: [{ name: "knot.editMode.defaultOn", value: "1" }],
            },
          ],
        },
      },
    },
  ],
  // The Rust server needs a long timeout because `cargo run` may compile
  // on first invocation. After the first run, the binary is cached.
  webServer: [
    {
      command: process.env.KNOT_TEST_BIN ?? "cargo run --bin knot-server",
      cwd: "..",
      port: 3000,
      reuseExistingServer: !process.env.CI,
      timeout: 180_000,
      stdout: "pipe",
      stderr: "pipe",
      // The auth endpoints require a real Postgres backend; the default
      // in-memory mode has no auth routes. Spreading process.env keeps
      // PATH and cargo's env intact so `cargo run` can find toolchains.
      env: {
        ...(process.env as Record<string, string>),
        KNOT_DATABASE_URL: "postgres://knot:knot@localhost:5432/knot",
        KNOT_SESSION_KEY: "test-key-32-bytes-aaaaaaaaaaaaaa",
        KNOT_OIDC_ENABLED: "true",
        KNOT_OIDC_ISSUER: "http://localhost:5556/dex",
        KNOT_OIDC_CLIENT_ID: "knot",
        KNOT_OIDC_CLIENT_SECRET: "knot-dev-secret",
        // Route the OIDC callback through Vite so the final redirect lands
        // on the frontend origin (5173). Dex has both URIs registered.
        KNOT_OIDC_REDIRECT_URL: "http://localhost:5173/auth/oidc/callback",
        // After the callback the server redirects to base_url. Point it at
        // the Vite dev-server so Playwright ends up on the right origin.
        KNOT_BASE_URL: "http://localhost:5173",
        // Auto-provision the OIDC user into the (existing or newly-created)
        // workspace so the round-trip lands a session.
        KNOT_OIDC_AUTO_PROVISION: "always",
        // Snapshot after every update. The history e2e no longer needs it
        // (its timeline comes from the edit log, not from snapshots); it
        // stays because every spec has run with it, and dropping it changes
        // when pages snapshot under all of them.
        KNOT_SNAPSHOT_EVERY_N: "1",
      },
    },
    {
      command: "pnpm dev",
      cwd: "../web",
      port: 5173,
      reuseExistingServer: !process.env.CI,
      timeout: 60_000,
      env: {
        ...(process.env as Record<string, string>),
        // Route /collab through the toxiproxy sidecar so chaos tests
        // (ws-reconnect.spec.ts) can force-flap the WS via the toxiproxy
        // admin API. Passthrough when no toxics are configured.
        VITE_COLLAB_VIA_PROXY: "1",
      },
    },
  ],
});
