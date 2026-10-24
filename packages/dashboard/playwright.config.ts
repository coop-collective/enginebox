import { defineConfig } from "@playwright/test";

const fleetPort = process.env.EBOX_TEST_FLEET_PORT ?? "18765";
const dashboardPort = process.env.EBOX_TEST_DASHBOARD_PORT ?? "3007";
for (const port of [fleetPort, dashboardPort])
  if (!/^\d+$/.test(port) || Number(port) < 1024 || Number(port) > 65535)
    throw new Error("Invalid test port");
const fleetOrigin = `http://127.0.0.1:${fleetPort}`;
const dashboardOrigin = `http://127.0.0.1:${dashboardPort}`;
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
      url: `${fleetOrigin}/healthz`,
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
        ENGINEBOX_API_URL: fleetOrigin,
        ENGINEBOX_API_TOKEN: "fixture-api-token-server-only-12345",
      },
    },
  ],
});
