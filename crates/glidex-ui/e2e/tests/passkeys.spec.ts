// Passkeys (spec/25-passkeys.md §5): one added on the Account page with
// Chromium's virtual authenticator, then a sign-in with it from the
// login page, without a user name.
import { api, del, expect, test } from "../fixtures";

// Signing out on purpose: the page's requests answer 401 until the
// passkey signs it in again.
test.use({ ignoredConsoleErrors: /status of 401/ });

test("a passkey is added on the Account page and signs in", async ({ page }) => {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: { protocol: "ctap2", transport: "internal", hasResidentKey: true, hasUserVerification: true, isUserVerified: true },
  });
  const me = await api("/auth/whoami");
  try {
    await page.goto("/account");
    const card = page.locator("section", { has: page.getByRole("heading", { name: "Passkeys" }) });
    await expect(card.getByText("No passkeys yet.")).toBeVisible();
    await card.locator("#passkey-name").fill("e2e virtual key");
    await card.getByRole("button", { name: "Add a passkey" }).click();
    await expect(card.getByText("Passkey added")).toBeVisible();
    await expect(card.getByTestId("passkeys")).toContainText("e2e virtual key");
    // A sign-in method of the user's now.
    await expect(page.getByTestId("sign-in-methods-list")).toContainText("Passkey");

    // Sign out, and back in with the passkey: no user name, no password.
    await page.context().clearCookies();
    await page.goto("/");
    const finished = page.waitForResponse((r) => r.url().includes("/auth/passkeys/login/finish"));
    await page.getByRole("button", { name: "Sign in with a passkey" }).click();
    const r = await finished;
    expect(r.status(), await r.text()).toBe(200);
    // Signed in: the login page is gone.
    await expect(page.getByRole("heading", { name: "Sign in to the VM Control Panel" })).toHaveCount(0);
    await page.goto("/account");
    await expect(page.locator("section", { has: page.getByRole("heading", { name: "Passkeys" }) })).toContainText("last used");
    const w = await page.evaluate(async () => (await fetch("/api/auth/whoami")).json());
    expect(w.user_id ?? w.user?.id).toBe(me.user?.id ?? me.user_id);
    expect(w.method).toBe("passkey");
  } finally {
    const uid = me.user?.id ?? me.user_id;
    const list = await api(`/users/${uid}/passkeys`);
    for (const p of list.passkeys ?? []) await del(`/users/${uid}/passkeys/${p.id}`);
  }
});
