// Trust with other organizations on one host (spec/19-tenancy.md D64–D66,
// D13): the Admissions page lists trusted directories (none here) and asks
// for offers; the tenant page offers to accept one and refuses a token that
// isn't one. The two-organization flow runs in the control
// plane's trust tests and on real hosts (spec/20-idp-checks.md).
import { expect, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /status of (400|403)/ });

test("the Admissions page lists trusted directories and makes offers", async ({ page }) => {
  await page.goto("/admissions");
  await expect(page.getByRole("heading", { name: "Directories of other organizations" })).toBeVisible();
  await expect(page.getByText("None yet. Send a tenant's administrator an offer")).toBeVisible();
  await page.getByLabel("Tenant (its slug there)").fill("acme");
  await page.getByLabel("Its directory's issuer").fill("https://id.acme.example:8841/d/local");
  await page.getByRole("button", { name: "Make offer" }).click();
  // The scratch control plane serves its API without TLS: an offer needs a
  // certificate the other organization can pin, so it is refused, and says why.
  await expect(page.getByText("this cluster's API has no TLS certificate to pin")).toBeVisible();
  await expect(page.getByTestId("offer-token")).toHaveCount(0);
  await expect(page.getByText("No offers.")).toBeVisible();
});

test("the tenant page refuses something that isn't an offer", async ({ page }) => {
  await page.goto("/tenants/default");
  const offer = page.getByLabel("Use a cluster: the offer its operator sent you");
  await expect(offer).toBeVisible();
  await offer.fill("gxo_not-an-offer");
  await page.getByRole("button", { name: "Accept offer" }).click();
  await expect(page.getByText(/offer/i).filter({ hasText: /not|refused|invalid/i }).first()).toBeVisible();
});
