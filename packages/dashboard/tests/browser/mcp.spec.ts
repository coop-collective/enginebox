import { test, expect, type Page } from "@playwright/test";
import { createHmac, randomBytes } from "node:crypto";
import { dashboardOrigin } from "./urls";
import { mcpConfiguration } from "../../lib/mcp-setup";
async function openMcp(page: Page, path = "/?panel=mcp") {
  const payload = Buffer.from(JSON.stringify({ expires: Date.now() + 3600000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
  const signature = createHmac("sha256", "fixture-session-secret-at-least-32-characters").update("fixture-dashboard-password").update("\0").update(payload).digest("base64url");
  await page.context().addCookies([{ name: "enginebox_session", value: `${payload}.${signature}`, url: dashboardOrigin, httpOnly: true, sameSite: "Strict" }]);
  await page.goto(path);
}
test("MCP teaches stdio and scoped OAuth separately and retains grant revocation", async ({ page }) => {
  const grants = [{ id: "grant-1", client_id: "client-1", client_name: "Fixture app", redirect_hosts: ["fixture.example"], workspaces: [], pools: [{ pool: "app-workers", project: "app", profile: "coder" }], repositories: [{ project: "app", access: "write" }], project: "app", created_at: "2026-10-01T00:00:00Z", refreshed_at: "2026-10-01T00:00:00Z", access_token_active: true }];
  let revoked = false;
  await page.route("**/api/oauth/grants**", async route => {
    if (route.request().method() === "POST") { revoked = true; grants.splice(0); }
    await route.fulfill({ json: { enabled: true, grants } });
  });
  await openMcp(page);
  const nav = page.getByRole("navigation", { name: "Control plane" });
  await expect(nav.getByRole("button", { name: "MCP", exact: true })).toHaveAttribute("aria-current", "page");
  await expect(nav.getByRole("button", { name: "Connected apps", exact: true })).toHaveCount(0);
  await expect(page.getByLabel("MCP stdio configuration")).toHaveValue(mcpConfiguration);
  await expect(page.getByText(/there is currently no remote/)).toBeVisible();
  await expect(page.getByText("pool_agent_launch", { exact: true })).toBeVisible();
  await page.context().grantPermissions(["clipboard-read", "clipboard-write"]);
  await page.getByRole("button", { name: "Copy MCP configuration" }).click();
  await expect(page.getByRole("button", { name: "Configuration copied" })).toBeVisible();
  expect(await page.evaluate(() => navigator.clipboard.readText())).toBe(mcpConfiguration);
  await expect(page.getByRole("article", { name: "Connection Fixture app" })).toContainText("app-workers");
  page.once("dialog", dialog => dialog.accept());
  await page.getByRole("button", { name: "Disconnect", exact: true }).click();
  await expect(page.getByText("No apps are connected.")).toBeVisible();
  expect(revoked).toBe(true);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
test("legacy apps URL opens MCP and disabled OAuth does not hide local setup", async ({ page }) => {
  await page.route("**/api/oauth/grants", route => route.fulfill({ json: { enabled: false, grants: [] } }));
  await openMcp(page, "/?panel=apps");
  await expect(page.getByRole("heading", { name: "Connect a local MCP client" })).toBeVisible();
  await expect(page.getByText(/OAuth app connections are not enabled/)).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Control plane" }).getByRole("button", { name: "MCP", exact: true })).toHaveAttribute("aria-current", "page");
});
