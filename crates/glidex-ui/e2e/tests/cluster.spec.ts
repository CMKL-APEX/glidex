// Cluster operations in the UI (spec/clustering-ui.md §11): a mocked
// two-node cluster in front of the scratch (standalone) control plane. Each
// dialog's request is checked, tokens never reach storage or the URL, and
// the client rides out a leader change and says when there is no quorum.
import type { Page, Route } from "@playwright/test";
import { expect, test } from "../fixtures";

// The mocks refuse things on purpose (the stream, a 409, a 503): the
// browser logs each as a failed load.
test.use({ ignoredConsoleErrors: /Failed to load resource/ });

/** No live stream: an HTTP error closes an EventSource for good, so the pages poll. */
const noStream = (r: Route) => r.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ error: "too_many_watchers", message: "poll" }) });

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

type Seen = { method: string; path: string; body: unknown }[];

/** Serve the cluster endpoints from fixtures; everything else goes to the real control plane. */
async function mockCluster(page: Page): Promise<Seen> {
  const seen: Seen = [];
  const record = (r: Route) => {
    const req = r.request();
    let body: unknown = null;
    try {
      body = req.postDataJSON();
    } catch {
      body = req.postData();
    }
    seen.push({ method: req.method(), path: new URL(req.url()).pathname.replace(/^\/api/, ""), body });
  };
  const json = (r: Route, v: unknown, status = 200) => r.fulfill({ status, contentType: "application/json", body: JSON.stringify(v) });
  // No live stream: the pages poll, so the fixtures are what they see.
  await page.route("**/api/watch*", noStream);
  await page.route("**/api/cluster/status*", (r) =>
    json(r, {
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
      nodes: [
        { id: S1, name: "s1", role: "NODE_ROLE_SERVER", phase: "NODE_PHASE_ACTIVE", advertise: "192.0.2.11:8842", ready: "CONDITION_STATUS_TRUE", cert: { current_ca: true, not_after: CERT_UNTIL } },
        { id: A1, name: "a1", role: "NODE_ROLE_AGENT", phase: "NODE_PHASE_ACTIVE", advertise: "192.0.2.12:8842", ready: "CONDITION_STATUS_TRUE", cert: { current_ca: true, not_after: CERT_UNTIL } },
      ],
    }),
  );
  // a1 left once and joined again: its old record keeps the name.
  await page.route("**/api/nodes", (r) => json(r, { nodes: [node(S1, "s1", "server"), node(A1, "a1", "agent"), node(GONE, "a1", "agent", "NODE_PHASE_REMOVED")] }));
  await page.route("**/api/departures", (r) => json(r, {}));
  await page.route("**/api/cluster/ovn", (r) => json(r, {}));
  await page.route("**/api/imports", (r) => json(r, { plans: [PLAN] }));
  await page.route("**/api/imports/p1", (r) =>
    json(r, {
      plan: {
        ...PLAN,
        sources: {
          projects: [{ id: "src1", name: "lab" }],
          networks: [{ id: "n1", name: "lan", mode: "nat" }],
          vms: [{ id: "v1", name: "web", project: "src1" }],
        },
      },
    }),
  );
  await page.route("**/api/imports/p1/approve", (r) => {
    record(r);
    const dry = (r.request().postDataJSON() as { dry_run?: boolean }).dry_run;
    // No problems left: the list is left out (JSON drops the undefined).
    return json(r, { plan: { ...PLAN, problems: undefined, state: dry ? "IMPORT_STATE_PENDING" : "IMPORT_STATE_COMMITTED" } });
  });
  await page.route("**/api/cluster/join-tokens", (r) => {
    record(r);
    return json(
      r,
      { token: "gxj_secret-token-value_ab", role: "NODE_ROLE_AGENT", ttl_secs: "3600", servers: ["192.0.2.11:8842"], ca_fingerprint: "ab".repeat(32) },
      201,
    );
  });
  let removals = 0;
  await page.route(`**/api/nodes/${A1}/remove`, (r) => {
    record(r);
    removals += 1;
    return removals === 1
      ? json(r, { error: "conflict", message: "a1 is the last gateway of gx-edge: confirm with --force" }, 409)
      : json(r, { node: "a1", phase: "NODE_PHASE_REMOVED" });
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
  expect(seen.filter((s) => s.path.endsWith("/remove")).map((s) => s.body)).toEqual([{ force: false }, { force: true }]);
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
  expect(seen.find((s) => s.path === "/cluster/join-tokens")?.body).toEqual({ role: "NODE_ROLE_AGENT", ttl_secs: 3600, allow_import: false });
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
  const bodies = seen.filter((s) => s.path === "/imports/p1/approve").map((s) => s.body as Record<string, unknown>);
  expect(bodies[0]).toMatchObject({ projects: { src1: "new:lab" }, dry_run: true });
  expect(bodies[1]).toMatchObject({ projects: { src1: "new:lab" } });
  expect(bodies[1].dry_run).toBeUndefined();
});

test.describe("without a leader", () => {
  test("a leader change is retried and a lost quorum shows the banner", async ({ page }) => {
    await page.route("**/api/watch*", noStream);
    // One election: the next try reaches the new leader.
    let tries = 0;
    await page.route("**/api/vms*", (r) => {
      tries += 1;
      return tries === 1
        ? r.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ error: "leader_changed", message: "retry" }) })
        : r.fallback();
    });
    await page.goto("/");
    await expect(page.getByRole("heading", { name: "Virtual Machines" })).toBeVisible();
    await expect.poll(() => tries).toBeGreaterThanOrEqual(2);
    await expect(page.getByRole("alert")).toHaveCount(0);

    // No quorum: reads come from this server's copy, and the banner says so,
    // whatever host-local calls still answer meanwhile.
    await page.unroute("**/api/vms*");
    await page.route("**/api/credentials*", (r) =>
      r.request().headers()["x-glidex-consistency"] === "local"
        ? r.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ credentials: [] }) })
        : r.fulfill({ status: 503, contentType: "application/json", body: JSON.stringify({ error: "cluster_unavailable", message: "no quorum" }) }),
    );
    await page.getByRole("link", { name: "Credentials", exact: true }).click();
    await expect(page.getByRole("alert")).toContainText("no leader");
    await expect(page.getByRole("alert")).toContainText("out of date");
  });
});

test("OVN settings stay reachable when OVN's state can't be read", async ({ page }) => {
  await mockCluster(page);
  const json = (r: Route, v: unknown, status = 200) => r.fulfill({ status, contentType: "application/json", body: JSON.stringify(v) });
  await page.route("**/api/cluster/settings/ovn", (r) =>
    json(r, { enabled: true, clustered: true, source: "OVN_SETTINGS_SOURCE_CLUSTER", settings: { enabled: true } }),
  );
  await page.route("**/api/cluster/ovn", (r) => json(r, { error: "unavailable", message: "northbound database not reachable" }, 503));
  await page.route("**/api/cluster/ovn/tunnels", (r) => json(r, { enabled: true, nodes: [] }));
  await page.route("**/api/cluster/settings/ovn/edge-defaults", (r) =>
    json(r, { edge: { physnet: "uplink" }, complete: false }),
  );
  await page.goto("/cluster");
  await expect(page.getByText(/OVN's state couldn't be read/)).toBeVisible();
  await page.getByRole("button", { name: "OVN settings…" }).click();
  await expect(page.getByRole("heading", { name: "OVN settings" })).toBeVisible();
});

test("a node whose OVN is older than the cluster's northd is called out", async ({ page }) => {
  await mockCluster(page);
  const json = (r: Route, v: unknown, status = 200) => r.fulfill({ status, contentType: "application/json", body: JSON.stringify(v) });
  const why = "its ovn-controller 24.03.8 is older than the cluster's ovn-northd 26.03.0: re-run glidex-install on a1 to upgrade OVN";
  await page.route("**/api/cluster/settings/ovn", (r) =>
    json(r, {
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
  await page.route("**/api/cluster/ovn", (r) => json(r, { error: "unavailable", message: "northbound database not reachable" }, 503));
  await page.route("**/api/cluster/ovn/tunnels", (r) => json(r, { enabled: true, nodes: [] }));
  await page.goto("/cluster");
  const alert = page.getByRole("alert").filter({ hasText: "OVN is out of date" });
  await expect(alert).toContainText("OVN is out of date on a1");
  await expect(alert).toContainText(why);
  await expect(alert).not.toContainText("s1");
});

test("through an agent's UI, the agent is this node and the Raft card names the server", async ({ page }) => {
  await mockCluster(page);
  // What a server answers for a request the agent's UI socket relayed.
  await page.route("**/api/cluster/status*", (r) =>
    r.fulfill({
      status: 200,
      contentType: "application/json",
      body: JSON.stringify({
        clustered: true,
        cluster_id: CLUSTER,
        node_id: A1,
        name: "a1",
        role: "NODE_ROLE_AGENT",
        served_by: { node_id: S1, name: "s1" },
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
      }),
    }),
  );
  await page.goto("/cluster");
  await expect(page.getByRole("row", { name: /a1/ })).toContainText("(this node)");
  await expect(page.getByRole("row", { name: /s1/ })).not.toContainText("(this node)");
  await expect(page.getByText("s1 is Leader, term 3")).toBeVisible();
});
