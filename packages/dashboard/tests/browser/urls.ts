import { tmpdir } from "node:os";
import { join } from "node:path";
export const fixturePort = process.env.ENGINEBOX_TEST_FLEET_PORT ?? process.env.EBOX_TEST_FLEET_PORT ?? "18765";
export const dashboardPort = process.env.ENGINEBOX_TEST_DASHBOARD_PORT ?? process.env.EBOX_TEST_DASHBOARD_PORT ?? "3007";
for (const port of [fixturePort, dashboardPort])
  if (!/^\d+$/.test(port) || Number(port) < 1024 || Number(port) > 65535)
    throw new Error("Invalid test port");
export const fixtureOrigin = `http://127.0.0.1:${fixturePort}`;
export const dashboardOrigin = `http://127.0.0.1:${dashboardPort}`;

/**
 * Server-side settings shared by the dashboard under test and the Playwright
 * workers, which mint real member sessions in the same identity state.
 */
export const identityDir = join(tmpdir(), `enginebox-dashboard-e2e-${dashboardPort}`);
export const ownerEmail = "owner@fixture.example";
export const dashboardEnv = {
  ENGINEBOX_DASHBOARD_ORIGIN: dashboardOrigin,
  ENGINEBOX_SESSION_SECRET: "fixture-session-secret-at-least-32-characters",
  ENGINEBOX_API_URL: fixtureOrigin,
  ENGINEBOX_API_TOKEN: "fixture-api-token-server-only-12345",
  ENGINEBOX_IDENTITY_STATE_DIR: identityDir,
  ENGINEBOX_OWNER_EMAIL: ownerEmail,
  ENGINEBOX_EMAIL_TRANSPORT: "file",
};
for (const [key, value] of Object.entries(dashboardEnv)) process.env[key] = value;
