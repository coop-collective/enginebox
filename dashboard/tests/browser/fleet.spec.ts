import { test, expect, type Page } from "@playwright/test";

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
  await expect(created.getByText("Stopped", { exact: true })).toBeVisible();
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
    const response = await context.request.post(
      `${origin}/api/control/v1/hosts/metal-01/instances`,
      {
        headers,
        data: { name: "ebox-nope", template: "native", size: "small" },
      },
    );
    expect(response.status()).toBe(403);
  }
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
