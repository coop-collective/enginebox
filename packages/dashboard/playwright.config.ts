import { fixtureOrigin, dashboardOrigin, dashboardPort } from "./tests/browser/urls";
import { defineConfig } from "@playwright/test";

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
        ENGINEBOX_NODE_PUBLIC_ORIGIN: "https://controller.fixture.example",
        ENGINEBOX_NODE_INSTALLER_URL: "https://github.com/coop-collective/enginebox/releases/download/fixture-1/install.ps1",
        ENGINEBOX_NODE_INSTALLER_SHA256: "f".repeat(64),
        ENGINEBOX_LINUX_NODE_INSTALLER_URL: "https://github.com/coop-collective/enginebox/releases/download/linux-node-preview-fixture/install-linux-node.sh",
        ENGINEBOX_LINUX_NODE_INSTALLER_SHA256: "d".repeat(64),
        PORT: dashboardPort,
        ENGINEBOX_DASHBOARD_ORIGIN: dashboardOrigin,
        ENGINEBOX_DASHBOARD_PASSWORD: "fixture-dashboard-password",
        ENGINEBOX_SESSION_SECRET:
          "fixture-session-secret-at-least-32-characters",
        ENGINEBOX_API_URL: fixtureOrigin,
        ENGINEBOX_API_TOKEN: "fixture-api-token-server-only-12345",
        ENGINEBOX_ENVIRONMENTS_URL: fixtureOrigin,
        ENGINEBOX_ENVIRONMENTS_TOKEN: "fixture-api-token-server-only-12345",
      },
    },
  ],
});
