import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests/browser",
  workers: 1,
  timeout: 45_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: "http://127.0.0.1:3007",
    viewport: { width: 1440, height: 1000 },
    trace: "retain-on-failure",
  },
  webServer: [
    {
      command: "node tests/fixtures/fleet-server.mjs",
      url: "http://127.0.0.1:18765/healthz",
      reuseExistingServer: false,
    },
    {
      command: "pnpm start",
      url: "http://127.0.0.1:3007/healthz",
      reuseExistingServer: false,
      env: {
        HOST: "127.0.0.1",
        PORT: "3007",
        ENGINEBOX_DASHBOARD_ORIGIN: "http://127.0.0.1:3007",
        ENGINEBOX_DASHBOARD_PASSWORD: "fixture-dashboard-password",
        ENGINEBOX_SESSION_SECRET:
          "fixture-session-secret-at-least-32-characters",
        ENGINEBOX_API_URL: "http://127.0.0.1:18765",
        ENGINEBOX_API_TOKEN: "fixture-api-token-server-only-12345",
      },
    },
  ],
});
