import { test, expect, type Page } from "@playwright/test";

const fixture = "http://127.0.0.1:18765";
async function signIn(page: Page) {
  await page.goto("/");
  await page.getByLabel("Operator password").fill("fixture-dashboard-password");
  await page.getByRole("button", { name: "Open dashboard" }).click();
  await expect(
    page.getByRole("heading", { name: "Fleet overview." }),
  ).toBeVisible();
}
const provider = (page: Page, name: string) =>
  page.getByRole("article", { name, exact: true });
test.beforeEach(async ({ request }) => {
  await request.post(`${fixture}/__test/reset`);
});

test("native sign-ins show device links, reconnect, accept Claude codes and cancel", async ({
  page,
  request,
}) => {
  await request.post(`${fixture}/__test/mode`, { data: { mode: "accounts" } });
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await signIn(page);
  await page
    .getByRole("row")
    .filter({ hasText: "ebox-editor" })
    .getByRole("button", { name: "Connect accounts" })
    .click();
  await expect(page.getByLabel("Private workspace")).toHaveValue("ebox-editor");
  const codex = provider(page, "Codex (ChatGPT)");
  await codex.getByRole("button", { name: "Connect", exact: true }).click();
  await expect(codex.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  const link = codex.getByRole("link", { name: "Open provider sign-in" });
  await expect(link).toHaveAttribute(
    "href",
    "https://auth.openai.com/codex/device",
  );
  await expect(link).toHaveAttribute("rel", "noopener noreferrer");
  await expect(link).toHaveAttribute("target", "_blank");
  await expect(codex.locator("input")).toHaveCount(0);
  await expect(codex.getByText("Signed in", { exact: true })).toBeVisible();
  await codex.getByRole("button", { name: "Reconnect" }).click();
  await codex.getByRole("button", { name: "Cancel sign-in" }).click();
  await expect(codex.getByText("Cancelled", { exact: true })).toBeVisible();
  const claude = provider(page, "Claude");
  await claude.getByRole("button", { name: "Connect", exact: true }).click();
  await expect(
    claude.getByRole("link", { name: "Open provider sign-in" }),
  ).toHaveAttribute("href", /https:\/\/claude.com\/cai\/oauth\/authorize/);
  const code = claude.getByLabel("Claude authorization code");
  await expect(code).toHaveAttribute("type", "password");
  await expect(code).toHaveAttribute("minlength", "8");
  await expect(code).toHaveAttribute("maxlength", "2048");
  await code.fill("fixture-owner-code");
  const inputRequest = page.waitForRequest(
    (req) => req.method() === "POST" && req.url().endsWith("/input"),
  );
  await claude.getByRole("button", { name: "Submit code" }).click();
  expect((await inputRequest).postDataJSON()).toEqual({
    text: "fixture-owner-code",
  });
  await expect(code).toHaveCount(0);
  await expect(claude.getByText("Signed in", { exact: true })).toBeVisible();
  const cursor = provider(page, "Cursor");
  await cursor.getByRole("button", { name: "Connect", exact: true }).click();
  await cursor.getByRole("button", { name: "Cancel sign-in" }).click();
  await expect(cursor.getByText("Cancelled", { exact: true })).toBeVisible();
  await expect(
    cursor.getByRole("button", { name: "Connect", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByText(
      "Sign-in status does not confirm a paid plan or remaining usage. Accounts are not pooled or copied between workspaces.",
    ),
  ).toBeVisible();
  expect(
    await page.evaluate(() => localStorage.length + sessionStorage.length),
  ).toBe(0);
  expect(errors).toEqual([]);
  await page.screenshot({
    path: "test-results/subscriptions-desktop.png",
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/subscriptions-mobile.png",
    fullPage: true,
  });
});

test("account setup excludes reserves and requires an explicit resume", async ({
  page,
  request,
}) => {
  await request.post(`${fixture}/__test/mode`, { data: { mode: "warm" } });
  await signIn(page);
  for (const name of [
    "ebox-native-base",
    "ebox-pool-ready",
    "ebox-pool-preparing",
    "ebox-pool-quarantined",
  ]) {
    await expect(
      page
        .getByRole("row")
        .filter({ hasText: name })
        .getByRole("button", { name: "Connect accounts" }),
    ).toHaveCount(0);
  }
  const mutations: string[] = [];
  page.on("request", (req) => {
    if (req.method() === "POST" && req.url().endsWith("/actions"))
      mutations.push(req.url());
  });
  await page
    .getByRole("row")
    .filter({ hasText: "ebox-review" })
    .getByRole("button", { name: "Connect accounts" })
    .click();
  await expect(
    page.getByText(
      "Resume this workspace to connect accounts. Sign-in commands need a running workspace.",
    ),
  ).toBeVisible();
  expect(mutations).toEqual([]);
  const options = await page
    .getByLabel("Private workspace")
    .locator("option")
    .allTextContents();
  expect(options.join(" ")).not.toMatch(/ebox-pool|ebox-native-base/);
  await page.getByRole("button", { name: "Resume workspace" }).click();
  await expect(provider(page, "Codex (ChatGPT)")).toBeVisible();
  expect(mutations).toHaveLength(1);
  expect(mutations[0]).toContain("/instances/ebox-review/actions");
});

test("switching workspaces hides delayed sessions and closing or logout stops polling", async ({
  page,
  request,
}) => {
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-delayed" },
  });
  await signIn(page);
  await page
    .getByRole("button", { name: "Subscriptions", exact: true })
    .click();
  await provider(page, "Codex (ChatGPT)")
    .getByRole("button", { name: "Connect", exact: true })
    .click();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  const polls: string[] = [];
  page.on("request", (req) => {
    if (
      req.method() === "GET" &&
      req.url().includes("/accounts/codex/sessions/")
    )
      polls.push(req.url());
  });
  await page.waitForRequest(
    (req) =>
      req.method() === "GET" && req.url().includes("/accounts/codex/sessions/"),
  );
  await page.getByLabel("Private workspace").selectOption("ebox-review");
  await expect(
    provider(page, "Codex (ChatGPT)").getByText("Signed in", { exact: true }),
  ).toBeVisible();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toHaveCount(0);
  const switchedCount = polls.length;
  await page.waitForTimeout(1300);
  expect(polls).toHaveLength(switchedCount);
  await page.getByLabel("Private workspace").selectOption("ebox-editor");
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  await page.reload();
  await page
    .getByRole("button", { name: "Subscriptions", exact: true })
    .click();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Close subscriptions" }).click();
  const closedCount = polls.length;
  await page.waitForTimeout(1300);
  expect(polls).toHaveLength(closedCount);
  await page
    .getByRole("button", { name: "Subscriptions", exact: true })
    .click();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page.getByLabel("Operator password")).toBeVisible();
  const loggedOutCount = polls.length;
  await page.waitForTimeout(1300);
  expect(polls).toHaveLength(loggedOutCount);
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toHaveCount(0);
});

test("account endpoints require dashboard auth and status failures remain visible", async ({
  page,
  request,
}) => {
  const path = "/api/control/v1/hosts/metal-01/instances/ebox-editor/accounts";
  expect((await request.get(path)).status()).toBe(401);
  expect(
    (await request.post(`${path}/codex/login`, { data: {} })).status(),
  ).toBe(401);
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-unavailable" },
  });
  await signIn(page);
  await page
    .getByRole("button", { name: "Subscriptions", exact: true })
    .click();
  await expect(
    provider(page, "Cursor").getByText("Unavailable", { exact: true }),
  ).toBeVisible();
  await expect(
    provider(page, "Cursor").getByRole("button", {
      name: "Connect",
      exact: true,
    }),
  ).toBeDisabled();
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-error" },
  });
  await page.getByRole("button", { name: "Refresh accounts" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Native account status is unavailable.",
  );
});
