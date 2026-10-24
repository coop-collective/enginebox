export const fixturePort = process.env.ENGINEBOX_TEST_FLEET_PORT ?? process.env.EBOX_TEST_FLEET_PORT ?? "18765";
export const dashboardPort = process.env.ENGINEBOX_TEST_DASHBOARD_PORT ?? process.env.EBOX_TEST_DASHBOARD_PORT ?? "3007";
for (const port of [fixturePort, dashboardPort])
  if (!/^\d+$/.test(port) || Number(port) < 1024 || Number(port) > 65535)
    throw new Error("Invalid test port");
export const fixtureOrigin = `http://127.0.0.1:${fixturePort}`;
export const dashboardOrigin = `http://127.0.0.1:${dashboardPort}`;
