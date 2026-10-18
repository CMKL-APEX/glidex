// Invitations (spec/tenancy.md §6.1): made on the tenant's page, the link
// shown once; the link's page works signed out, says what it is for and
// starts the login through its connector. The login itself needs an IdP,
// which the connector tests cover against a mock.
import { api, del, expect, test } from "../fixtures";

const CONNECTOR = "e2e-invite-sso";
const EMAIL = "invitee@example.org";

test("an invitation is made on the tenant page and its link opens signed out", async ({ page, browser }) => {
  const made = await api("/connectors", {
    method: "POST",
    body: { connector: { id: CONNECTOR, display_name: "E2E SSO", oidc: { issuer: "https://idp.example", client_id: "glidex" } } },
  });
  expect(made.connector?.id, JSON.stringify(made)).toBe(CONNECTOR);
  try {
    await page.goto("/tenants/default");
    await expect(page.getByRole("heading", { name: "Invitations" })).toBeVisible();
    await page.locator("#invite-email").fill(EMAIL);
    await page.locator("#invite-role").selectOption("role.tenant-auditor");
    await page.locator("#invite-connector").selectOption(CONNECTOR);
    await page.getByRole("button", { name: "Invite", exact: true }).click();
    const shown = page.getByTestId("invitation-link").locator("code");
    await expect(shown).toContainText("/invite/");
    const link = (await shown.textContent())!.trim();
    expect(link).toMatch(/\/invite\/[0-9a-f]+#[A-Za-z0-9_-]+$/);
    const secret = link.split("#")[1];
    await expect(page.getByRole("cell", { name: EMAIL })).toBeVisible();
    // Listed without its secret, and gone from the page once dismissed.
    expect(JSON.stringify(await api("/tenants/default/invitations"))).not.toContain(secret);
    await page.getByRole("button", { name: "Done" }).click();
    await expect(page.getByTestId("invitation-link")).toBeHidden();

    // The link, signed out: what it is for, and the fragment leaves the address.
    const context = await browser.newContext({ storageState: { cookies: [], origins: [] } });
    try {
      const guest = await context.newPage();
      const u = new URL(link);
      await guest.goto(u.pathname + u.hash);
      await expect(guest.getByRole("heading", { name: "You've been invited" })).toBeVisible();
      await expect(guest.getByText(EMAIL)).toBeVisible();
      await expect(guest.getByText("tenant auditor")).toBeVisible();
      await expect(guest.getByRole("button", { name: "Sign in with E2E SSO" })).toBeVisible();
      expect(guest.url()).not.toContain("#");
      // A wrong link says so (a new page: a changed fragment alone loads nothing).
      const wrong = await context.newPage();
      await wrong.goto(u.pathname + "#wrong");
      await expect(wrong.getByRole("alert")).toContainText("can't be used");
    } finally {
      await context.close();
    }

    // Revoked, it is gone.
    await page.getByRole("row", { name: new RegExp(EMAIL) }).getByRole("button", { name: "Revoke" }).click();
    await expect(page.getByRole("cell", { name: EMAIL })).toBeHidden();
  } finally {
    const left = (await api<{ invitations?: { id: string }[] }>("/tenants/default/invitations")).invitations ?? [];
    for (const i of left) await del(`/invitations/${i.id}`);
    await del(`/connectors/${CONNECTOR}`);
  }
});
