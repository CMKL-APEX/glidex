// Hosted directories (spec/19-tenancy.md D59–D61): the operator creates one
// on the Hosting page; its first administrator opens the invitation in
// another browser, creates a passkey (Chromium's virtual authenticator)
// and lands in the new directory, seeing none of the host's own people.
import { api, expect, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /status of 401/ });

test("a directory is created and its first administrator signs in with a passkey", async ({ page, browser }) => {
  const id = `e2e-${Date.now().toString(36)}`;
  await page.goto("/directories");
  await page.getByRole("button", { name: "New directory" }).click();
  await page.getByLabel(/^Id/).fill(id);
  await page.getByLabel("Name").fill("E2E Org");
  await page.getByLabel("Its first administrator's email").fill(`admin@${id}.example`);
  await page.getByRole("button", { name: "Create" }).click();
  const shown = page.getByTestId("directory-invitation");
  await expect(shown).toContainText(id);
  const link = (await shown.locator(".font-mono.break-all").textContent()) ?? "";
  const path = link.slice(link.indexOf("/invite/"));
  expect(path).toMatch(/^\/invite\/[^#]+#.+/);
  await expect(page.getByRole("row", { name: new RegExp(id) })).toContainText(`/d/${id}`);

  // The invitee, in a browser of their own.
  const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const invitee = await context.newPage();
  const cdp = await context.newCDPSession(invitee);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: { protocol: "ctap2", transport: "internal", hasResidentKey: true, hasUserVerification: true, isUserVerified: true },
  });
  try {
    await invitee.goto(path);
    await expect(invitee.getByText("create a passkey for it")).toBeVisible();
    const finished = invitee.waitForResponse((r) => r.url().includes("/auth/invitation/passkey/finish"));
    await invitee.getByRole("button", { name: "Create a passkey and sign in" }).click();
    expect((await finished).status()).toBe(201);
    await expect(invitee.getByRole("heading", { name: "Sign in to the VM Control Panel" })).toHaveCount(0);
    const me = await invitee.evaluate(async () => (await fetch("/api/auth/whoami")).json());
    expect(me.method).toBe("passkey");
    // Their directory has only them; the host's own people are not there.
    const users = await invitee.evaluate(async () => (await fetch("/api/users")).json());
    expect((users.users ?? []).map((u: any) => u.id)).toEqual([me.user?.id ?? me.user_id]);
    const hostUsers = (await api<{ users?: any[] }>("/users")).users ?? [];
    expect(hostUsers.map((u) => u.id)).not.toContain(me.user?.id ?? me.user_id);
  } finally {
    await context.close();
  }

  // The directory's own login page.
  const anon = await browser.newContext({ storageState: { cookies: [], origins: [] } });
  const login = await anon.newPage();
  await login.goto(`/login/${id}`);
  await expect(login.getByTestId("login-directory")).toContainText(id);
  await anon.close();
});
