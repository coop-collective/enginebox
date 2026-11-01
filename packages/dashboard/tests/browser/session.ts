import type { Page } from "@playwright/test";
import { memberCookie } from "../session-fixture";
import { dashboardOrigin, ownerEmail } from "./urls";

/**
 * Opens a real member session for `email` (the instance owner by default)
 * and returns the cookie to add to a browser context. urls.ts points this
 * worker at the same identity state and secret as the dashboard under test.
 */
export async function signedInCookie(email = ownerEmail): Promise<{ name: string; value: string }> {
  const pair = memberCookie(email);
  const index = pair.indexOf("=");
  return { name: pair.slice(0, index), value: pair.slice(index + 1) };
}

/** Adds a fresh session cookie and opens `path`. */
export async function openSignedIn(page: Page, path = "/", email = ownerEmail) {
  const cookie = await signedInCookie(email);
  await page.context().addCookies([{ ...cookie, url: dashboardOrigin, httpOnly: true, sameSite: "Strict" }]);
  await page.goto(path);
}
