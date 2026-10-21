// A standalone host is one node (`local`): the Cluster page says so and
// leads to its Host page, which a cluster node has too (spec/host-page.md
// W1). In a (mocked) cluster, another node's host-only calls name it (W2).
import { api, expect, rpcRoute, test } from "../fixtures";

test("a standalone host shows its node and its page", async ({ page }) => {
  await page.goto("/cluster");
  await expect(page.getByText("This host is standalone.")).toBeVisible();
  const card = page.getByTestId("this-host");
  await expect(card).toContainText(/CPUs/);
  await card.getByRole("link", { name: "Open this host's page" }).click();
  await expect(page).toHaveURL(/\/hosts\/[^/]+$/);
  await expect(page.getByRole("heading", { name: "Capacity" })).toBeVisible();
  // A standalone host isn't removed or detached from a cluster it isn't in.
  await expect(page.getByRole("button", { name: /^Remove/ })).toHaveCount(0);
  await expect(page.getByRole("button", { name: /^Detach/ })).toHaveCount(0);
});

test("the Host page has the host's networking, and the old address leads there", async ({ page }) => {
  const nodes = (await api<{ nodes?: any[] }>("/nodes")).nodes ?? [];
  const id = nodes[0]?.meta?.id;
  expect(id).toBeTruthy();
  await page.goto("/hosts");
  await expect(page).toHaveURL(new RegExp(`/hosts/${id}$`));
  await page.goto(`/cluster/nodes/${id}`);
  await expect(page).toHaveURL(new RegExp(`/hosts/${id}$`));
  await expect(page.getByRole("heading", { name: "Capacity" })).toBeVisible();
  const status = await api("/ovs/status");
  const section = page.getByRole("region", { name: "Host networking" });
  if (status?.netd?.available) {
    await expect(section.getByText("Open vSwitch:")).toBeVisible();
    if (status.netd.access === "NETD_ACCESS_FULL") await expect(section.getByRole("heading", { name: "glidex bridges" })).toBeVisible();
  } else {
    await expect(section.getByText(/glidex-netd is not reachable/)).toBeVisible();
  }
  // The Networking page keeps the networks and points here.
  await page.goto("/networking");
  await expect(page.getByTestId("host-networking-link").getByRole("link")).toHaveAttribute("href", `/hosts/${id}`);
  await expect(page.getByText("Open vSwitch:")).toHaveCount(0);
});

test.describe("another node", () => {
  test.use({ ignoredConsoleErrors: /Failed to load resource/ });

  const S1 = "11111111-1111-4111-8111-111111111111";
  const A1 = "22222222-2222-4222-8222-222222222222";
  const node = (id: string, name: string, role: string) => ({
    meta: { id, name, created_at: "2026-09-21T14:13:20Z", generation: "1", resource_version: "1" },
    spec: { name, role },
    status: {
      phase: "NODE_PHASE_ACTIVE",
      ready: "CONDITION_STATUS_TRUE",
      versions: { glidex: "0.1.0" },
      capacity: { cpus: 8, memory_mib: "16384" },
      allocatable: { cpus: 7, memory_mib: "14336" },
      features: { kvm: true, hypervisors: ["cloudhypervisor"], arch: "x86_64" },
    },
  });

  test("its Host page asks the server for that node's networking", async ({ page }) => {
    const targets: Record<string, string | undefined> = {};
    await rpcRoute(page, "glidex.watch.v1.WatchService/Watch", () => ({
      error: { code: 14, reason: "too_many_watchers", message: "poll", httpStatus: 503 },
    }));
    await rpcRoute(page, "glidex.cluster.v1.NodeService/ListNodes", () => ({
      message: { nodes: [node(S1, "s1", "NODE_ROLE_SERVER"), node(A1, "a1", "NODE_ROLE_AGENT")] },
    }));
    await rpcRoute(page, "glidex.cluster.v1.NodeService/GetNode", (req) => (req.id === A1 ? { message: { node: node(A1, "a1", "NODE_ROLE_AGENT") } } : undefined));
    await rpcRoute(page, "glidex.cluster.v1.ClusterService/GetClusterStatus", () => ({
      message: { clustered: true, cluster_id: "c", node_id: S1, name: "s1", role: "NODE_ROLE_SERVER" },
    }));
    const host = "glidex.host.v1.HostNetworkService";
    await rpcRoute(page, `${host}/GetOvsStatus`, (_req, route) => {
      targets.GetOvsStatus = route.request().headers()["x-glidex-target-node"];
      return {
        message: {
          netd: { available: true, access: "NETD_ACCESS_FULL" },
          host: { ovs_installed: true, ovs_running: true, ovs_version: "3.9.9-a1" },
        },
      };
    });
    await rpcRoute(page, `${host}/ListBridges`, (_req, route) => {
      targets.ListBridges = route.request().headers()["x-glidex-target-node"];
      return { message: { bridges: [{ spec: { name: "gxbr-a1", datapath: "DATAPATH_SYSTEM" }, live: { ports: [] } }] } };
    });
    await rpcRoute(page, `${host}/ListUplinks`, (_req, route) => {
      targets.ListUplinks = route.request().headers()["x-glidex-target-node"];
      return { message: { uplinks: [] } };
    });

    await page.goto(`/hosts/${A1}`);
    const section = page.getByRole("region", { name: "Host networking" });
    await expect(section.getByText("3.9.9-a1")).toBeVisible();
    await expect(section.getByText("gxbr-a1")).toBeVisible();
    await expect.poll(() => targets).toEqual({ GetOvsStatus: A1, ListBridges: A1, ListUplinks: A1 });
  });
});
