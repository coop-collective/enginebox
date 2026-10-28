import { test, expect, type Page } from "@playwright/test";
import { createHmac, randomBytes } from "node:crypto";
import { dashboardOrigin, fixtureOrigin } from "./urls";
import type { Workspace } from "../../lib/workspaces";
async function signIn(page: Page, path = "/?panel=workspaces") {
  const payload = Buffer.from(JSON.stringify({ expires: Date.now() + 3600000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
  const signature = createHmac("sha256", "fixture-session-secret-at-least-32-characters").update("fixture-dashboard-password").update("\0").update(payload).digest("base64url");
  await page.context().addCookies([{ name: "enginebox_session", value: `${payload}.${signature}`, url: dashboardOrigin, httpOnly: true, sameSite: "Strict" }]);
  await page.goto(path);
}
function workspace(id: string): Workspace {
  return { id, storage_usage: [], repositories: [], pools: [], runs: [], storage_nodes: [], checkouts: [], source_observations: [], integration_observations: [], operations: [], launch_events: [] };
}
test("one workspace source list creates hosted sources, links approved GitHub and launches native runs", async ({ page }) => {
  const app = workspace("app");
  app.pools = [{ name: "app-pool", member_status: [{ host_id: "offline", name: "ebox-test", busy: true }], running_runs: 1 }];
  app.storage_usage = [{ host_id: "linux", observed_at_unix_ms: 1790812800000, status: "complete", shared_caches: [{ kind: "pnpm-store", status: "complete", apparent_bytes: 1048576, allocated_bytes: 524288, files: 4 }], projects: [] }];
  app.checkouts = [{ name: "agent", host_id: "linux", status: "mounted", observed_at: "2026-10-01T00:00:00Z", last_checkpoint: { kind: "Committed", head: "checkpoint-abc" }, command_timings: [{ kind: "test", status: "succeeded", elapsed_ms: 1200, observed_at_unix_ms: 1790812800000 }] }];
  app.operations = [{ id: "import", host_id: "linux", name: "app", operation: "github-import", status: "succeeded", created_at: "2026-10-01T00:00:00Z", duration_seconds: 1.2, bytes: 2048 }];
  app.integration_observations = [{ host_id: "linux", observed_at: "2026-10-01T00:00:00Z", main: { revision: "accepted-abc" }, sync_status: "delivered" }];
  const workspaces: Workspace[] = [app];
  const writes: { path: string; body: Record<string, unknown> }[] = [];
  await page.route("**/api/control/v1/workspaces", route => route.fulfill({ json: { workspaces, coverage: { run_history_limit: 256, operation_history_limit: 200, operations_per_workspace_limit: 30 } } }));
  await page.route("**/api/control/v1/repository-backends", route => route.fulfill({ json: { backend: { configured: true } } }));
  await page.route("**/api/control/v1/github", route => route.fulfill({ json: { repositories: [{ id: 7, full_name: "fixture/private", branch: "main", private: true }] } }));
  await page.route("**/api/environments", route => route.fulfill({ json: { environments: [{ id: "dev-app", source: { kind: "layerfs", project: "app" }, status: "running", healthy: true, metrics: { preparation: { revision: "r1", observed_at: "2026-10-01T00:00:00Z", status: "succeeded", elapsed_ms: 180, source_copy_ms: 40, source_bytes: 2048, prepare_ms: 120 }, preparation_reuse: { revision: "r1", observed_at: "2026-10-01T00:01:00Z", status: "reused", elapsed_ms: 2 } } }] } }));
  await page.route("**/api/control/v1/projects/*/repositories**", async route => {
    const path = new URL(route.request().url()).pathname, body = route.request().postDataJSON();
    writes.push({ path, body });
    const project = path.split("/")[5];
    let w = workspaces.find(w => w.id === project);
    if (!w) { w = workspace(project); workspaces.push(w); }
    const repo = { project, name: body.name, backend: path.endsWith("github") ? "github" : "artifacts_worker", remote: path.endsWith("github") ? `https://github.com/${body.full_name}.git` : "https://hosted.example/new.git", default_branch: "main", created_at: "2026-10-01T00:00:00Z" };
    w.repositories.push(repo); await route.fulfill({ json: { repository: repo }, status: 201 });
  });
  await page.route("**/api/control/v1/pools/app-pool/agent-runs", async route => {
    writes.push({ path: new URL(route.request().url()).pathname, body: route.request().postDataJSON() });
    app.runs = [{ id: "run-1", provider: "codex", pool: "app-pool", status: "queued" }];
    await route.fulfill({ json: { run: { id: "run-1" } }, status: 202 });
  });
  await signIn(page);
  await expect(page.getByRole("navigation", { name: "Control plane" }).getByRole("button", { name: "GitHub", exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "Map fixture/private to workspace" }).click();
  await expect(page.getByLabel("Source type")).toHaveValue("github");
  await expect(page.getByLabel("Selected GitHub repository")).toHaveValue("fixture/private");
  await page.getByRole("button", { name: "← All workspaces", exact: true }).click();
  await page.getByRole("button", { name: "Open workspace app" }).click();
  await expect(page.getByText(/1 runtime states unknown/)).toBeVisible();
  await expect(page.getByText(/Source copy 40 ms/)).toBeVisible();
  await expect(page.getByText(/Shared node cache pnpm-store/)).toContainText("Logical file size 1.00 MiB");
  await expect(page.getByText(/Accepted main accepted-abc/)).toBeVisible();
  await expect(page.getByText(/test command · succeeded · 1.20 s/)).toBeVisible();
  await expect(page.getByText(/Checkpoint checkpoint-abc/)).toBeVisible();
  await page.getByText("Add source", { exact: true }).click();
  await page.getByLabel("Source type").selectOption("github");
  await page.getByLabel("Selected GitHub repository").selectOption("fixture/private");
  await page.getByRole("button", { name: "Link GitHub source" }).click();
  await expect(page.getByRole("link", { name: "Review and prepare source" })).toHaveAttribute("href", "/?panel=workspaces&storage=1&workspace=app&github_repository=7");
  await page.getByLabel("Agent pool").selectOption("app-pool");
  await page.getByLabel("Repository access").selectOption("app/main");
  await page.getByLabel("Task", { exact: true }).fill("Inspect the code and propose a change");
  await page.getByRole("button", { name: "Submit agent run" }).click();
  await expect(page.getByText("codex · queued", { exact: true })).toBeVisible();
  expect(writes.at(-1)?.body).toEqual({ provider: "codex", prompt: "Inspect the code and propose a change", repository: "app/main" });
  await page.getByRole("button", { name: "New workspace", exact: true }).click();
  await page.getByLabel("Workspace ID").fill("fresh");
  await page.getByLabel("Source type").selectOption("hosted");
  await page.getByRole("button", { name: "Create hosted source" }).click();
  await expect(page.getByRole("heading", { name: "fresh", exact: true })).toBeVisible();
  await expect(page.getByText(/Source connected · preparation needed/)).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
});
test("partial unavailable observations remain unknown and old repository URLs open Workspaces", async ({ page }) => {
  const app = workspace("app");
  await page.route("**/api/control/v1/workspaces", route => route.fulfill({ json: { workspaces: [app], coverage: {} } }));
  await page.route("**/api/control/v1/repository-backends", route => route.fulfill({ json: { backend: { configured: false } } }));
  await page.route("**/api/control/v1/github", route => route.fulfill({ json: { repositories: [] } }));
  await page.route("**/api/environments", route => route.fulfill({ json: { error: "Disconnected" }, status: 502 }));
  await signIn(page, "/?panel=repositories&workspace=app");
  await expect(page.getByRole("heading", { name: "app", exact: true })).toBeVisible();
  await expect(page.getByText(/Development state unavailable/)).toBeVisible();
  await expect(page.getByText(/No project-attributed storage operations measured yet/)).toBeVisible();
  await expect(page.getByText(/No agent pool is assigned/)).toBeVisible();
});

test("workspace storage controls cannot operate on another project's checkouts", async ({ page, request }) => {
  await request.post(`${fixtureOrigin}/__test/reset`);
  await page.route("**/api/control/v1/jobs/storage-*", async route => {
    const response = await route.fetch();
    const value = await response.json();
    if (Array.isArray(value.job?.result?.result?.workspaces)) {
      value.job.result.result.workspaces.push({ name: "demo-private", project: "demo", status: "ready" });
      value.job.result.result.github_projects.push({ id: "other", repository: "private/other", branch: "main", current: { project: "native-other", commit: "a".repeat(40) } });
    }
    await route.fulfill({ response, json: value });
  });
  await signIn(page, "/?panel=workspaces&workspace=game&storage=1");
  await expect(page.getByRole("heading", { name: "Source and storage · game" })).toBeVisible();
  const control = page.getByRole("region", { name: "Source and storage · game" });
  await expect(control.getByRole("combobox", { name: "Project", exact: true })).toBeDisabled();
  await expect(control.getByRole("combobox", { name: "Project", exact: true })).toHaveValue("game");
  await expect(control.getByRole("combobox", { name: "Project", exact: true }).locator("option")).toHaveCount(1);
  await control.getByText("Import a GitHub repository", { exact: true }).click();
  await expect(control.getByLabel("GitHub project ID")).toHaveValue("game");
  await expect(control.getByLabel("GitHub project ID")).toHaveAttribute("readonly", "");
  await expect(control.getByRole("region", { name: "Checkout worker-a", exact: true })).toBeVisible();
  await expect(control.getByRole("region", { name: "Checkout demo-private", exact: true })).toHaveCount(0);
  await expect(control.getByText("private/other", { exact: false })).toHaveCount(0);
  await expect(page.getByRole("navigation", { name: "Control plane" }).getByRole("button", { name: "Storage", exact: true })).toHaveCount(0);
});
