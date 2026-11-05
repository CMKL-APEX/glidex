// Browsing asks only what the caller may read: a project member without a
// host role opens the pages that read host state for those who have one,
// and the audit has no denial from it (spec/11-security.md §10). Seen
// through a personal token narrowed to one project role. No VM is booted.
import { api, del, expect, test } from "../fixtures";

const PROJECT = "e2e-no-denials";
const TOKEN = "e2e-no-denials";

async function cleanup() {
  for (const t of ((await api<{ tokens?: any[] }>("/tokens")).tokens ?? []).filter((t) => t.name.startsWith(TOKEN)))
    await del(`/tokens/${t.id}`);
  const p = ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((x) => x.name === PROJECT);
  if (p) await del(`/projects/${p.id}`);
}

test.beforeAll(cleanup);
test.afterAll(cleanup);

test("a project editor browses without a denial in the audit", async ({ browser }) => {
  const me = await api("/auth/whoami");
  const project = (await api("/projects", { method: "POST", body: { name: PROJECT } })).project;
  await api(`/projects/${project.id}/bindings`, {
    method: "POST",
    body: { role: "role.editor", principal: { type: "User", id: me.user.id } },
  });
  const t = await api("/tokens", {
    method: "POST",
    body: { name: TOKEN, kind: "TOKEN_KIND_PERSONAL", expires_in_days: 1, roles: [{ role: "role.editor", project: project.id }] },
  });
  const since = Date.now();
  const editor = await browser.newContext({
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: { Authorization: `Bearer ${t.token}` },
  });
  try {
    const page = await editor.newPage();
    // Networking: the host's OVS status is for host readers.
    await page.goto("/networking");
    await expect(page.getByRole("heading", { name: "Networking" }).first()).toBeVisible();
    // Account and Access: node names are for those who may list nodes.
    await page.goto("/account");
    await expect(page.locator("main")).toBeVisible();
    await page.goto("/access");
    await expect(page.locator("main")).toBeVisible();
    // The token form: other organizations' clusters, per tenant.
    await page.goto("/tokens");
    await page.getByRole("button", { name: "+ Create Token" }).click();
    await expect(page.locator("#token-name")).toBeVisible();
    // The create form: the host's GPUs are for host readers.
    await page.goto("/");
    await page.getByRole("button", { name: "+ Create VM" }).click();
    await expect(page.getByRole("heading", { name: "Create New VM" })).toBeVisible();
    // Let the form's requests land; the live stream keeps the network busy,
    // so there is no idle to wait for.
    await page.waitForTimeout(1500);
  } finally {
    await editor.close();
  }

  const audit = await api(`/audit?since=${since}&limit=5000`);
  const denied = (audit.entries ?? []).filter((e: any) => e.result === "denied").map((e: any) => e.action);
  expect(denied).toEqual([]);
});
