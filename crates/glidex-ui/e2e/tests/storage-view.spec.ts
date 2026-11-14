// A tenant's storage on a trusted hyperstor cluster (spec/19-tenancy.md
// D86, D87): the tenant page opens it from the cluster's row, and the
// browser reads it only through this server's storage relay, never from
// the cluster. The cluster list and the relay are mocked: one host has no
// hyperstor cluster to trust.
import { expect, rpcRoute, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /status of 502/ });

const CLUSTER = "c-store";
const TENANT = 7;

test("the tenant page shows a storage cluster's volumes, buckets and keys per project", async ({ page }) => {
  const relayed: string[] = [];
  const sent: string[] = [];
  page.on("request", (r) => sent.push(r.url()));
  await rpcRoute(page, "glidex.tenancy.v1.TenantService/ListTenantClusters", () => ({
    message: {
      clusters: [
        {
          cluster_id: CLUSTER,
          name: "store-north",
          api: "https://store.example:8443",
          report: {
            reported_at: new Date().toISOString(),
            status: "ADMISSION_STATUS_ADMITTED",
            storage: { volumes: "2", volume_gib: "30", buckets: "1", bucket_bytes: "2048", s3_keys: "1" },
          },
        },
      ],
    },
  }));
  await page.route(`**/api/tenants/*/clusters/${CLUSTER}/storage/**`, async (route) => {
    const req = route.request();
    expect(req.method()).toBe("POST");
    // The session's CSRF value, as for any call here; no bearer token.
    expect(req.headers()["x-glidex-csrf"]).toBeTruthy();
    expect(req.headers()["authorization"]).toBeUndefined();
    const call = req.url().split("/storage/")[1];
    // The relay's tenant is glidex's id of the page's tenant.
    const glidexTenant = decodeURIComponent(req.url().split("/tenants/")[1].split("/")[0]);
    relayed.push(call);
    const json = (body: unknown) => route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) });
    switch (call) {
      case "hyperstor.v1.TenantService/ListTenants":
        return json({
          tenants: [
            {
              id: TENANT,
              name: "acme",
              glidex_id: glidexTenant,
              default_project: 70,
              projects: [
                { id: 70, name: "default", is_default: true },
                { id: 71, name: "web", glidex_id: "p-web" },
              ],
            },
            { id: 9, name: "other", glidex_id: "someone-else", projects: [] },
          ],
        });
      case "hyperstor.v1.ClusterService/ListVolumes":
        // Pool 0: every pool the person reaches (hyperstor I401).
        expect(req.postDataJSON()).toEqual({ pool: 0 });
        return json({
          volumes: [
            { id: "a1", name: "web-root", pool: 3, size: String(20 * 2 ** 30), tenant: TENANT, project: 71 },
            { id: "a2", name: "scratch", pool: 3, size: String(10 * 2 ** 30), tenant: TENANT, project: 0 },
            { id: "b1", name: "not-ours", pool: 3, size: "1", tenant: 9, project: 90 },
          ],
        });
      case "hyperstor.v1.S3Service/ListS3Buckets":
        return json({ buckets: [{ name: "assets", tenant: TENANT, project: 71, counted: true, objects: "4", bytes: "2048" }] });
      case "hyperstor.v1.S3Service/ListS3Keys":
        return json({ keys: [{ id: "HSAAAAAAAAAAAAAAAAAA", tenant: TENANT, description: "ci", project: 71 }] });
      default:
        return route.fulfill({ status: 404, contentType: "application/json", body: JSON.stringify({ error: "not_relayed", message: call }) });
    }
  });
  await page.goto("/tenants/default");
  const row = page.getByTestId("tenant-clusters").getByRole("row").filter({ hasText: "store-north" });
  await row.getByRole("button", { name: "Open storage" }).click();
  const view = page.getByTestId("storage-view");
  await expect(view).toBeVisible();
  await expect(view.getByTestId("storage-volumes").getByText("web-root")).toBeVisible();
  await expect(view.getByTestId("storage-buckets").getByText("assets")).toBeVisible();
  await expect(view.getByTestId("storage-keys").getByText("HSAAAAAAAAAAAAAAAAAA")).toBeVisible();
  const usage = view.getByTestId("storage-usage");
  // The project without a glidex project here goes by the cluster's name.
  await expect(usage.getByRole("row").filter({ hasText: "web" }).getByText("20.0 GiB")).toBeVisible();
  await expect(usage.getByRole("row").filter({ hasText: "default" }).getByText("10.0 GiB")).toBeVisible();
  expect(relayed).toEqual(
    expect.arrayContaining([
      "hyperstor.v1.TenantService/ListTenants",
      "hyperstor.v1.S3Service/ListS3Buckets",
      "hyperstor.v1.S3Service/ListS3Keys",
      "hyperstor.v1.ClusterService/ListVolumes",
    ]),
  );
  // Nothing went anywhere but this server: no call to the cluster itself.
  const origin = new URL(page.url()).origin;
  expect(sent.filter((u) => !u.startsWith(origin) && !u.startsWith("data:"))).toEqual([]);
});

test("the storage view shows the cluster's refusal", async ({ page }) => {
  await rpcRoute(page, "glidex.tenancy.v1.TenantService/ListTenantClusters", () => ({
    message: {
      clusters: [
        {
          cluster_id: CLUSTER,
          name: "store-north",
          api: "https://store.example:8443",
          report: { reported_at: new Date().toISOString(), status: "ADMISSION_STATUS_ADMITTED", storage: { volumes: "1" } },
        },
      ],
    },
  }));
  await page.route(`**/api/tenants/*/clusters/${CLUSTER}/storage/**`, (route) =>
    route.fulfill({ status: 502, contentType: "application/json", body: JSON.stringify({ error: "cluster_unreachable", message: "cluster store-north didn't answer" }) }),
  );
  await page.goto("/tenants/default");
  await page.getByTestId("tenant-clusters").getByRole("button", { name: "Open storage" }).click();
  await expect(page.getByText("cluster store-north didn't answer")).toBeVisible();
});
