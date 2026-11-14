import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";

// Node 25+ ships a global localStorage that shadows jsdom's; turn it off where Node knows the flag.
const NO_WEBSTORAGE = "--no-experimental-webstorage";
const execArgv = process.allowedNodeEnvironmentFlags.has(NO_WEBSTORAGE) ? [NO_WEBSTORAGE] : [];

export default defineConfig({
  plugins: [react()],
  test: {
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    globals: false,
    execArgv,
  },
  resolve: {
    alias: { "@": "/src" },
  },
});
