import { test, expect, type Page } from "@playwright/test";
import { createHmac, randomBytes } from "node:crypto";
import { dashboardOrigin } from "./urls";

async function open(page: Page, path: string) {
  const payload = Buffer.from(JSON.stringify({ expires: Date.now() + 3600000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
  const signature = createHmac("sha256", "fixture-session-secret-at-least-32-characters").update("fixture-dashboard-password").update("\0").update(payload).digest("base64url");
  await page.context().addCookies([{ name: "enginebox_session", value: `${payload}.${signature}`, url: dashboardOrigin, httpOnly: true, sameSite: "Strict" }]);
  await page.goto(path);
}

const SECRET = "typed-secret-value-never-shown";

function catalogFixture(page: Page) {
  const connections = [
    { name: "kitbash", kind: "mcp_http", url: "https://kitbash.example.com/mcp", headers: { Authorization: { secret: "KITBASH_TOKEN", prefix: "Bearer " } }, secret_names: ["KITBASH_TOKEN"], used_by: ["coder"], revision: 2, created_by: "owner", updated_at: "2026-10-29T00:00:00Z", last_used_at: "2026-10-29T01:00:00Z" },
    { name: "staging-secrets", kind: "secret_source", source: { provider: "kitbash", integration: "canvalytics-staging", project: "canvalytics", environment: "staging" }, names: ["DATABASE_URL"], secret_names: [], used_by: [], revision: 1, created_by: "owner", updated_at: "2026-10-29T00:00:00Z" },
  ];
  const profile = { name: "coder", description: "Builds one change per run.", template: "native-dev", org_id: "default", providers: { claude: { permission_profile: "edit", max_turns: 300, skills: [{ name: "hq-loop", files: [] }], env_keys: ["GIT_AUTHOR_NAME"] } }, connections: ["kitbash", "staging-secrets"], connection_policy: { planner: [] as string[] }, revision: 3, updated_at: "2026-10-29T00:00:00Z" };
  const posts: { url: string; body: unknown }[] = [];
  return {
    posts,
    profile,
    install: () =>
      page.route("**/api/catalog/**", async (route) => {
        const url = new URL(route.request().url()).pathname;
        if (route.request().method() === "POST") {
          const body = route.request().postDataJSON();
          posts.push({ url, body });
          if (url.endsWith("/test")) return route.fulfill({ json: { test: { ok: true, reachable: true, http_status: 200, auth: "accepted" } } });
          if (url.endsWith("/connections/policy")) profile.connection_policy = body.connection_policy;
          if (url === "/api/catalog/connections") connections.push({ ...body, secret_names: Object.keys(body.secrets ?? {}), used_by: [], revision: 1, created_by: "owner", updated_at: "2026-10-29T02:00:00Z", secrets: undefined });
          return route.fulfill({ json: { ok: true } });
        }
        if (url === "/api/catalog/connections") return route.fulfill({ json: { connections, can_manage: true, role: "owner" } });
        if (url === "/api/catalog/profiles") return route.fulfill({ json: { profiles: [profile], can_manage: true, role: "owner" } });
        if (url === "/api/catalog/profiles/coder/runs") return route.fulfill({ json: { runs: [{ id: "0123456789abcdef", status: "succeeded", provider: "claude", created_at: "2026-10-29T01:00:00Z", profile: { run_kind: "debug", connections: [{ name: "kitbash", revision: 2 }] } }] } });
        return route.fulfill({ status: 404, json: { error: "Unknown" } });
      }),
  };
}

test("Connections lists, tests and creates connections with write-only secret inputs", async ({ page }) => {
  const fixture = catalogFixture(page);
  await fixture.install();
  await open(page, "/?panel=connections");
  const nav = page.getByRole("navigation", { name: "Control plane" });
  await expect(nav.getByRole("button", { name: "Connections", exact: true })).toHaveAttribute("aria-current", "page");
  const panel = page.getByRole("region", { name: "Connections" });
  await expect(panel.getByRole("cell", { name: "https://kitbash.example.com/mcp" })).toBeVisible();
  await expect(panel.getByText("Authorization: Bearer secret KITBASH_TOKEN")).toBeVisible();
  await expect(panel.getByText("Kitbash canvalytics-staging · canvalytics/staging")).toBeVisible();
  await panel.getByRole("button", { name: "Test" }).first().click();
  await expect(panel.getByRole("status")).toContainText("Working · HTTP 200 · auth accepted");
  await expect(panel.getByRole("button", { name: "Delete" }).first()).toBeDisabled();

  await panel.getByLabel("Connection name").fill("docs");
  await panel.getByLabel("MCP URL").fill("https://docs.example.com/mcp");
  await panel.getByLabel("Header").fill("Authorization");
  await panel.getByLabel("Name", { exact: true }).fill("DOCS_TOKEN");
  await panel.getByLabel("Prefix").fill("Bearer ");
  const value = panel.getByLabel("Secret value");
  await expect(value).toHaveAttribute("type", "password");
  await value.fill(SECRET);
  await panel.getByRole("button", { name: "Add connection" }).click();
  await expect(panel.getByRole("cell", { name: "https://docs.example.com/mcp" })).toBeVisible();
  const created = fixture.posts.find((p) => p.url === "/api/catalog/connections")!.body as Record<string, unknown>;
  expect(created).toMatchObject({ name: "docs", kind: "mcp_http", headers: { Authorization: { secret: "DOCS_TOKEN", prefix: "Bearer " } }, secrets: { DOCS_TOKEN: SECRET } });
  await expect(panel.getByLabel("Secret value")).toHaveValue("");
  expect(await page.content()).not.toContain(SECRET);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});

test("Profiles shows the profile, its connections, per-run-kind policy and recent runs", async ({ page }) => {
  const fixture = catalogFixture(page);
  await fixture.install();
  await open(page, "/?panel=profiles");
  const panel = page.getByRole("region", { name: "Profiles" });
  await expect(panel.getByText("Template: native-dev")).toBeVisible();
  await expect(panel.getByRole("cell", { name: "skill hq-loop" })).toBeVisible();
  await expect(panel.getByRole("cell", { name: "kitbash r2" })).toBeVisible();
  await expect(panel.getByRole("cell", { name: "debug" })).toBeVisible();
  await panel.getByLabel("New run kind").fill("builder");
  await panel.getByRole("button", { name: "Add kind" }).click();
  await panel.getByRole("row", { name: /builder/ }).getByLabel("staging-secrets").check();
  await panel.getByRole("button", { name: "Save policy" }).click();
  await expect.poll(() => fixture.posts.find((p) => p.url.endsWith("/connections/policy"))?.body).toEqual({ connection_policy: { planner: [], builder: ["staging-secrets"] } });
  await panel.getByRole("button", { name: "Detach" }).first().click();
  await expect.poll(() => fixture.posts.find((p) => p.url.endsWith("/connections/detach"))?.body).toEqual({ connection: "kitbash" });
});
