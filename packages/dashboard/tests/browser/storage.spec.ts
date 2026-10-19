import { test, expect, type Page } from "@playwright/test";
import { createHmac, randomBytes } from "node:crypto";

async function openStorage(page: Page) {
  // Login itself is exercised by fleet/account tests. Use a signed fixture
  // session here so additional feature tests do not consume the production
  // twenty-attempt login budget or require weakening that protection.
  const payload = Buffer.from(JSON.stringify({ expires: Date.now() + 3_600_000, nonce: randomBytes(24).toString("hex") })).toString("base64url");
  const signature = createHmac("sha256", "fixture-session-secret-at-least-32-characters").update("fixture-dashboard-password").update("\0").update(payload).digest("base64url");
  await page.context().addCookies([{ name: "enginebox_session", value: `${payload}.${signature}`, url: "http://127.0.0.1:3007", httpOnly: true, sameSite: "Strict" }]);
  await page.goto("/");
  await page.getByRole("navigation", { name: "Control plane" }).getByRole("button", { name: "Storage", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Shared checkout storage" })).toBeVisible();
}
test.beforeEach(async ({ request }) => { await request.post("http://127.0.0.1:18765/__test/reset"); });

test("storage source review binds import to the approved commit and supports prepared forks", async ({ page, request }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await openStorage(page);
  await page.getByRole("button", { name: "Inspect node" }).click();
  await expect(page.getByText("Node prerequisites passed.", { exact: false })).toBeVisible();
  await page.getByRole("combobox", { name: "Project", exact: true }).selectOption("demo");
  await page.getByText("Review and import a source revision", { exact: true }).click();
  await page.getByRole("button", { name: "Review source", exact: true }).click();
  await expect(page.getByLabel("Tracked file list")).toContainText("src/main.ts");
  const importButton = page.getByRole("button", { name: "Import reviewed commit" });
  await expect(importButton).toBeDisabled();
  await page.getByRole("checkbox").check();
  await importButton.click();
  await expect(page.getByText("Import completed.", { exact: true })).toBeVisible();
  await page.getByLabel("Checkout name").fill("new-agent");
  await page.getByRole("button", { name: "Create checkout" }).click();
  await expect(page.getByRole("region", { name: "Checkout new-agent", exact: true })).toBeVisible();
  await page.getByRole("combobox", { name: "Project", exact: true }).selectOption("game");
  await page.getByRole("combobox", { name: "Start from", exact: true }).selectOption("prepared");
  await page.getByLabel("Checkout name").fill("cached-agent");
  await page.getByRole("button", { name: "Create checkout" }).click();
  await expect(page.getByRole("region", { name: "Checkout cached-agent", exact: true })).toBeVisible();
  const requests = await (await request.get("http://127.0.0.1:18765/__test/storage-requests")).json();
  expect(requests.find((r: { operation: string }) => r.operation === "import").body).toEqual({ project: "demo", revision: "a".repeat(40), approval: "b".repeat(64), credential_free: true });
  expect(requests.filter((r: { operation: string }) => r.operation === "fork").at(-1).body.from).toBe("prepared");
  expect(JSON.stringify(requests)).not.toContain("/srv/");
  expect(errors).toEqual([]);
  await page.screenshot({ path: "test-results/storage-desktop.png", fullPage: true });
  await page.setViewportSize({ width: 390, height: 844 });
  expect(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth)).toBe(true);
  await page.screenshot({ path: "test-results/storage-mobile.png", fullPage: true });
});

test("storage actions disable only their own checkout and show refusals and recovery", async ({ page, request }) => {
  await request.post("http://127.0.0.1:18765/__test/mode", { data: { mode: "storage-slow" } });
  await openStorage(page);
  const a = page.getByRole("region", { name: "Checkout worker-a", exact: true });
  const b = page.getByRole("region", { name: "Checkout worker-b", exact: true });
  await a.getByRole("button", { name: "Checkpoint", exact: true }).click();
  await expect(a.getByRole("button", { name: "Remount", exact: true })).toBeDisabled();
  await expect(b.getByRole("button", { name: "Remount", exact: true })).toBeEnabled();
  await expect(b.getByText("Ready", { exact: true })).toBeVisible();
  await expect(a.getByText("Checkpoint completed.")).toBeVisible();
  await b.getByRole("button", { name: "Close", exact: true }).click();
  await expect(b.getByRole("alert")).toContainText("uncheckpointed changes");
  await expect(b.getByText("Ready", { exact: true })).toBeVisible();
  const interrupted = page.getByRole("region", { name: "Checkout interrupted", exact: true });
  await interrupted.getByRole("button", { name: "Inspect recovery" }).click();
  await expect(interrupted.getByText("Ready", { exact: true })).toBeVisible();
});

for (const status of [404, 503]) test(`lost job observation (${status}) never resubmits the command and can check the existing job`, async ({ page, request }) => {
  await openStorage(page);
  const a = page.getByRole("region", { name: "Checkout worker-a", exact: true });
  await expect(a).toBeVisible();
  let lost = false;
  await page.route("**/api/control/v1/jobs/storage-*", async (route) => {
    if (!lost) { lost = true; return route.fulfill({ status, contentType: "application/json", body: JSON.stringify({ error: "Temporary status outage." }) }); }
    return route.continue();
  });
  await a.getByRole("button", { name: "Remount", exact: true }).click();
  await expect(a.getByRole("alert")).toContainText("No command was resubmitted");
  await expect(a.getByRole("button", { name: "Remount", exact: true })).toBeDisabled();
  await a.getByRole("button", { name: "Check job status" }).click();
  await expect(a.getByText("Remount completed.")).toBeVisible();
  const requests = await (await request.get("http://127.0.0.1:18765/__test/storage-requests")).json();
  expect(requests.filter((r: { operation: string }) => r.operation === "remount")).toHaveLength(1);
});

test("empty and unavailable storage are explicit and proxy rejects arbitrary commands", async ({ page, request }) => {
  await request.post("http://127.0.0.1:18765/__test/mode", { data: { mode: "storage-empty" } });
  await openStorage(page);
  await expect(page.getByRole("heading", { name: "Storage setup required" })).toBeVisible();
  expect((await page.request.post("/api/control/v1/storage/hosts/metal-01/exec", { headers: { origin: "http://127.0.0.1:3007" }, data: { argv: ["id"] } })).status()).toBe(404);
  await request.post("http://127.0.0.1:18765/__test/mode", { data: { mode: "storage-error" } });
  await page.getByRole("button", { name: "Refresh", exact: true }).click();
  await expect(page.getByRole("alert")).toContainText("Storage catalog unavailable");
});
