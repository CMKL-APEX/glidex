// A project's VPC routers in the UI (spec/clustering.md §11.2a): the
// Routers card on the project page and the router choice in the network
// form. The scratch control plane is standalone, so OVN, the routers and
// two nodes are mocked; the project is real.
import type { Page } from "@playwright/test";
import { api, del, expect, field, RpcCode, rpcFetch, rpcRoute, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /Failed to load resource/ });

const PROJECT = "e2e-routers";

async function cleanup() {
  const p = ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((x) => x.name === PROJECT);
  if (p) await del(`/projects/${p.id}`);
}
test.beforeAll(cleanup);
test.afterAll(cleanup);

const ok = (message: unknown) => ({ message });

/** A server node as glidex.cluster.v1 sends it (proto3 JSON). */
function node(id: string, name: string, phase = "NODE_PHASE_ACTIVE") {
  return {
    meta: { id, name, created_at: "2026-09-21T14:13:20Z", generation: "1", resource_version: "1" },
    spec: { name, role: "NODE_ROLE_SERVER" },
    status: {
      phase,
      ready: "CONDITION_STATUS_TRUE",
      versions: { glidex: "0.1.0" },
      capacity: { cpus: 8, memory_mib: "16384" },
      allocatable: { cpus: 7, memory_mib: "14336" },
      features: {},
    },
  };
}

type Seen = { method: string; path: string; body: any }[];

/** r1's id. */
const R1 = "a1b2c3d4";
/** A project NAT network spanning the nodes, as glidex.network.v1 sends it. */
const CLUSTER_NAT = {
  scope: "NETWORK_SCOPE_CLUSTER",
  mode: "NETWORK_MODE_NAT",
  bridge: "br-int",
  port_type: "PORT_TYPE_TAP",
  dns: true,
  owns_bridge: false,
  all_projects: false,
  created_at: "2026-09-21T12:00:00Z",
  phase: "NETWORK_PHASE_READY",
};

/** OVN on with a router pool, two nodes, and a project's routers kept in memory. */
async function mockOvn(page: Page, project: string, pool = true): Promise<Seen> {
  const seen: Seen = [];
  // glidex.network.v1 shapes: fields at their default left out, RFC 3339 times.
  const routers: any[] = [{ id: R1, project, spec: { name: "r1", external: true }, status: { external_ip: "192.0.2.240", snat_ct_zone: 60001 }, created_at: "2026-09-21T12:00:00Z" }];
  // No live stream: the pages poll, so the mocks are what they see.
  await rpcRoute(page, "glidex.watch.v1.WatchService/Watch", () => ({
    error: { code: RpcCode.UNAVAILABLE, reason: "too_many_watchers", message: "poll", httpStatus: 503 },
  }));
  await rpcRoute(page, "glidex.cluster.v1.NodeService/ListNodes", () =>
    ok({
      nodes: [
        node("11111111-1111-4111-8111-111111111111", "s1"),
        node("22222222-2222-4222-8222-222222222222", "s2"),
        // s2 before it left and joined again: a tombstone with the same name.
        node("33333333-3333-4333-8333-333333333333", "s2", "NODE_PHASE_REMOVED"),
      ],
    }),
  );
  await rpcRoute(page, "glidex.cluster.v1.ClusterService/GetOvnSettings", () => ok({ enabled: true, edge: true, vpc_pool: pool }));
  await rpcRoute(page, "glidex.cluster.v1.ClusterService/GetOvnOverview", () =>
    ok({ enabled: true, gateways: [{ router: "gxr-a1b2c3d4", active: { chassis: "node:1", node: "1", node_name: "s1" } }] }),
  );
  await rpcRoute(page, "glidex.network.v1.RouterService/ListRouters", (req) => (req.project === project ? ok({ routers }) : undefined));
  await rpcRoute(page, "glidex.network.v1.RouterService/CreateRouter", (req) => {
    if (req.project !== project) return undefined;
    const { project: _, ...spec } = req;
    seen.push({ method: "POST", path: "routers", body: spec });
    const made = { id: "e5f6a7b8", project, spec, status: spec.external ? { external_ip: "192.0.2.241" } : {}, created_at: "2026-09-21T12:00:01Z" };
    routers.push(made);
    return ok({ router: made });
  });
  await rpcRoute(page, "glidex.network.v1.RouterService/DeleteRouter", (req) => {
    if (req.project !== project) return undefined;
    // By id or name, as the server takes it; recorded by name.
    const key = req.router;
    const i = routers.findIndex((x) => x.id === key || x.spec.name === key);
    seen.push({ method: "DELETE", path: `routers/${routers[i]?.spec.name ?? key}`, body: null });
    routers.splice(i, 1);
    return ok({});
  });
  // The project's NAT networks, `app` on r1 and `web` on the edge (the
  // networks list is otherwise real).
  const nets: any[] = [
    { id: "0a000000-0000-4000-8000-000000000001", name: "app", ...CLUSTER_NAT, project, router: R1 },
    { id: "0a000000-0000-4000-8000-000000000002", name: "web", ...CLUSTER_NAT, project },
    { id: "0a000000-0000-4000-8000-000000000003", name: "lab", ...CLUSTER_NAT, mode: "NETWORK_MODE_ISOLATED", project },
  ];
  await rpcRoute(page, "glidex.network.v1.NetworkService/ListNetworks", async (_req, r) => {
    // The page may have moved on (to the Networking page) meanwhile.
    const real = await rpcFetch(r);
    if (real === null) return { error: { code: RpcCode.UNAVAILABLE, reason: "unavailable", message: "gone", httpStatus: 503 } };
    return ok({ networks: [...(real.networks ?? []), ...nets] });
  });
  await rpcRoute(page, "glidex.network.v1.NetworkService/SetNetworkRouter", (req) => {
    if (req.project !== project) return undefined;
    // The network and router by id or name; recorded by the network's name.
    const { project: _, network: key, ...body } = req;
    const n = nets.find((x) => x.id === key || x.name === key)!;
    seen.push({ method: "PUT", path: `networks/${n.name}/router`, body });
    if (body.router) n.router = routers.find((x) => x.id === body.router || x.spec.name === body.router)!.id;
    else delete n.router;
    return ok({ network: n });
  });
  await rpcRoute(page, "glidex.network.v1.NetworkService/CreateProjectNetwork", (req) => {
    if (req.project !== project) return undefined;
    seen.push({ method: "POST", path: "networks", body: req });
    return ok({ network: { id: "0a000000-0000-4000-8000-000000000004", name: "db", ...CLUSTER_NAT, project } });
  });
  return seen;
}

test("a project's routers: listed with their address and gateway, created and deleted", async ({ page }) => {
  const project = (await api("/projects", { method: "POST", body: { name: PROJECT } })).project;
  const seen = await mockOvn(page, project.id);
  await page.goto(`/projects/${project.id}`);

  const card = page.locator("table").filter({ hasText: "Outside address" });
  const r1 = card.getByRole("row", { name: /r1/ });
  await expect(r1).toContainText("192.0.2.240");
  await expect(r1).toContainText("on s1");
  await expect(r1).toContainText("app");
  // A router with networks can't go.
  await expect(r1.getByRole("button", { name: "Delete" })).toBeDisabled();
  // The network says which way it leaves.
  await expect(page.getByText(/leaves via router r1/)).toBeVisible();
  // The router quotas are shown with the others.
  await expect(page.getByText("External IPs")).toBeVisible();

  await page.getByRole("button", { name: "New router" }).click();
  // Gateway nodes: the nodes in the cluster, once each; not the one that left.
  const dialog = page.getByRole("dialog");
  await expect(dialog.getByRole("checkbox", { name: "s1" })).toHaveCount(1);
  await expect(dialog.getByRole("checkbox", { name: "s2" })).toHaveCount(1);
  await page.getByLabel("Name").fill("r2");
  await expect(page.getByLabel("Reaches outside", { exact: false })).toBeChecked();
  await page.getByRole("button", { name: "Create", exact: true }).click();
  const r2 = card.getByRole("row", { name: /r2/ });
  await expect(r2).toContainText("192.0.2.241");
  const created = seen.find((s) => s.path === "routers")!.body;
  expect(created.name).toBe("r2");
  expect(created.external).toBe(true);

  await r2.getByRole("button", { name: "Delete" }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Delete" }).click();
  await expect(card.getByRole("row", { name: /r2/ })).toHaveCount(0);
  expect(seen.some((s) => s.method === "DELETE" && s.path === "routers/r2")).toBe(true);
});

test("without a pool a router can only join its networks", async ({ page }) => {
  const project = (await api("/projects", { method: "POST", body: { name: `${PROJECT}-np` } })).project;
  try {
    const seen = await mockOvn(page, project.id, false);
    await page.goto(`/projects/${project.id}`);
    await page.getByRole("button", { name: "New router" }).click();
    const outside = page.getByLabel("Reaches outside", { exact: false });
    await expect(outside).not.toBeChecked();
    await expect(outside).toBeDisabled();
    await expect(page.getByText(/no pool of addresses for routers/)).toBeVisible();
    await page.getByLabel("Name").fill("inner");
    await page.getByRole("button", { name: "Create", exact: true }).click();
    await expect(page.getByRole("row", { name: /inner/ })).toContainText("internal only");
    expect(seen.find((s) => s.path === "routers")!.body.external).toBe(false);
  } finally {
    await del(`/projects/${project.id}`);
  }
});

test("a network made from a router's card goes on that router", async ({ page }) => {
  const project = (await api("/projects", { method: "POST", body: { name: `${PROJECT}-net` } })).project;
  try {
    const seen = await mockOvn(page, project.id);
    // The form opens where netd can make networks (none on the scratch host).
    await rpcRoute(page, "glidex.host.v1.HostNetworkService/GetOvsStatus", async (_req, r) => {
      const real = await rpcFetch(r);
      if (real === null) return { error: { code: RpcCode.UNAVAILABLE, reason: "unavailable", message: "gone", httpStatus: 503 } };
      return ok({ ...real, netd: { ...(real.netd ?? {}), available: true, access: "NETD_ACCESS_FULL" }, host: { ...(real.host ?? {}), ovs_running: true } });
    });
    await page.goto(`/projects/${project.id}`);
    await page.getByRole("link", { name: "+ network on r1" }).click();
    await expect(page).toHaveURL(/\/networking/);
    await expect(field(page, "Network for")).toHaveValue(project.id);
    await expect(field(page, "Leaves through")).toHaveValue(R1);
    // The shared edge is the other choice.
    await expect(field(page, "Leaves through").locator("option")).toHaveText([/shared edge/, /Router r1 \(192\.0\.2\.240\)/]);
    await page.getByPlaceholder("lab").fill("db");
    await page.getByRole("button", { name: "Create", exact: true }).click();
    await expect.poll(() => seen.find((s) => s.path === "networks")?.body).toMatchObject({ name: "db", mode: "NETWORK_MODE_NAT", scope: "NETWORK_SCOPE_CLUSTER", router: R1 });
  } finally {
    await del(`/projects/${project.id}`);
  }
});

test("existing networks attach to a router and detach back to the edge", async ({ page }) => {
  const project = (await api("/projects", { method: "POST", body: { name: `${PROJECT}-att` } })).project;
  try {
    const seen = await mockOvn(page, project.id);
    await page.goto(`/projects/${project.id}`);
    const r1 = page.locator("table").filter({ hasText: "Outside address" }).getByRole("row", { name: /r1/ });
    await expect(page.getByText(/web.*leaves via the shared edge/)).toBeVisible();

    await r1.getByRole("button", { name: "Attach network…" }).click();
    const dialog = page.getByRole("dialog", { name: "Attach networks to r1" });
    // Only what isn't on r1 yet, and where it is now.
    await expect(dialog.getByRole("checkbox")).toHaveCount(2);
    await expect(dialog).toContainText("now via the shared edge");
    await expect(dialog).toContainText("leave with its address 192.0.2.240");
    await dialog.getByRole("checkbox", { name: /web/ }).check();
    await dialog.getByRole("button", { name: "Attach" }).click();
    await expect(dialog).toHaveCount(0);
    expect(seen.find((s) => s.path === "networks/web/router")?.body).toEqual({ router: R1 });
    await expect(page.getByText(/web.*leaves via router r1/)).toBeVisible();
    // r1 now has both, so it still can't be deleted.
    await expect(r1).toContainText("web");

    // app goes back to the edge.
    await r1.getByText("app").locator("..").getByRole("button", { name: "Detach" }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Detach" }).click();
    await expect.poll(() => seen.find((s) => s.path === "networks/app/router")?.body).toEqual({});
    await expect(page.getByText(/app.*leaves via the shared edge/)).toBeVisible();
  } finally {
    await del(`/projects/${project.id}`);
  }
});

test("an isolated network attaches to a router and is isolated again when detached", async ({ page }) => {
  const project = (await api("/projects", { method: "POST", body: { name: `${PROJECT}-iso` } })).project;
  try {
    const seen = await mockOvn(page, project.id);
    await page.goto(`/projects/${project.id}`);
    const r1 = page.locator("table").filter({ hasText: "Outside address" }).getByRole("row", { name: /r1/ });
    await r1.getByRole("button", { name: "Attach network…" }).click();
    const dialog = page.getByRole("dialog", { name: "Attach networks to r1" });
    await expect(dialog.getByRole("checkbox", { name: /lab/ }).locator("..")).toContainText("isolated now");
    // Running VMs learn the new gateway later: said before it happens.
    await expect(dialog.getByText(/DHCP lease renews/)).toHaveCount(0);
    await dialog.getByRole("checkbox", { name: /lab/ }).check();
    await expect(dialog.getByText(/DHCP lease renews/)).toBeVisible();
    await dialog.getByRole("button", { name: "Attach" }).click();
    await expect(dialog).toHaveCount(0);
    expect(seen.find((s) => s.path === "networks/lab/router")?.body).toEqual({ router: R1 });
    await expect(page.getByText(/lab.*routed via router r1/)).toBeVisible();

    await r1.getByText("lab", { exact: true }).locator("..").getByRole("button", { name: "Detach" }).click();
    await expect(page.getByRole("dialog")).toContainText("isolated again");
    await page.getByRole("dialog").getByRole("button", { name: "Detach" }).click();
    await expect.poll(() => seen.filter((s) => s.path === "networks/lab/router").pop()?.body).toEqual({});
    await expect(page.getByText(/routed via router r1/)).toHaveCount(0);
  } finally {
    await del(`/projects/${project.id}`);
  }
});
