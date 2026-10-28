import { test, expect, type Page } from "@playwright/test";
import { dashboardOrigin, fixtureOrigin } from "./urls";
import { createHmac, randomBytes } from "node:crypto";

async function signIn(page: Page) {
  // Login is covered elsewhere; feature tests must not consume its rate budget.
  const payload = Buffer.from(JSON.stringify({ expires: Date.now() + 3_600_000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
  const signature = createHmac("sha256", "fixture-session-secret-at-least-32-characters").update("fixture-dashboard-password").update("\0").update(payload).digest("base64url");
  await page.context().addCookies([{ name: "enginebox_session", value: `${payload}.${signature}`, url: dashboardOrigin, httpOnly: true, sameSite: "Strict" }]);
  await page.goto("/?panel=github");
}
test("GitHub selects private repositories, links LayerFS sync and drains without granting runner permissions implicitly", async ({ page, request }) => {
  await request.post(`${fixtureOrigin}/__test/reset`);
  const repo = { id: 12, installation_id: 4, full_name: "fixture/private", private: true, branch: "main", default_branch: "main", permissions: { push: true } };
  const status = { configured: true, runner_app_configured: true, accounts: [{ kind: "repos", login: "fixture" }], repositories: [] as typeof repo[], pools: [{ id: "linux", organization: "fixture", labels: ["enginebox-linux"], runner_group_id: 8, max_runners: 1, preparation: { repository_id: 12, workflows: ["fixture/private/.github/workflows/ci.yml@refs/heads/main"] } }], jobs: [], draining: false };
  const writes: { path: string; body: Record<string, unknown> }[] = [];
  await page.route("**/api/control/v1/github**", async route => {
    const request = route.request(), path = new URL(request.url()).pathname;
    let value: unknown = status;
    if (path.endsWith("/repos/installations")) value = { installations: [{ id: 4, account: "fixture", suspended: false, permissions: { contents: "write" } }], install_url: "https://github.com/apps/fixture/installations/new" };
    else if (path.endsWith("/4/repositories")) value = { repositories: [repo] };
    else if (request.method() === "POST") {
      const body = request.postDataJSON(); writes.push({ path, body });
      if (path.endsWith("/select")) status.repositories = [repo];
      else if (path.endsWith("/drain")) status.draining = body.draining;
      else if (path.endsWith("/remove")) status.repositories = [];
    }
    await route.fulfill({ json: value });
  });
  await signIn(page);
  await expect(page.getByText("LayerFS preparation required")).toBeVisible();
  await expect(page.getByText("Connected as fixture")).toBeVisible();
  await page.getByRole("button", { name: "Choose repositories" }).click();
  await page.getByLabel("GitHub installation").selectOption("4");
  await page.getByRole("combobox", { name: "Repository", exact: true }).selectOption("12");
  await expect(page.getByLabel("Sync branch")).toHaveValue("main");
  await page.getByRole("button", { name: "Add repository" }).click();
  await expect(page.getByText("fixture/private", { exact: true })).toBeVisible();
  await expect(page.getByRole("link", { name: "Import or sync in Storage" })).toHaveAttribute("href", "/?panel=storage&github_repository=12");
  await page.getByLabel("Prepared runner profile").selectOption("linux");
  await page.getByLabel("Runner group name").fill("fixture-prepared");
  await page.getByLabel("Enginebox action at commit SHA").fill(`fixture/enginebox/packages/enginebox-actions@${"a".repeat(40)}`);
  await page.getByLabel("Checkout action at commit SHA").fill(`actions/checkout@${"b".repeat(40)}`);
  await page.getByRole("button", { name: "Preview prepared workflow" }).click();
  await expect(page.getByLabel("Prepared workflow fragment")).toHaveValue(/install-scripts: deny/);
  await expect(page.getByLabel("Prepared workflow fragment")).toHaveValue(/labels: "enginebox-linux"/);
  expect(writes.some(w => w.path.includes("workflow"))).toBe(false);
  await page.getByRole("button", { name: "Drain runners", exact: true }).click();
  await expect(page.getByRole("button", { name: "Resume runner provisioning" })).toBeVisible();
  expect(writes.some(w => w.path.includes("connect"))).toBe(false);
  expect(writes.find(w => w.path.endsWith("/select"))?.body.repository_id).toBe(12);
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.getByRole("link", { name: "Import or sync in Storage" }).click();
  await expect(page.getByLabel("GitHub repository", { exact: true })).toHaveValue("fixture/private");
  await expect(page.getByLabel("GitHub branch", { exact: true })).toHaveValue("main");
  await page.getByLabel("GitHub project ID", { exact: true }).fill("private-app");
  await page.getByRole("button", { name: "Fetch and review", exact: true }).click();
  await expect(page.getByLabel("GitHub tracked files")).toBeVisible();
  await expect(page.getByRole("button", { name: "Import into LayerFS" })).toBeDisabled();
  await page.getByRole("checkbox", { name: "I reviewed this source", exact: false }).check();
  await page.getByRole("button", { name: "Import into LayerFS" }).click();
  await expect(page.getByLabel("Connected project")).toHaveValue("private-app");
});
test("GitHub disabled state explains configuration without offering a broken connection", async ({ page }) => {
  await page.route("**/api/control/v1/github", route => route.fulfill({ json: { configured: false } }));
  await signIn(page);
  await expect(page.getByText("Configure your GitHub App", { exact: false })).toBeVisible();
  await expect(page.getByRole("button", { name: "Connect GitHub account" })).toHaveCount(0);
});

test("one App shares the connection and reviews permissions without another OAuth flow", async ({ page }) => {
  const status = { configured: true, shared_app: true, runner_app_configured: true, accounts: [{kind:"github",login:"fixture"}], repositories: [{id:12,installation_id:4,full_name:"fixture/private",private:true,branch:"main"}], pools: [], jobs: [], draining: false };
  const permissions: Record<string,string> = { contents:"write", actions:"read" };
  let disconnected = false;
  await page.route("**/api/control/v1/github**", async route => {
    const path = new URL(route.request().url()).pathname;
    let value: unknown = status;
    if (path.endsWith("/installations")) value = {installations:[{id:4,account:"fixture",permissions,suspended:false}],install_url:"https://github.com/apps/fixture-shared/installations/new"};
    if (path.endsWith("/repos/disconnect")) { disconnected=true; status.accounts=[]; status.repositories=[]; }
    await route.fulfill({json:value});
  });
  await signIn(page);
  await expect(page.getByRole("heading", {name:"GitHub connection",exact:true})).toBeVisible();
  await expect(page.getByRole("button", {name:"Reconnect GitHub",exact:true})).toBeVisible();
  await expect(page.getByRole("button", {name:"Connect runner administration"})).toHaveCount(0);
  await page.getByRole("button", {name:"Choose repositories"}).click();
  await expect(page.getByText("fixture: Repository sync approved", {exact:true})).toBeVisible();
  await page.getByRole("button", {name:"Review runner permissions"}).click();
  await expect(page.getByText("fixture: Organization approval required", {exact:true})).toBeVisible();
  await expect(page.getByRole("link", {name:"Approve permissions for the connected App on GitHub"})).toHaveAttribute("href","https://github.com/apps/fixture-shared/installations/new");
  permissions.organization_self_hosted_runners="write";
  await page.getByRole("button", {name:"Review runner permissions"}).click();
  await expect(page.getByText("fixture: Runner administration approved", {exact:true})).toBeVisible();
  await page.getByRole("button", {name:"Disconnect GitHub",exact:true}).click();
  await expect(page.getByRole("button", {name:"Review runner permissions"})).toBeDisabled();
  await expect(page.getByRole("button", {name:"Choose repositories"})).toBeDisabled();
  await expect(page.getByText("No repositories selected.", {exact:true})).toBeVisible();
  await expect(page.getByText("fixture: Runner administration approved", {exact:true})).toHaveCount(0);
  expect(disconnected).toBe(true);
});
