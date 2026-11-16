// Trust with other organizations on one host (spec/19-tenancy.md D64–D66,
// D13): the Admissions page lists trusted directories (none here) and asks
// for offers; the tenant page offers to accept one and refuses a token that
// isn't one. The two-organization flow runs in the control
// plane's trust tests and on real hosts (spec/20-idp-checks.md).
import { expect, rpcRoute, test } from "../fixtures";

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

// D85: a storage cluster reports only storage, as tenant totals; the tenant
// page shows its volumes' provisioned and used space side by side, and a
// VM cluster's row beside it shows none. The list is mocked: one host has
// no cluster of another organization to report.
test("the tenant page shows a storage cluster's report", async ({ page }) => {
  await rpcRoute(page, "glidex.tenancy.v1.TenantService/ListTenantClusters", () => ({
    message: {
      clusters: [
        {
          cluster_id: "c-store",
          name: "store-north",
          api: "https://store.example:8443",
          report: {
            reported_at: new Date().toISOString(),
            status: "ADMISSION_STATUS_ADMITTED",
            storage: {
              volumes: "2",
              volume_gib: "100",
              volume_used_bytes: "7516192768",
              buckets: "1",
              bucket_bytes: "12345",
              s3_keys: "3",
            },
          },
        },
        {
          cluster_id: "c-vms",
          name: "vms-north",
          api: "https://vms.example:8841",
          report: { reported_at: new Date().toISOString(), status: "ADMISSION_STATUS_ADMITTED", vms: "4", running_vms: "1" },
        },
      ],
    },
  }));
  await page.goto("/tenants/default");
  const table = page.getByTestId("tenant-clusters");
  await expect(table).toBeVisible();
  await expect(table.getByRole("columnheader", { name: "Storage" })).toBeVisible();
  const store = table.getByRole("row").filter({ hasText: "store-north" });
  await expect(store.getByText("2 volumes")).toBeVisible();
  await expect(store.getByText("(100 GiB provisioned · 7.0 GiB used)")).toBeVisible();
  await expect(store.getByText("1 buckets")).toBeVisible();
  await expect(store.getByText("(12.1 KiB)")).toBeVisible();
  await expect(store.getByText("3 S3 keys")).toBeVisible();
  const vms = table.getByRole("row").filter({ hasText: "vms-north" });
  await expect(vms.getByTestId("tenant-cluster-storage")).toHaveText("—");
});
