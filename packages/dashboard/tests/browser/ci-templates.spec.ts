import { fixtureOrigin, dashboardOrigin } from "./urls";
import { test, expect } from "@playwright/test";
import { createHmac, randomBytes } from "node:crypto";

test.beforeEach(async ({ request }) => { await request.post(`${fixtureOrigin}/__test/reset`); });

test("CI templates enable, show failed verification, retry to readiness and disable", async ({ page, request }) => {
  expect((await request.get("/api/control/v1/ci/templates")).status()).toBe(401);
  // Reuse the signed-fixture session pattern from storage tests; login behavior
  // has its own coverage and this feature must not exhaust its attempt budget.
  const payload = Buffer.from(JSON.stringify({ expires: Date.now() + 3_600_000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
  const signature = createHmac("sha256", "fixture-session-secret-at-least-32-characters").update("fixture-dashboard-password").update("\0").update(payload).digest("base64url");
  await page.context().addCookies([{ name: "enginebox_session", value: `${payload}.${signature}`, url: dashboardOrigin, httpOnly: true, sameSite: "Strict" }]);
  await page.goto("/");
  await page.getByRole("button", { name: "Templates", exact: true }).click();
  const panel = page.getByRole("region", { name: "CI toolchain templates" });
  await expect(panel.getByText("No CI templates registered.")).toBeVisible();
  await panel.getByLabel("CI recipe JSON").fill("invalid");
  await panel.getByRole("button", { name: "Register CI template" }).click();
  await expect(panel.getByRole("alert")).toHaveText("The CI template must be valid JSON.");
  await panel.getByLabel("CI recipe JSON").fill(JSON.stringify({
    schema_version: 1, id: "node-linux", revision: 1, label: "Node Linux", os: "linux", architecture: "x64",
    image: "node-24", capabilities: ["node"], minimum: { cpu: 1, memory_mib: 256, disk_mib: 512 }, shell: "bash",
    install: "mkdir bin", verify: [{ capability: "node", run: "node --version", expected_stdout: "v24.22.1" }], paths: ["bin"], timeout_minutes: 10,
  }));
  await panel.getByRole("button", { name: "Register CI template" }).click();
  const card = panel.getByRole("article", { name: "Node Linux CI template" });
  await expect(card.getByText("Disabled", { exact: true })).toBeVisible();
  await card.getByRole("button", { name: "Enable CI template" }).click();
  await expect(card.getByText("verification failed")).toBeVisible();
  await expect(card.getByText("0 / 1 nodes ready")).toBeVisible();
  await card.getByRole("button", { name: "Retry installation" }).click();
  await expect(card.getByText("1 / 1 nodes ready")).toBeVisible();
  await expect(card.getByText(/Capabilities verified/)).toBeVisible();
  await card.getByRole("button", { name: "Disable CI template" }).click();
  await expect(card.getByText("Disabled", { exact: true })).toBeVisible();
  await expect(card.getByText("0 / 1 nodes ready")).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(panel.getByLabel("CI recipe JSON")).toBeVisible();
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth)).toBe(true);
});
