// Cluster operations in the UI (spec/16-clustering-ui.md §11): a mocked
// two-node cluster in front of the scratch (standalone) control plane. Each
// dialog's request is checked, tokens never reach storage or the URL, and
// the client rides out a leader change and says when there is no quorum.
import type { Page } from "@playwright/test";
import { expect, RpcCode, rpcRoute, test, type RpcReply } from "../fixtures";

// The mocks refuse things on purpose (the stream, a 409, a 503): the
// browser may log each as a failed load.
test.use({ ignoredConsoleErrors: /Failed to load resource/ });

const CLUSTER_SVC = "glidex.cluster.v1.ClusterService";
const NODE_SVC = "glidex.cluster.v1.NodeService";
const IMPORT_SVC = "glidex.cluster.v1.ImportService";

/** No live stream: the Watch RPC fails, so the pages poll. */
const noStream = (page: Page) =>
  rpcRoute(page, "glidex.watch.v1.WatchService/Watch", () => ({
    error: { code: RpcCode.UNAVAILABLE, reason: "too_many_watchers", message: "poll", httpStatus: 503 },
  }));

const ok = (message: unknown): RpcReply => ({ message });

const CLUSTER = "c0ffee00-0000-4000-8000-000000000001";
const S1 = "11111111-1111-4111-8111-111111111111";
const A1 = "22222222-2222-4222-8222-222222222222";
const GONE = "44444444-4444-4444-8444-444444444444";

// The fixtures are glidex.cluster.v1 messages as proto3 JSON: enums by full
// name, RFC 3339 times, 64-bit numbers as strings, defaults left out.
const JOINED = "2026-09-21T14:13:20Z";
const CERT_UNTIL = "2033-05-18T03:33:20Z";

function node(id: string, name: string, role: "server" | "agent", phase = "NODE_PHASE_ACTIVE") {
  return {
    meta: { id, name, created_at: JOINED, generation: "1", resource_version: "1" },
    spec: { name, role: role === "server" ? "NODE_ROLE_SERVER" : "NODE_ROLE_AGENT" },
    status: {
      phase,
      ready: "CONDITION_STATUS_TRUE",
      advertise: role === "server" ? "192.0.2.11:8842" : "192.0.2.12:8842",
      versions: { glidex: "0.1.0" },
      capacity: { cpus: 8, memory_mib: "16384" },
      allocatable: { cpus: 7, memory_mib: "14336" },
      features: { kvm: true, hypervisors: ["cloudhypervisor"] },
    },
  };
}

const PLAN = {
  plan: "p1",
  state: "IMPORT_STATE_PENDING",
  node: "lab-host",
  node_id: "33333333-3333-4333-8333-333333333333",
  advertise: "192.0.2.20:8842",
  summary: { vms: 2, disks: 2, images: 1, networks: 1, credentials: 1, projects: 1 },
  problems: ["project 'lab' exists with another id: map it with --project src1=<existing> or src1=new:<name>"],
  created_by: "api:someone",
  created_at: JOINED,
  expires_at: "2100-01-01T00:00:00Z",
};

type Seen = { procedure: string; body: any }[];

const STATUS = {
  clustered: true,
  cluster_id: CLUSTER,
  node_id: S1,
  name: "s1",
  role: "NODE_ROLE_SERVER",
  feature_level: 1,
  ca_fingerprint: "ab".repeat(32),
  ca: { signing: "ab".repeat(32), not_after: CERT_UNTIL },
  raft: {
    id: "1",
    state: "RAFT_STATE_LEADER",
    term: "3",
    leader: { raft_id: "1", name: "s1", address: "192.0.2.11:8842" },
    voters: [{ raft_id: "1", name: "s1", address: "192.0.2.11:8842" }],
  },
};

/** Serve the cluster RPCs from fixtures; everything else goes to the real control plane. */
async function mockCluster(page: Page): Promise<Seen> {
  const seen: Seen = [];
  const record = (procedure: string, body: unknown) => seen.push({ procedure, body });
  // No live stream: the pages poll, so the fixtures are what they see.
  await noStream(page);
  await rpcRoute(page, `${CLUSTER_SVC}/GetClusterStatus`, () =>
    ok({
      ...STATUS,
      nodes: [
        { id: S1, name: "s1", role: "NODE_ROLE_SERVER", phase: "NODE_PHASE_ACTIVE", advertise: "192.0.2.11:8842", ready: "CONDITION_STATUS_TRUE", cert: { current_ca: true, not_after: CERT_UNTIL } },
        { id: A1, name: "a1", role: "NODE_ROLE_AGENT", phase: "NODE_PHASE_ACTIVE", advertise: "192.0.2.12:8842", ready: "CONDITION_STATUS_TRUE", cert: { current_ca: true, not_after: CERT_UNTIL } },
      ],
    }),
  );
  // a1 left once and joined again: its old record keeps the name.
  await rpcRoute(page, `${NODE_SVC}/ListNodes`, () => ok({ nodes: [node(S1, "s1", "server"), node(A1, "a1", "agent"), node(GONE, "a1", "agent", "NODE_PHASE_REMOVED")] }));
  await rpcRoute(page, `${NODE_SVC}/ListDepartures`, () => ok({}));
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnOverview`, () => ok({}));
  await rpcRoute(page, `${IMPORT_SVC}/ListImports`, () => ok({ plans: [PLAN] }));
  await rpcRoute(page, `${IMPORT_SVC}/GetImport`, (req) =>
    req.plan === "p1"
      ? ok({
          plan: {
            ...PLAN,
            sources: {
              projects: [{ id: "src1", name: "lab" }],
              networks: [{ id: "n1", name: "lan", mode: "nat" }],
              vms: [{ id: "v1", name: "web", project: "src1" }],
            },
          },
        })
      : undefined,
  );
  await rpcRoute(page, `${IMPORT_SVC}/ApproveImport`, (req) => {
    record("ApproveImport", req);
    // No problems left: the list is left out (JSON drops the undefined).
    return ok({ plan: { ...PLAN, problems: undefined, state: req.dry_run ? "IMPORT_STATE_PENDING" : "IMPORT_STATE_COMMITTED" } });
  });
  await rpcRoute(page, `${CLUSTER_SVC}/CreateJoinToken`, (req) => {
    record("CreateJoinToken", req);
    return ok({ token: "gxj_secret-token-value_ab", role: "NODE_ROLE_AGENT", ttl_secs: "3600", servers: ["192.0.2.11:8842"], ca_fingerprint: "ab".repeat(32) });
  });
  let removals = 0;
  await rpcRoute(page, `${NODE_SVC}/RemoveNode`, (req) => {
    record("RemoveNode", req);
    removals += 1;
    return removals === 1
      ? { error: { code: RpcCode.ABORTED, reason: "conflict", message: "a1 is the last gateway of gx-edge: confirm with --force", httpStatus: 409 } }
      : ok({ node: "a1", phase: "NODE_PHASE_REMOVED" });
  });
  return seen;
}

test("the Cluster page lists the nodes and a pending import", async ({ page }) => {
  await mockCluster(page);
  await page.goto("/cluster");
  await expect(page.getByRole("heading", { name: "Cluster" })).toBeVisible();
  await expect(page.getByRole("link", { name: "s1" })).toBeVisible();
  await expect(page.getByRole("link", { name: "a1", exact: true })).toBeVisible();
  // The record a1 left behind reads apart from it.
  await page.getByText("1 departed, removed or forgotten").click();
  await expect(page.getByRole("link", { name: "a1 · 44444444", exact: true })).toBeVisible();
  await expect(page.getByLabel("Operations on a1 · 44444444")).toBeVisible();
  await expect(page.getByRole("link", { name: "lab-host" })).toBeVisible();
  await expect(page.getByText("Leader, term 3")).toBeVisible();
});

test("removing a node asks for its name and offers --force only after the server asks", async ({ page }) => {
  const seen = await mockCluster(page);
  await page.goto("/cluster");
  await page.getByLabel("Operations on a1", { exact: true }).selectOption({ label: "Remove…" });
  const dialog = page.getByRole("heading", { name: "Remove a1" }).locator("..").locator("..");
  const go = dialog.getByRole("button", { name: "Remove", exact: true });
  await expect(go).toBeDisabled();
  await dialog.getByLabel(/Type a1 to confirm/).fill("a1");
  await go.click();
  await expect(dialog.getByText(/last gateway/)).toBeVisible();
  await dialog.getByLabel(/Do it anyway/).check();
  await dialog.getByRole("button", { name: "Remove anyway" }).click();
  await expect(page.getByRole("heading", { name: "Remove a1" })).toHaveCount(0);
  // proto JSON leaves a false bool out.
  const removes = seen.filter((s) => s.procedure === "RemoveNode").map((s) => s.body);
  expect(removes).toHaveLength(2);
  expect(removes[0].force ?? false).toBe(false);
  expect(removes[1]).toMatchObject({ force: true });
  expect([A1, "a1"]).toContain(removes[0].id);
});

test("a join token is shown once, masked, and never stored", async ({ page }) => {
  const seen = await mockCluster(page);
  await page.goto("/cluster");
  await page.getByRole("button", { name: "+ Add node" }).click();
  await page.getByRole("button", { name: "Create token" }).click();
  const field = page.getByLabel("token");
  await expect(field).toHaveAttribute("type", "password");
  await expect(field).toHaveValue("gxj_secret-token-value_ab");
  // The commands read it from a file, never from the command line.
  await expect(page.getByText(/--token-file \.\/join-token/).first()).toBeVisible();
  await expect(page.getByText("gxj_secret-token-value_ab")).toHaveCount(0);
  const stored = await page.evaluate(() => JSON.stringify({ ...localStorage }) + JSON.stringify({ ...sessionStorage }));
  expect(stored).not.toContain("gxj_secret");
  expect(page.url()).not.toContain("gxj_secret");
  await page.getByRole("button", { name: "Done" }).click();
  await expect(page.getByLabel("token")).toHaveCount(0);
  // uint64 is a string in proto JSON, and a false bool is left out.
  expect(seen.find((s) => s.procedure === "CreateJoinToken")?.body).toEqual({ role: "NODE_ROLE_AGENT", ttl_secs: "3600" });
});

test("an import is checked with its names before it can be approved", async ({ page }) => {
  const seen = await mockCluster(page);
  await page.goto("/cluster/imports/p1");
  await expect(page.getByRole("heading", { name: "Import lab-host" })).toBeVisible();
  await expect(page.getByText(/exists with another id/)).toBeVisible();
  const approve = page.getByRole("button", { name: "Approve" });
  await expect(approve).toBeDisabled();
  await page.getByLabel("Where lab goes").selectOption({ label: "a new project named…" });
  await page.getByRole("button", { name: "Check" }).click();
  await expect(page.getByText("The check passed with these names.")).toBeVisible();
  await approve.click();
  await page.getByLabel(/Type lab-host to confirm/).fill("lab-host");
  await page.getByRole("button", { name: "Approve", exact: true }).last().click();
  await expect.poll(() => seen.filter((s) => s.procedure === "ApproveImport").length).toBe(2);
  const bodies = seen.filter((s) => s.procedure === "ApproveImport").map((s) => s.body as Record<string, unknown>);
  expect(bodies[0]).toMatchObject({ plan: "p1", projects: { src1: "new:lab" }, dry_run: true });
  expect(bodies[1]).toMatchObject({ plan: "p1", projects: { src1: "new:lab" } });
  expect(bodies[1].dry_run).toBeUndefined();
});

test.describe("without a leader", () => {
  test("a leader change is retried and a lost quorum shows the banner", async ({ page }) => {
    await noStream(page);
    // One election: the next try reaches the new leader.
    let tries = 0;
    await rpcRoute(page, "glidex.vm.v1.VmService/ListVms", () => {
      tries += 1;
      return tries === 1 ? { error: { code: RpcCode.UNAVAILABLE, reason: "leader_changed", message: "retry", httpStatus: 503 } } : undefined;
    });
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Virtual Machines" })).toBeVisible();
    await expect.poll(() => tries).toBeGreaterThanOrEqual(2);
    await expect(page.getByRole("alert")).toHaveCount(0);

    // No quorum: reads come from this server's copy, and the banner says so,
    // whatever host-local calls still answer meanwhile.
    await page.unroute("**/api/glidex.vm.v1.VmService/ListVms");
    await rpcRoute(page, "glidex.storage.v1.CredentialService/ListCredentials", (_req, r) =>
      r.request().headers()["x-glidex-consistency"] === "local"
        ? ok({ credentials: [] })
        : { error: { code: RpcCode.UNAVAILABLE, reason: "cluster_unavailable", message: "no quorum", httpStatus: 503 } },
    );
    await page.getByRole("link", { name: "Credentials", exact: true }).click();
    await expect(page.getByRole("alert")).toContainText("no leader");
    await expect(page.getByRole("alert")).toContainText("out of date");
  });
});

test("OVN settings stay reachable when OVN's state can't be read", async ({ page }) => {
  await mockCluster(page);
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnSettings`, () =>
    ok({ enabled: true, clustered: true, source: "OVN_SETTINGS_SOURCE_CLUSTER", settings: { enabled: true } }),
  );
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnOverview`, () => ({ error: { code: RpcCode.UNAVAILABLE, reason: "unavailable", message: "northbound database not reachable", httpStatus: 503 } }));
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnTunnels`, () => ok({ enabled: true, nodes: [] }));
  await rpcRoute(page, `${CLUSTER_SVC}/GetEdgeDefaults`, () =>
    ok({ edge: { physnet: "uplink" }, complete: false }),
  );
  await page.goto("/cluster");
  await expect(page.getByText(/OVN's state couldn't be read/)).toBeVisible();
  await page.getByRole("button", { name: "OVN settings…" }).click();
  await expect(page.getByRole("heading", { name: "OVN settings" })).toBeVisible();
});

test("a node whose OVN is older than the cluster's northd is called out", async ({ page }) => {
  await mockCluster(page);
  const why = "its ovn-controller 24.03.8 is older than the cluster's ovn-northd 26.03.0: re-run glidex-install on a1 to upgrade OVN";
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnSettings`, () =>
    ok({
      enabled: true,
      clustered: true,
      source: "OVN_SETTINGS_SOURCE_CLUSTER",
      settings: { enabled: true },
      nodes: [
        { id: S1, name: "s1", role: "NODE_ROLE_SERVER", ovn_version: "26.03.0", ovn_central: true },
        { id: A1, name: "a1", role: "NODE_ROLE_AGENT", ovn_version: "24.03.8", ovn_central: false, outdated: why },
      ],
    }),
  );
  // OVN's live state isn't what's checked here.
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnOverview`, () => ({ error: { code: RpcCode.UNAVAILABLE, reason: "unavailable", message: "northbound database not reachable", httpStatus: 503 } }));
  await rpcRoute(page, `${CLUSTER_SVC}/GetOvnTunnels`, () => ok({ enabled: true, nodes: [] }));
  await page.goto("/cluster");
  const alert = page.getByRole("alert").filter({ hasText: "OVN is out of date" });
  await expect(alert).toContainText("OVN is out of date on a1");
  await expect(alert).toContainText(why);
  await expect(alert).not.toContainText("s1");
});

test("through an agent's UI, the agent is this node and the Raft card names the server", async ({ page }) => {
  await mockCluster(page);
  // What a server answers for a request the agent's UI socket relayed.
  await rpcRoute(page, `${CLUSTER_SVC}/GetClusterStatus`, () =>
    ok({ ...STATUS, node_id: A1, name: "a1", role: "NODE_ROLE_AGENT", served_by: { node_id: S1, name: "s1" } }),
  );
  await page.goto("/cluster");
  await expect(page.getByRole("row", { name: /a1/ })).toContainText("(this node)");
  await expect(page.getByRole("row", { name: /s1/ })).not.toContainText("(this node)");
  await expect(page.getByText("s1 is Leader, term 3")).toBeVisible();
});
