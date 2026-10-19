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

test("Agents distinguishes live allowance, provider credits, reset dates and unknown billing", async ({ page, request }) => {
  await request.post(`${fixture}/__test/mode`, { data: { mode: "usage-current" } });
  await signIn(page);
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  const codex = provider(page, "Codex (ChatGPT)");
  await expect(codex.getByText("75% left", { exact: true })).toBeVisible();
  await expect(codex.getByText("7-day allowance", { exact: true })).toBeVisible();
  await expect(codex.getByText("12.50", { exact: true })).toBeVisible();
  await expect(codex.getByText("Quota reset:", { exact: false })).toBeVisible();
  await expect(codex.getByText("Not reported", { exact: true })).toBeVisible();
  await expect(codex.getByRole("link", { name: "Open provider usage & billing" })).toHaveAttribute("href", "https://chatgpt.com/codex/settings/usage");
  for (const name of ["Claude", "Cursor"]) {
    const card = provider(page, name);
    await expect(card.getByText("Allowance not reported", { exact: true })).toBeVisible();
    await expect(card.getByRole("progressbar")).toHaveCount(0);
  }
  await request.post(`${fixture}/__test/mode`, { data: { mode: "usage-stale" } });
  await page.getByRole("button", { name: "Refresh agents", exact: true }).click();
  await expect(codex.getByText("Last reported", { exact: true })).toBeVisible();
  await expect(codex.getByText("75% left", { exact: true })).toHaveCount(0);
  await expect(codex.getByRole("progressbar")).toHaveCount(0);
  await request.post(`${fixture}/__test/mode`, { data: { mode: "usage-error" } });
  await page.getByRole("button", { name: "Refresh agents", exact: true }).click();
  await expect(codex.getByText("Usage could not be refreshed.", { exact: false })).toBeVisible();
  await expect(codex.getByText("Signed in", { exact: true })).toBeVisible();
});

test("Agents shows native sign-in links, reconnect, Claude codes and cancellation", async ({
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
    .getByRole("button", { name: "Manage agents" })
    .click();
  await expect(page.getByRole("combobox")).toHaveCount(0);
  await expect(
    page.getByRole("region", { name: "Agents", exact: true }),
  ).toBeVisible();
  await expect(
    page.getByRole("heading", { name: "Agents", exact: true }),
  ).toBeVisible();
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
  await expect(
    codex.getByText("Available to 2 private workspaces."),
  ).toBeVisible();
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
      "Each provider's workspace access is shown on its card. Sign-in status does not confirm a paid plan or remaining usage.",
    ),
  ).toBeVisible();
  expect(
    await page.evaluate(() => localStorage.length + sessionStorage.length),
  ).toBe(0);
  expect(errors).toEqual([]);
  await page.screenshot({
    path: "test-results/agents-desktop.png",
    fullPage: true,
  });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(
    await page.evaluate(
      () => document.documentElement.scrollWidth <= window.innerWidth,
    ),
  ).toBe(true);
  await page.screenshot({
    path: "test-results/agents-mobile.png",
    fullPage: true,
  });
});

test("global agent connections need no host or running workspace and never resume a VM", async ({
  page,
  request,
}) => {
  await request.post(`${fixture}/__test/mode`, { data: { mode: "warm" } });
  await signIn(page);
  const mutations: string[] = [];
  page.on("request", (req) => {
    if (req.method() === "POST") mutations.push(req.url());
  });
  await page
    .getByRole("row")
    .filter({ hasText: "ebox-review" })
    .getByRole("button", { name: "Manage agents" })
    .click();
  await expect(provider(page, "Codex (ChatGPT)")).toBeVisible();
  await expect(page.getByRole("combobox")).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: "Resume workspace" }),
  ).toHaveCount(0);
  expect(mutations).toEqual([]);
  await page.getByRole("button", { name: "Close agents" }).click();
  await request.post(`${fixture}/__test/mode`, { data: { mode: "empty" } });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(
    page.getByRole("heading", { name: "Add your first host" }),
  ).toBeVisible();
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  await expect(provider(page, "Codex (ChatGPT)")).toBeVisible();
});

test("global sessions recover after reload and closing or logout stops every provider poll", async ({
  page,
  request,
}) => {
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-delayed" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  await provider(page, "Codex (ChatGPT)")
    .getByRole("button", { name: "Connect", exact: true })
    .click();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  await provider(page, "Cursor")
    .getByRole("button", { name: "Connect", exact: true })
    .click();
  await provider(page, "Cursor")
    .getByRole("button", { name: "Cancel sign-in" })
    .click();
  await expect(
    provider(page, "Cursor").getByText("Cancelled", { exact: true }),
  ).toBeVisible();
  await expect(
    provider(page, "Codex (ChatGPT)").getByRole("button", {
      name: "Cancel sign-in",
    }),
  ).toBeVisible();
  const polls: string[] = [];
  page.on("request", (req) => {
    if (req.method() === "GET" && req.url().includes("/agents/codex/sessions/"))
      polls.push(req.url());
  });
  await page.waitForRequest(
    (req) =>
      req.method() === "GET" && req.url().includes("/agents/codex/sessions/"),
  );
  await page.getByRole("button", { name: "Close agents" }).click();
  const closedCount = polls.length;
  await page.waitForTimeout(1300);
  expect(polls).toHaveLength(closedCount);
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toHaveCount(0);
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  await page.reload();
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  await expect(page.getByText("ABCD-EFGH", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Sign out" }).click();
  await expect(page.getByLabel("Operator password")).toBeVisible();
  const loggedOutCount = polls.length;
  await page.waitForTimeout(1300);
  expect(polls).toHaveLength(loggedOutCount);
});

test("account endpoints require dashboard auth and status failures remain visible", async ({
  page,
  request,
}) => {
  const path = "/api/control/v1/agents";
  expect((await request.get(path)).status()).toBe(401);
  expect(
    (await request.post(`${path}/codex/login`, { data: {} })).status(),
  ).toBe(401);
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-unavailable" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "Agents", exact: true }).click();
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
  await page.getByRole("button", { name: "Refresh agents" }).click();
  await expect(page.getByRole("alert")).toContainText(
    "Native account status is unavailable.",
  );
  await expect(page.locator(".accounts-grid")).toHaveCount(0);
});

test("signed-in providers report unsupported or expired delegation without claiming workspace access", async ({
  page,
  request,
}) => {
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-unsupported" },
  });
  await signIn(page);
  await page.getByRole("button", { name: "Agents", exact: true }).click();
  const cursor = provider(page, "Cursor");
  await expect(cursor.getByText("Signed in", { exact: true })).toBeVisible();
  await expect(
    cursor.getByText("Workspace access is not supported for this provider."),
  ).toBeVisible();
  await expect(cursor.getByText(/Available to/)).toHaveCount(0);
  await request.post(`${fixture}/__test/mode`, {
    data: { mode: "accounts-reauth" },
  });
  await page.getByRole("button", { name: "Refresh agents" }).click();
  await expect(
    provider(page, "Claude").getByText(
      "Reconnect to restore workspace access.",
    ),
  ).toBeVisible();
  await expect(provider(page, "Claude").getByText(/Available to/)).toHaveCount(
    0,
  );
});
