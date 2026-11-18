// Trust with other organizations on one host (spec/19-tenancy.md D64–D66,
// D13): the Admissions page lists trusted directories (none here) and asks
// for offers; the tenant page offers to accept one and refuses a token that
// isn't one. The two-organization flow runs in the control
// plane's trust tests and on real hosts (spec/20-idp-checks.md).
import { expect, rpcRoute, test, waitForRpc } from "../fixtures";

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

// T11 (D92, D93, D99): the directory's Clusters tab lists the clusters it
// trusts, takes a directory offer with automatic opt-in, toggles it and
// untrusts. The calls are mocked: one host has no other organization's
// cluster to trust; the trust tests and the real check cover the rest.
test("the directory's Clusters tab trusts a cluster as a whole", async ({ page }) => {
  let auto = false;
  const cluster = () => ({
    id: "c-store",
    name: "store-north",
    api: "https://store.example:8443",
    whole: true,
    admit: "ADMIT_SETTING_OPEN",
    auto_opt_in: auto,
    tenants: [
      { id: "t1", slug: "lab-a", status: "CLUSTER_ADMISSION_ADMITTED" },
      { id: "t2", slug: "lab-b", status: "CLUSTER_ADMISSION_PENDING" },
    ],
  });
  let trusted = false;
  await rpcRoute(page, "glidex.directory.v1.ClusterTrustService/ListDirectoryClusters", () => ({
    message: { clusters: trusted ? [cluster()] : [] },
  }));
  await rpcRoute(page, "glidex.directory.v1.ClusterTrustService/TrustCluster", (req) => {
    trusted = true;
    auto = !!req.auto_opt_in;
    return { message: { cluster: cluster() } };
  });
  await rpcRoute(page, "glidex.directory.v1.ClusterTrustService/UpdateDirectoryCluster", (req) => {
    auto = !!req.auto_opt_in;
    return { message: { cluster: cluster() } };
  });
  await rpcRoute(page, "glidex.directory.v1.ClusterTrustService/UntrustCluster", () => {
    trusted = false;
    return { message: {} };
  });

  await page.goto("/directory/clusters");
  await expect(page.getByText("None yet.")).toBeVisible();
  const offer = page.getByLabel("The directory offer its operator sent you");
  await expect(offer).toHaveAttribute("type", "password");
  await offer.fill("gxo_test-offer");
  await page.locator("#trust-auto-opt-in").check();
  const trust = waitForRpc(page, "glidex.directory.v1.ClusterTrustService/TrustCluster");
  await page.getByRole("button", { name: "Trust cluster" }).click();
  expect(await trust).toEqual({ offer: "gxo_test-offer", auto_opt_in: true });
  await expect(offer).toHaveValue("");

  const row = page.getByTestId("directory-clusters").getByRole("row").filter({ hasText: "store-north" });
  await expect(row.getByText("As a whole; tenants admitted at once")).toBeVisible();
  await expect(row.getByText("lab-a")).toBeVisible();
  await expect(row.getByText("pending")).toBeVisible();
  const toggle = row.getByLabel("Automatic opt-in for store-north");
  await expect(toggle).toBeChecked();
  const update = waitForRpc(page, "glidex.directory.v1.ClusterTrustService/UpdateDirectoryCluster");
  await toggle.uncheck();
  expect(await update).toEqual({ cluster: "c-store", auto_opt_in: false });
  await expect(toggle).not.toBeChecked();

  await row.getByRole("button", { name: "Untrust" }).click();
  const untrust = waitForRpc(page, "glidex.directory.v1.ClusterTrustService/UntrustCluster");
  await page.getByRole("dialog").getByRole("button", { name: "Untrust" }).click();
  expect(await untrust).toEqual({ cluster: "c-store" });
  await expect(page.getByText("None yet.")).toBeVisible();
});

// D93, D97: the tenant page offers the clusters its directory trusts as a
// whole, opts in with no offer, and shows a pending and a refused opt-in,
// which may ask again.
test("the tenant page opts in to a cluster its directory trusts", async ({ page }) => {
  let status = "TENANT_CLUSTER_STATUS_AVAILABLE";
  const store = () => ({ cluster_id: "c-store", name: "store-north", api: "https://store.example:8443", whole: true, status });
  const refused = { cluster_id: "c-vms", name: "vms-north", api: "https://vms.example:8841", whole: true, status: "TENANT_CLUSTER_STATUS_REFUSED" };
  await rpcRoute(page, "glidex.tenancy.v1.TenantService/ListTenantClusters", (req) => {
    expect(req.available).toBe(true);
    return { message: { clusters: [store(), refused] } };
  });
  await rpcRoute(page, "glidex.tenancy.v1.TenantService/OptInCluster", (req) => {
    if (req.cluster === "c-store") status = "TENANT_CLUSTER_STATUS_PENDING";
    return { message: { cluster: req.cluster === "c-store" ? store() : refused } };
  });

  await page.goto("/tenants/default");
  const available = page.getByTestId("tenant-clusters-available");
  await expect(available.getByText("store-north")).toBeVisible();
  const optIn = waitForRpc(page, "glidex.tenancy.v1.TenantService/OptInCluster");
  await available.getByRole("button", { name: "Opt in" }).click();
  expect(await optIn).toMatchObject({ cluster: "c-store" });

  const table = page.getByTestId("tenant-clusters");
  const pending = table.getByRole("row").filter({ hasText: "store-north" });
  await expect(pending.getByText("pending")).toBeVisible();
  await expect(pending.getByText("Waiting for the cluster's operator to approve")).toBeVisible();
  await expect(available).toHaveCount(0);

  const no = table.getByRole("row").filter({ hasText: "vms-north" });
  await expect(no.getByText("refused")).toBeVisible();
  const again = waitForRpc(page, "glidex.tenancy.v1.TenantService/OptInCluster");
  await no.getByRole("button", { name: "Ask again" }).click();
  expect(await again).toMatchObject({ cluster: "c-vms" });
});
