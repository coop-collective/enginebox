import { test, expect, type Page } from "@playwright/test";
import { readFile } from "node:fs/promises";

const origin = "http://127.0.0.1:3007";
async function signIn(page: Page) {
  await page.goto("/");
  await page.getByLabel("Operator password").fill("fixture-dashboard-password");
  await page.getByRole("button", { name: "Open dashboard" }).click();
  await expect(
    page.getByRole("heading", { name: "Fleet overview." }),
  ).toBeVisible();
}
test.beforeEach(async ({ request }) => {
  await request.post("http://127.0.0.1:18765/__test/reset");
});

test("login protects fleet data, rejects wrong passwords and keeps API token server-side", async ({
  page,
  request,
  context,
}) => {
  const response = await request.get("/api/control/v1/fleet");
  expect(response.status()).toBe(401);
  await page.goto("/");
  await page.getByLabel("Operator password").fill("wrong-password");
  await page.getByRole("button", { name: "Open dashboard" }).click();
  await expect(page.getByRole("alert")).toContainText("not accepted");
  const browserRequests: string[] = [];
  page.on("request", (req) =>
    browserRequests.push(
      `${req.url()} ${JSON.stringify(req.headers())} ${req.postData() ?? ""}`,
    ),
  );
  await signIn(page);
  await expect(
    page.getByText("Build machine", { exact: true }).first(),
  ).toBeVisible();
  expect(browserRequests.join("\n")).not.toContain(
    "fixture-api-token-server-only-12345",
  );
  const cookie = (await context.cookies()).find(
    (cookie) => cookie.name === "enginebox_session",
  );
  expect(cookie?.httpOnly).toBe(true);
  expect(cookie?.sameSite).toBe("Strict");
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page.getByLabel("Operator password")).toBeVisible();
});

test("live inventory, template creation and all lifecycle actions follow asynchronous jobs", async ({
  page,
}) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await signIn(page);
  await expect(page.getByText("SSH connection timed out.")).toBeVisible();
  const base = page.getByRole("row").filter({ hasText: "ebox-native-base" });
  await expect(base.getByText("Read only")).toBeVisible();
  await expect(base.getByRole("button")).toHaveCount(0);
  await page.getByRole("button", { name: "New workspace" }).click();
  await page.getByLabel("Workspace name").fill("ebox-new-project");
  await page
    .getByRole("combobox", { name: "Size", exact: true })
    .selectOption("small");
  await page
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect(
    page.getByText("Create ebox-new-project", { exact: true }),
  ).toBeVisible();
  const created = page.getByRole("row").filter({ hasText: "ebox-new-project" });
  await expect(created.getByText("Running", { exact: true })).toBeVisible();
  await expect(
    page.getByText("Cold start · ready in 12.40 s", { exact: false }),
  ).toBeVisible();
  const row = page.getByRole("row").filter({ hasText: "ebox-editor" });
  for (const [action, expected] of [
    ["Pause", "Paused"],
    ["Resume", "Running"],
    ["Hibernate", "Hibernated"],
    ["Resume", "Running"],
    ["Stop", "Stopped"],
  ]) {
    if (action === "Stop") page.once("dialog", (dialog) => dialog.accept());
    await row.getByRole("button", { name: action, exact: true }).click();
    await expect(row.getByText(expected, { exact: true })).toBeVisible();
  }
  expect(errors).toEqual([]);
  await page.screenshot({
    path: "test-results/fleet-desktop.png",
    fullPage: true,
  });
});

test("failed jobs, empty fleet and API errors are visible", async ({
  page,
  request,
}) => {
  await signIn(page);
  await page.getByRole("button", { name: "New workspace" }).click();
  await page.getByLabel("Workspace name").fill("ebox-fail");
  await page
    .getByRole("button", { name: "Create workspace", exact: true })
    .click();
  await expect(
    page.getByText("The selected host does not have enough free storage."),
  ).toBeVisible();
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "empty" },
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Add your first host" }),
  ).toBeVisible();
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "error" },
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Fixture host inventory is unavailable",
  );
});

test("pending lifecycle operations only change controls on their target workspace", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "slow-actions" },
  });
  await signIn(page);
  const editor = page.getByRole("row").filter({ hasText: "ebox-editor" });
  const review = page.getByRole("row").filter({ hasText: "ebox-review" });
  const submitted: string[] = [];
  page.on("request", (req) => {
    if (req.method() === "POST" && req.url().endsWith("/actions"))
      submitted.push(new URL(req.url()).pathname);
  });
  for (const [action, status] of [
    ["Hibernate", "Hibernated"],
    ["Resume", "Running"],
  ]) {
    await editor.getByRole("button", { name: action, exact: true }).click();
    await expect(editor.getByText("Working", { exact: true })).toBeVisible();
    expect(
      await review
        .getByRole("button", { name: "Resume", exact: true })
        .isVisible(),
    ).toBe(true);
    await expect(
      review.getByRole("button", { name: "Resume", exact: true }),
    ).toBeEnabled();
    await expect(review.getByText("Paused", { exact: true })).toBeVisible();
    await expect(review.getByText("Working", { exact: true })).toHaveCount(0);
    await expect(editor.getByText(status, { exact: true })).toBeVisible();
  }
  expect(submitted).toEqual([
    "/api/control/v1/hosts/metal-01/instances/ebox-editor/actions",
    "/api/control/v1/hosts/metal-01/instances/ebox-editor/actions",
  ]);
});

test("top resource cards distinguish CPU allocation from actual host memory usage", async ({
  page,
}) => {
  await signIn(page);
  const cpu = page.locator(".stat-card").filter({ hasText: "CPU allocated" });
  await expect(cpu.locator("strong")).toHaveText("12 / 32");
  await expect(cpu.locator("small")).toHaveText(
    "37.5% allocated · logical cores",
  );
  const memory = page
    .locator(".stat-card")
    .filter({ hasText: "Host memory used" });
  await expect(memory.locator("strong")).toHaveText("24 GiB / 128 GiB");
  await expect(memory.locator("small")).toHaveText(
    "18.8% used · across online hosts",
  );
});

test("authenticated mutations reject missing or different Origin", async ({
  page,
  context,
}) => {
  await signIn(page);
  const attempts: Record<string, string>[] = [
    { Origin: "https://attacker.example" },
    {},
  ];
  for (const headers of attempts) {
    for (const endpoint of ["instances", "allocations"]) {
      const response = await context.request.post(
        `${origin}/api/control/v1/hosts/metal-01/${endpoint}`,
        {
          headers,
          data: { name: "ebox-nope", template: "native", size: "small" },
        },
      );
      expect(response.status()).toBe(403);
    }
  }
});

test("warm reserve is separate inventory and custom resources can claim it with no cold capacity", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "warm" },
  });
  await signIn(page);
  const assigned = page
    .locator(".stat-card")
    .filter({ hasText: "Assigned workspaces" });
  await expect(assigned.locator("strong")).toHaveText("2");
  await expect(assigned).toContainText("1 ready in reserve");
  await expect(
    page.getByText("1 ready · 1 preparing · target 2"),
  ).toBeVisible();
  for (const state of ["ready", "preparing", "quarantined"]) {
    const row = page.getByRole("row").filter({ hasText: `ebox-pool-${state}` });
    await expect(row.getByText("Reserved for allocation")).toBeVisible();
    await expect(row.getByRole("button")).toHaveCount(0);
  }
  await page.getByRole("button", { name: "New workspace" }).click();
  await page.getByLabel("Workspace name").fill("ebox-my-project");
  await page
    .getByRole("combobox", { name: "Size", exact: true })
    .selectOption("custom");
  await expect(
    page.getByText(
      "No cold-start capacity remains; another request may claim the reserve first.",
    ),
  ).toBeVisible();
  const button = page.getByRole("button", {
    name: "Create workspace",
    exact: true,
  });
  await expect(button).toBeEnabled();
  const submitted = page.waitForRequest(
    (req) => req.method() === "POST" && req.url().endsWith("/allocations"),
  );
  await button.click();
  expect((await submitted).postDataJSON()).toEqual({
    name: "ebox-my-project",
    template: "native",
    resources: { cpu: 4, memory_gib: 4, disk_gib: 40 },
  });
  const claimed = page.getByRole("row").filter({ hasText: "ebox-my-project" });
  await expect(claimed.getByText("Running", { exact: true })).toBeVisible();
  await expect(
    claimed.getByText("VM: ebox-pool-ready", { exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText("Warm allocation · ready in 0.15 s", { exact: false }),
  ).toBeVisible();
  await expect(assigned.locator("strong")).toHaveText("3");
  const action = page.waitForRequest(
    (req) => req.method() === "POST" && req.url().endsWith("/actions"),
  );
  await claimed.getByRole("button", { name: "Pause", exact: true }).click();
  expect(new URL((await action).url()).pathname).toBe(
    "/api/control/v1/hosts/metal-01/instances/ebox-pool-ready/actions",
  );
  await expect(claimed.getByText("Paused", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "New workspace" }).click();
  await expect(
    page.getByRole("button", { name: "Create workspace", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByText(
      "1 matching workspace is preparing. Availability refreshes automatically.",
    ),
  ).toBeVisible();
  await page.screenshot({
    path: "test-results/warm-reserve-desktop.png",
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/warm-reserve-mobile.png",
    fullPage: true,
  });
});

test("template minimum and custom resources validate before a cold allocation", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "typed" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "New workspace" }).click();
  await page.getByLabel("Workspace name").fill("ebox-sized-project");
  const size = page.getByRole("combobox", { name: "Size", exact: true });
  await expect(size).toHaveValue("recommended");
  await expect(size.locator('option[value="minimum"]')).toHaveText(
    "Minimum · 2 vCPU / 3 GiB / 35 GiB disk",
  );
  await expect(size.locator('option[value="recommended"]')).toHaveText(
    "Recommended · 4 vCPU / 6 GiB / 50 GiB disk",
  );
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.setViewportSize({ width: 1440, height: 1000 });
  await expect(
    page.getByText("4 vCPU · 6 GiB RAM · 50 GiB disk", { exact: true }),
  ).toBeVisible();
  await size.selectOption("minimum");
  await expect(
    page.getByText("2 vCPU · 3 GiB RAM · 35 GiB disk", { exact: true }),
  ).toBeVisible();
  await size.selectOption("custom");
  await page.getByLabel("CPU cores").fill("1");
  const button = page.getByRole("button", {
    name: "Create workspace",
    exact: true,
  });
  await expect(button).toBeDisabled();
  await expect(
    page.getByText(
      "Custom resources must be whole numbers at or above the template minimum.",
    ),
  ).toBeVisible();
  await page.getByLabel("CPU cores").fill("3");
  await page.getByLabel("Memory (GiB)").fill("5");
  await page.getByLabel("Disk (GiB)").fill("35");
  await expect(button).toBeEnabled();
  const submitted = page.waitForRequest(
    (req) => req.method() === "POST" && req.url().endsWith("/allocations"),
  );
  await button.click();
  expect((await submitted).postDataJSON()).toEqual({
    name: "ebox-sized-project",
    template: "native",
    resources: { cpu: 3, memory_gib: 5, disk_gib: 35 },
  });
  await expect(
    page.getByText("Cold start · ready in 12.40 s", { exact: false }),
  ).toBeVisible();
});

test("preparing reserves remain unavailable and refill errors are visible", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "pool-preparing" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "New workspace" }).click();
  await expect(
    page.getByRole("button", { name: "Create workspace", exact: true }),
  ).toBeDisabled();
  await expect(
    page.getByText(
      "2 matching workspaces are preparing. Availability refreshes automatically.",
    ),
  ).toBeVisible();
  await expect(
    page.getByText("Refill is waiting for host capacity.").first(),
  ).toBeVisible();
});

test("canonical templates import once and automatically sync revisions and future nodes", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "catalog" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "Templates", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Template catalog" }),
  ).toBeVisible();
  await page.getByLabel("Import from").selectOption("json");
  await page.getByLabel("Template JSON").fill("{invalid");
  await page.getByRole("button", { name: "Register template" }).click();
  await expect(page.getByRole("alert")).toContainText("must be valid JSON");
  const manifest = {
    schema_version: 1,
    id: "web-dev",
    revision: 1,
    label: "Web development",
    recipe: "native-clients-v1",
    source_image: "images:ubuntu/24.04/cloud",
    minimum: { cpu: 2, memory_gib: 2, disk_gib: 30 },
    recommended: { cpu: 4, memory_gib: 4, disk_gib: 40 },
  };
  await page.getByLabel("Template JSON").fill(JSON.stringify(manifest));
  await page.getByRole("button", { name: "Register template" }).click();
  const card = page
    .locator(".catalog-card")
    .filter({ hasText: "Web development" });
  await expect(page.getByLabel("Install host")).toHaveCount(0);
  await expect(card.getByText("Syncing", { exact: true })).toBeVisible();
  await expect(
    card.getByText("Installed revision 1 · Up to date"),
  ).toBeVisible();
  await page
    .getByLabel("Template JSON")
    .fill(JSON.stringify({ ...manifest, revision: 2 }));
  await page.getByRole("button", { name: "Register template" }).click();
  await expect(
    card.getByText("Installed revision 1 · Awaiting synchronization"),
  ).toBeVisible();
  await expect(
    card.getByText("Installed revision 2 · Up to date"),
  ).toBeVisible();
  await page.getByLabel("Import from").selectOption("url");
  await page
    .getByLabel("Template URL")
    .fill("https://example.com/template.json");
  await page.getByRole("button", { name: "Register template" }).click();
  await expect(
    page.locator(".catalog-card").filter({ hasText: "url-native" }),
  ).toBeVisible();
  const requests: string[] = [];
  page.on("request", (req) => {
    if (req.method() === "POST") requests.push(req.url());
  });
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "catalog-new-node" },
  });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  const future = card.getByRole("listitem", {
    name: "New machine synchronization",
  });
  await expect(future.getByText("Syncing", { exact: true })).toBeVisible();
  await expect(future.getByText("Ready", { exact: true })).toBeVisible();
  expect(requests).toEqual([]);
  await expect(
    card.getByText("2 / 3 nodes ready · target revision 2"),
  ).toBeVisible();
  await expect(
    card.getByText(
      "Node is offline; synchronization will retry when it reconnects.",
    ),
  ).toBeVisible();
  await page.screenshot({
    path: "test-results/template-catalog-desktop.png",
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/template-catalog-mobile.png",
    fullPage: true,
  });
});

test("template node failures stay explicit and fleet-wide retry recovers capacity errors", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "catalog-sync-error" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "Templates", exact: true }).click();
  await page.getByLabel("Import from").selectOption("url");
  await page.getByLabel("Template URL").fill("https://example.com/native.json");
  await page.getByRole("button", { name: "Register template" }).click();
  const card = page.locator(".catalog-card").filter({ hasText: "url-native" });
  await expect(
    card.getByText("Not enough disk capacity to prepare this template."),
  ).toBeVisible();
  await expect(
    card.getByText("0 / 2 nodes ready · target revision 1"),
  ).toBeVisible();
  const retry = page.waitForRequest(
    (req) =>
      req.method() === "POST" &&
      req.url().endsWith("/v1/templates/url-native/sync"),
  );
  await card.getByRole("button", { name: "Retry sync" }).click();
  expect((await retry).postDataJSON()).toEqual({});
  await expect(
    card.getByText("1 / 2 nodes ready · target revision 1"),
  ).toBeVisible();
  await expect(
    card.getByText("Not enough disk capacity to prepare this template."),
  ).toHaveCount(0);
});

test("exported templates download as portable manifests and can be reimported", async ({
  page,
  request,
}) => {
  await request.post("http://127.0.0.1:18765/__test/mode", {
    data: { mode: "catalog" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "Templates", exact: true }).click();
  const card = page
    .locator(".catalog-card")
    .filter({ hasText: "Native coding agents" });
  await expect(
    card.getByText("Installed revision 1 · Up to date"),
  ).toBeVisible();
  const downloaded = page.waitForEvent("download");
  await card.getByRole("button", { name: "Export JSON" }).click();
  const download = await downloaded;
  expect(download.suggestedFilename()).toBe("native-r1.json");
  await download.saveAs("test-results/native-r1.json");
  const serialized = await readFile("test-results/native-r1.json", "utf8");
  const manifest = JSON.parse(serialized);
  expect(Object.keys(manifest).sort()).toEqual(
    [
      "schema_version",
      "id",
      "revision",
      "label",
      "description",
      "recipe",
      "source_image",
      "minimum",
      "recommended",
      "native_clients",
      "config",
    ].sort(),
  );
  expect(manifest.schema_version).toBe(1);
  expect(manifest.minimum).toEqual({ cpu: 2, memory_gib: 2, disk_gib: 30 });
  expect(manifest.recommended).toEqual({ cpu: 4, memory_gib: 4, disk_gib: 40 });
  expect(serialized).not.toContain("ebox-native-base");
  expect(serialized).not.toContain("metal-01");
  await page.getByLabel("Import from").selectOption("json");
  await page.getByLabel("Template JSON").fill(serialized);
  await page.getByRole("button", { name: "Register template" }).click();
  await expect(
    page.getByText(
      "Template registered. Automatic synchronization is queued across all nodes.",
    ),
  ).toBeVisible();
  await expect(
    card.getByText("Installed revision 1 · Up to date"),
  ).toBeVisible();
});

test("mobile layout has no page overflow and remains operable", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await signIn(page);
  await expect(
    page.getByText("Build machine", { exact: true }).first(),
  ).toBeVisible();
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.getByRole("button", { name: "New workspace" }).click();
  await expect(page.getByLabel("Workspace name")).toBeVisible();
  await page.screenshot({
    path: "test-results/fleet-mobile.png",
    fullPage: true,
  });
});
