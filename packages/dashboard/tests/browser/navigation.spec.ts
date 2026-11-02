import { test, expect } from "@playwright/test";
import { openSignedIn } from "./session";
import { fixtureOrigin } from "./urls";

test.beforeEach(async ({ request }) => {
  await request.post(`${fixtureOrigin}/__test/reset`);
});

test("Overview lists what needs a person and links to the page that fixes it", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(error.message));
  await openSignedIn(page);
  await expect(page.getByRole("heading", { name: "Overview", exact: true })).toBeVisible();
  const attention = page.getByRole("region", { name: "Needs attention" });
  await expect(attention.getByText("Spare machine is offline")).toBeVisible();
  await expect(attention.getByText("SSH connection timed out.")).toBeVisible();
  await attention.getByRole("button", { name: "Open machines" }).click();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  await expect(page).toHaveURL(/\?panel=compute$/);
  await page.goBack();
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Overview");
  expect(errors).toEqual([]);
});

test("Jump to opens any view by name from the keyboard", async ({ page }) => {
  await openSignedIn(page);
  await expect(page.getByRole("heading", { name: "Overview", exact: true })).toBeVisible();
  await page.keyboard.press("ControlOrMeta+k");
  const dialog = page.getByRole("dialog", { name: "Jump to" });
  await expect(dialog).toBeVisible();
  await page.keyboard.type("runner");
  await expect(dialog.getByRole("option")).toHaveCount(1);
  await page.keyboard.press("Enter");
  await expect(dialog).toHaveCount(0);
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Actions runners");
  await page.keyboard.press("/");
  await page.keyboard.press("Escape");
  await expect(dialog).toHaveCount(0);
});

test("legacy panel links land on their new pages", async ({ page }) => {
  await openSignedIn(page, "/?panel=fleet");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  await page.goto("/?panel=mcp");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("MCP & apps");
  await page.goto("/?panel=github");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("GitHub");
  await page.goto("/?panel=repositories");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Projects");
  await page.goto("/?panel=instances");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Machines");
  await page.goto("/?panel=toolchains");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Templates");
  await expect(page.getByRole("tab", { name: "Build toolchains" })).toHaveAttribute("aria-selected", "true");
  await page.goto("/?panel=environments");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Projects");
  await expect(page.getByRole("tab", { name: "Previews & routes" })).toHaveAttribute("aria-selected", "true");
  await page.goto("/?panel=unknown");
  await expect(page.getByRole("heading", { level: 1 })).toHaveText("Overview");
});
