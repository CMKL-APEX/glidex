// Project networks from the Networking page (spec/07-web-ui.md, 11-security.md
// §6.2): the project page links to the shared "Add Network" form with the
// project chosen; project networks are NAT or isolated, never bridged.
// Needs glidex-netd and Open vSwitch; no VM is booted.
import { api, del, expect, field, test } from "../fixtures";

const PROJECT = "e2e-netproj";
const NETWORK = "e2e-iso";
const OWNER_NETWORK = "e2e-own";

/** Every network, from `GET /networks` (glidex.network.v1: `{networks}`). */
const listNetworks = async () => (await api<{ networks?: any[] }>("/networks")).networks ?? [];

async function cleanup() {
  // By id: a name is unique only within a tenant.
  for (const n of (await listNetworks()).filter((x) => [NETWORK, OWNER_NETWORK].includes(x.name))) await del(`/networks/${n.id}?wait=30`);
  for (const t of ((await api<{ tokens?: any[] }>("/tokens")).tokens ?? []).filter((t) => t.name === "e2e-net-owner")) await del(`/tokens/${t.id}`);
  const p = ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((x) => x.name === PROJECT);
  if (p) await del(`/projects/${p.id}`);
}

test.beforeAll(cleanup);
test.afterAll(cleanup);

test("a project network is created from the Networking page", async ({ page }) => {
  const status = await api("/ovs/status");
  test.skip(status?.netd?.access !== "NETD_ACCESS_FULL" || status?.host?.ovs_running !== true, "needs glidex-netd and a running Open vSwitch");

  const project = (await api("/projects", { method: "POST", body: { name: PROJECT } })).project;
  await page.goto(`/projects/${project.id}`);
  await page.getByRole("link", { name: "Create a project network" }).click();
  await expect(page).toHaveURL(/\/networking/);

  // The form opens for that project: no bridged mode, no bridge or VLAN.
  const form = page.locator("form").filter({ has: page.getByPlaceholder("lab") });
  await expect(field(page, "Network for")).toHaveValue(project.id);
  const mode = field(page, "Mode");
  await expect(mode.locator("option")).toHaveText([/NAT/, /Isolated/]);
  await expect(form.getByText("VLAN (optional)")).toHaveCount(0);
  // A host network offers all three modes again.
  await field(page, "Network for").selectOption("");
  await expect(mode.locator("option")).toHaveCount(3);
  await field(page, "Network for").selectOption(project.id);

  await page.getByPlaceholder("lab").fill(NETWORK);
  await mode.selectOption("NETWORK_MODE_ISOLATED");
  await page.getByRole("button", { name: "Create", exact: true }).click();

  const row = page.getByRole("row", { name: new RegExp(NETWORK) });
  await expect(row).toContainText(PROJECT, { timeout: 30_000 });
  await expect(row).toContainText("isolated");
  await expect(row).toContainText(/gxp-[0-9a-f]{8}/);
  const net = (await listNetworks()).find((x) => x.name === NETWORK && x.project === project.id);
  expect(net?.id).toMatch(/^[0-9a-f-]{36}$/);
  // By id, and by its name (unique in the tenant).
  for (const key of [net.id, NETWORK]) {
    const got = (await api(`/networks/${key}`)).network;
    expect([got?.id, got?.project, got?.mode, got?.port_type]).toEqual([net.id, project.id, "NETWORK_MODE_ISOLATED", "PORT_TYPE_TAP"]);
  }

  // The project page lists it; its owner deletes it from the Networking page.
  await page.goto(`/projects/${project.id}`);
  await expect(page.getByText(NETWORK).first()).toBeVisible();
  await page.goto("/networking");
  await page.getByRole("row", { name: new RegExp(NETWORK) }).getByRole("button", { name: "Delete" }).click();
  await expect(page.getByRole("row", { name: new RegExp(NETWORK) })).toHaveCount(0, { timeout: 30_000 });
});

// A project owner without host rights (a token narrowed to role.owner):
// the OVS status is refused (host.read), and the page still lists and
// creates the project's networks.
test.describe("as a project owner", () => {
  test.use({ ignoredConsoleErrors: /status of 403/ });

  test("creates a project network without host rights", async ({ browser }) => {
    const status = await api("/ovs/status");
    test.skip(status?.netd?.access !== "NETD_ACCESS_FULL" || status?.host?.ovs_running !== true, "needs glidex-netd and a running Open vSwitch");
    const project =
      ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((x) => x.name === PROJECT) ?? ((await api("/projects", { method: "POST", body: { name: PROJECT } })).project);
    const t = await api("/tokens", {
      method: "POST",
      body: { name: "e2e-net-owner", kind: "TOKEN_KIND_PERSONAL", expires_in_days: 1, roles: [{ role: "role.owner", project: project.id }] },
    });
    expect(t.token, JSON.stringify(t)).toBeTruthy();
    const context = await browser.newContext({
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${t.token}` },
    });
    const page = await context.newPage();
    const errors: string[] = [];
    page.on("pageerror", (e) => errors.push(e.message));
    page.on("dialog", (d) => d.accept()); // Delete confirms
    try {
      await page.goto("/networking");
      await expect(page.getByRole("heading", { name: "Networking" })).toBeVisible();
      await expect(page.getByText(/not allowed: readOvsStatus/)).toHaveCount(0);
      await expect(page.getByText("Open vSwitch:")).toHaveCount(0);

      // Only the owner's project is offered; no host network.
      await page.getByRole("button", { name: "Add Network" }).click();
      const scope = field(page, "Network for");
      await expect(scope.locator("option")).toHaveText([`Project ${PROJECT}`]);
      await page.getByPlaceholder("lab").fill(OWNER_NETWORK);
      await page.getByRole("button", { name: "Create", exact: true }).click();
      // Without host rights the table shows how the project gets a network.
      const row = page.getByRole("row", { name: new RegExp(OWNER_NETWORK) });
      await expect(row.getByRole("cell").nth(1)).toHaveText("project", { timeout: 30_000 });
      await expect(row).toContainText("nat");

      await row.getByRole("button", { name: "Delete" }).click();
      await expect(page.getByRole("row", { name: new RegExp(OWNER_NETWORK) })).toHaveCount(0, { timeout: 30_000 });
      expect(errors).toEqual([]);
    } finally {
      await context.close();
    }
  });
});
