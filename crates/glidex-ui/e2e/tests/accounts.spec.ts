// Accounts in one browser (spec/19-tenancy.md D71): the host's administrator
// adds a second account, the first administrator of a new directory
// (an invitation and a passkey), then switches between the two from the
// header's account menu and removes the new one on the Account page.
import { expect, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /status of 401/ });

const whoami = (page: import("@playwright/test").Page) => page.evaluate(async () => (await fetch("/api/auth/whoami")).json());

test("a browser holds accounts in two directories and switches between them", async ({ page }) => {
  const id = `e2e-acct-${Date.now().toString(36)}`;
  await page.goto("/");
  const admin = await whoami(page);
  const adminId = admin.user?.id ?? admin.user_id;

  // A new directory and its invitation.
  await page.goto("/directories");
  await page.getByRole("button", { name: "New directory" }).click();
  await page.getByLabel(/^Id/).fill(id);
  await page.getByLabel("Name").fill("Accounts Org");
  await page.getByLabel("Its first administrator's email").fill(`admin@${id}.example`);
  await page.getByRole("button", { name: "Create" }).click();
  const shown = page.getByTestId("directory-invitation");
  await expect(shown).toContainText(id);
  const link = (await shown.locator(".font-mono.break-all").textContent()) ?? "";
  const invite = link.slice(link.indexOf("/invite/"));

  // Add another account: the login page lists the one kept.
  await page.goto("/");
  await page.getByTitle("Your accounts").click();
  await page.getByRole("menuitem", { name: "Add another account" }).click();
  await expect(page.getByRole("heading", { name: "Sign in to the VM Control Panel" })).toBeVisible();
  await expect(page.getByTestId("account-list")).toContainText("This host");

  // The invitation, in this same browser.
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: { protocol: "ctap2", transport: "internal", hasResidentKey: true, hasUserVerification: true, isUserVerified: true },
  });
  await page.goto(invite);
  const finished = page.waitForResponse((r) => r.url().includes("/auth/invitation/passkey/finish"));
  await page.getByRole("button", { name: "Create a passkey and sign in" }).click();
  expect((await finished).status()).toBe(201);
  await page.goto("/");
  const second = await whoami(page);
  expect(second.directory_id).toBe(id);

  // The header's menu switches back to the host's administrator.
  await page.getByTitle("Your accounts").click();
  const menu = page.getByTestId("account-menu");
  await expect(menu).toContainText("Accounts Org");
  await menu.getByRole("menuitem", { name: /This host/ }).click();
  await expect.poll(async () => (await whoami(page)).user?.id).toBe(adminId);

  // And forward again.
  await page.getByTitle("Your accounts").click();
  await page.getByTestId("account-menu").getByRole("menuitem", { name: /Accounts Org/ }).click();
  await expect.poll(async () => (await whoami(page)).directory_id).toBe(id);

  // Back to the host's administrator (the session the other specs share).
  await page.getByTitle("Your accounts").click();
  await page.getByTestId("account-menu").getByRole("menuitem", { name: /This host/ }).click();
  await expect.poll(async () => (await whoami(page)).user?.id).toBe(adminId);

  // The Account page lists both; removing the directory's account signs
  // it out and forgets it.
  await page.goto("/account");
  const list = page.getByTestId("account-list");
  await expect(list).toContainText("This host");
  await expect(list).toContainText("Accounts Org");
  await list.getByRole("listitem").filter({ hasText: "Accounts Org" }).getByRole("button", { name: "Remove" }).click();
  await expect(list).not.toContainText("Accounts Org");
  const accounts = await page.evaluate(async () => (await fetch("/api/auth/accounts")).json());
  expect(accounts.accounts.map((a: any) => a.directory)).toEqual([admin.directory_id]);
});
