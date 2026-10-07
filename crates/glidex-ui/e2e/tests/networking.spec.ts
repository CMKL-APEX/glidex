// Project networks from the Networking page (spec/web-ui.md, security.md
// §6.2): the project page links to the shared "Add Network" form with the
// project chosen; project networks are NAT or isolated, never bridged.
// Needs glidex-netd and Open vSwitch; no VM is booted.
import { api, del, expect, field, test } from "../fixtures";

const PROJECT = "e2e-netproj";
const NETWORK = "e2e-iso";

async function cleanup() {
  const n = (await api<any[]>("/networks")).find((x) => x.name === NETWORK);
  if (n) await del(`/networks/${NETWORK}?wait=30`);
  const p = (await api<any[]>("/projects")).find((x) => x.name === PROJECT);
  if (p) await del(`/projects/${p.id}`);
}

test.beforeAll(cleanup);
test.afterAll(cleanup);

test("a project network is created from the Networking page", async ({ page }) => {
  const status = await api("/ovs/status");
  test.skip(status?.netd?.access !== "full" || status?.host?.ovs_running !== true, "needs glidex-netd and a running Open vSwitch");

  const project = await api("/projects", { method: "POST", body: { name: PROJECT } });
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
  await mode.selectOption("isolated");
  await page.getByRole("button", { name: "Create", exact: true }).click();

  const row = page.getByRole("row", { name: new RegExp(NETWORK) });
  await expect(row).toContainText(PROJECT, { timeout: 30_000 });
  await expect(row).toContainText("isolated");
  await expect(row).toContainText(/gxp-[0-9a-f]{8}/);
  const net = await api(`/networks/${NETWORK}`);
  expect([net.project, net.mode, net.port_type]).toEqual([project.id, "isolated", "tap"]);

  // The project page lists it; its owner deletes it from the Networking page.
  await page.goto(`/projects/${project.id}`);
  await expect(page.getByText(NETWORK).first()).toBeVisible();
  await page.goto("/networking");
  await page.getByRole("row", { name: new RegExp(NETWORK) }).getByRole("button", { name: "Delete" }).click();
  await expect(page.getByRole("row", { name: new RegExp(NETWORK) })).toHaveCount(0, { timeout: 30_000 });
});
