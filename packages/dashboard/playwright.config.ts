import { defineConfig } from "@playwright/test";

const dashboardPort = process.env.ENGINEBOX_E2E_DASHBOARD_PORT ?? "3007";
const fixturePort = process.env.ENGINEBOX_E2E_FIXTURE_PORT ?? "18765";
const dashboardOrigin = `http://127.0.0.1:${dashboardPort}`;
const fixtureOrigin = `http://127.0.0.1:${fixturePort}`;
export default defineConfig({
  testDir: "./tests/browser",
  workers: 1,
  timeout: 45_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: dashboardOrigin,
    viewport: { width: 1440, height: 1000 },
    trace: "retain-on-failure",
  },
  webServer: [
    {
      command: "node tests/fixtures/fleet-server.mjs",
      url: `${fixtureOrigin}/healthz`,
      reuseExistingServer: false,
    },
    {
      command: "pnpm start",
      url: `${dashboardOrigin}/healthz`,
      reuseExistingServer: false,
      env: {
        HOST: "127.0.0.1",
        PORT: dashboardPort,
        ENGINEBOX_DASHBOARD_ORIGIN: dashboardOrigin,
        ENGINEBOX_DASHBOARD_PASSWORD: "fixture-dashboard-password",
        ENGINEBOX_SESSION_SECRET:
          "fixture-session-secret-at-least-32-characters",
        ENGINEBOX_API_URL: fixtureOrigin,
        ENGINEBOX_API_TOKEN: "fixture-api-token-server-only-12345",
      },
    },
  ],
});
