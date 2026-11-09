// The Cluster page's Device pools card (spec/22-host-topology.md H20, H21, H24):
// defined and discovered pools from mocked answers, adopting a discovered
// pool, and the request each action sends. Any host will do.
import { expect, rpcRoute, test, waitForRpc } from "../fixtures";

const CLUSTER_SVC = "glidex.cluster.v1.ClusterService";
const NODE_SVC = "glidex.cluster.v1.NodeService";

const t4 = { name: "t4", class: "gpu", match: { vendor_id: "0x10de", device_id: "0x1eb8" }, per_device: { vcpus: 4, memory_mib: 8192 } };
const grant = { pool: "t4", projects: ["lab"] };

test.beforeEach(async ({ page }) => {
  await rpcRoute(page, `${CLUSTER_SVC}/GetPciSettings`, () => ({
    message: { source: "PCI_SETTINGS_SOURCE_CLUSTER", settings: { pools: [t4], allow: [grant], updated_by: "api:admin" } },
  }));
  await rpcRoute(page, `${NODE_SVC}/ListPciPools`, () => ({
    message: {
      pools: [
        {
          name: "t4",
          class: "gpu",
          free: 1,
          per_device_vcpus: 4,
          per_device_memory_mib: 8192,
          nodes: [{ node: "n1", node_name: "gpu-01", free: 1, devices: [{ address: "0000:17:00.0", ready: true }] }],
        },
        {
          name: "amd-7551",
          class: "gpu",
          free: 2,
          discovered: true,
          nodes: [
            {
              node: "n2",
              node_name: "gpu-dev2",
              free: 2,
              devices: [
                { address: "0000:03:00.0", ready: true },
                { address: "0000:43:00.0", ready: true },
              ],
            },
          ],
        },
        {
          name: "15b3-1021-vf",
          class: "nic",
          free: 1,
          discovered: true,
          nodes: [{ node: "n1", node_name: "gpu-01", free: 1, devices: [{ address: "0000:41:00.2", ready: true }] }],
        },
      ],
    },
  }));
});

test("the card lists defined and discovered pools with their grants", async ({ page }) => {
  await page.goto("/cluster");
  await expect(page.getByRole("heading", { name: "Device pools" })).toBeVisible();
  const t4Row = page.getByRole("row").filter({ hasText: "t4" });
  await expect(t4Row).toContainText("1 of 1 free");
  await expect(t4Row).toContainText("4 vCPUs, 8192 MiB");
  await expect(t4Row).toContainText("project lab");
  const amd = page.getByRole("row").filter({ hasText: "amd-7551" });
  await expect(amd).toContainText("discovered");
  await expect(amd).toContainText("2 of 2 free");
  await expect(amd).toContainText("host roles only");
});

test("adopting a discovered pool adds a pool matching its model and keeps the rest", async ({ page }) => {
  let sent: any = null;
  await rpcRoute(page, `${CLUSTER_SVC}/PutPciSettings`, (req) => {
    sent = req;
    return { message: { source: "PCI_SETTINGS_SOURCE_CLUSTER", settings: req } };
  });
  await page.goto("/cluster");
  await page.getByRole("row").filter({ hasText: "amd-7551" }).getByRole("button", { name: "Adopt…" }).click();
  const dialog = page.getByRole("dialog", { name: "Adopt amd-7551" });
  await dialog.getByLabel("Name").fill("r9700");
  const put = waitForRpc(page, `${CLUSTER_SVC}/PutPciSettings`);
  await dialog.getByRole("button", { name: "Adopt" }).click();
  await put;
  await expect(dialog).toBeHidden();
  expect(sent.pools).toEqual([t4, { name: "r9700", class: "gpu", match: { vendor_id: "0x1002", device_id: "0x7551" } }]);
  expect(sent.allow).toEqual([grant]);
});

test("adopting a discovered pool of VFs matches them by their card", async ({ page }) => {
  let sent: any = null;
  await rpcRoute(page, `${CLUSTER_SVC}/PutPciSettings`, (req) => {
    sent = req;
    return { message: { source: "PCI_SETTINGS_SOURCE_CLUSTER", settings: req } };
  });
  await page.goto("/cluster");
  await page.getByRole("row").filter({ hasText: "15b3-1021-vf" }).getByRole("button", { name: "Adopt…" }).click();
  const dialog = page.getByRole("dialog", { name: "Adopt 15b3-1021-vf" });
  await dialog.getByLabel("Name").fill("cx7-vfs");
  const put = waitForRpc(page, `${CLUSTER_SVC}/PutPciSettings`);
  await dialog.getByRole("button", { name: "Adopt" }).click();
  await put;
  expect(sent.pools[1]).toEqual({ name: "cx7-vfs", class: "nic", match: { vf_of: { vendor_id: "0x15b3", device_id: "0x1021" } } });
});

test.describe("the grants dialog", () => {
  test.beforeEach(async ({ page }) => {
    await rpcRoute(page, "glidex.access.v1.ProjectService/ListProjects", () => ({
      message: { projects: [{ id: "p1", name: "lab" }, { id: "p2", name: "ml" }] },
    }));
    await rpcRoute(page, "glidex.tenancy.v1.TenantService/ListTenants", () => ({
      message: { tenants: [{ id: "t1", slug: "acme", display_name: "Acme" }] },
    }));
  });

  test("adds projects and tenants to a pool's grant", async ({ page }) => {
    let sent: any = null;
    await rpcRoute(page, `${CLUSTER_SVC}/PutPciSettings`, (req) => {
      sent = req;
      return { message: { source: "PCI_SETTINGS_SOURCE_CLUSTER", settings: req } };
    });
    await page.goto("/cluster");
    await page.getByRole("row").filter({ hasText: "t4" }).getByRole("button", { name: "Grants…" }).click();
    const dialog = page.getByRole("dialog", { name: "Grants for t4" });
    // What the pool is granted to is ticked already.
    await expect(dialog.getByRole("checkbox", { name: "lab", exact: true })).toBeChecked();
    await dialog.getByRole("checkbox", { name: "ml", exact: true }).check();
    await dialog.getByRole("checkbox", { name: "acme", exact: true }).check();
    const put = waitForRpc(page, `${CLUSTER_SVC}/PutPciSettings`);
    await dialog.getByRole("button", { name: "Save" }).click();
    await put;
    await expect(dialog).toBeHidden();
    expect(sent.pools).toEqual([t4]);
    expect(sent.allow).toEqual([{ pool: "t4", projects: ["lab", "ml"], tenants: ["acme"] }]);
  });

  test("opening a discovered pool to every tenant adopts it in the same write", async ({ page }) => {
    let sent: any = null;
    await rpcRoute(page, `${CLUSTER_SVC}/PutPciSettings`, (req) => {
      sent = req;
      return { message: { source: "PCI_SETTINGS_SOURCE_CLUSTER", settings: req } };
    });
    await page.goto("/cluster");
    await page.getByRole("row").filter({ hasText: "amd-7551" }).getByRole("button", { name: "Grants…" }).click();
    const dialog = page.getByRole("dialog", { name: "Grants for amd-7551" });
    await dialog.getByRole("checkbox", { name: /Every tenant/ }).check();
    // "*" goes alone: the tenant list can't be ticked with it.
    await expect(dialog.getByRole("checkbox", { name: "acme", exact: true })).toBeDisabled();
    const put = waitForRpc(page, `${CLUSTER_SVC}/PutPciSettings`);
    await dialog.getByRole("button", { name: "Save" }).click();
    await put;
    expect(sent.pools).toEqual([t4, { name: "amd-7551", class: "gpu", match: { vendor_id: "0x1002", device_id: "0x7551" } }]);
    expect(sent.allow).toEqual([grant, { pool: "amd-7551", tenants: ["*"] }]);
  });
});

test("removing a pool takes its grants with it", async ({ page }) => {
  const put = waitForRpc(page, `${CLUSTER_SVC}/PutPciSettings`);
  await rpcRoute(page, `${CLUSTER_SVC}/PutPciSettings`, () => ({ message: { source: "PCI_SETTINGS_SOURCE_CLUSTER", settings: {} } }));
  await page.goto("/cluster");
  await page.getByRole("row").filter({ hasText: "t4" }).getByRole("button", { name: "Remove…" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Remove" }).click();
  const req = await put;
  expect(req.pools ?? []).toEqual([]);
  expect(req.allow ?? []).toEqual([]);
});
