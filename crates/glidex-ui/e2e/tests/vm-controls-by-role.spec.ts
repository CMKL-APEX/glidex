// The VM page shows each control only to a caller its action allows
// (spec/11-security.md §7.2): an operator runs a VM (Start, the console)
// but doesn't change it; an editor also changes, attaches and deletes. Seen
// through personal tokens narrowed to one project role.
import { api, createVm, del, expect, test } from "../fixtures";

const PROJECT = "e2e-vm-roles";
const TOKEN = "e2e-vm-roles";

async function cleanup() {
  for (const t of ((await api<{ tokens?: any[] }>("/tokens")).tokens ?? []).filter((t) => t.name.startsWith(TOKEN)))
    await del(`/tokens/${t.id}`);
  const p = ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((x) => x.name === PROJECT);
  if (p) {
    for (const vm of ((await api<{ vms?: any[] }>("/vms")).vms ?? []).filter((v) => v.project === p.id))
      await del(`/vms/${vm.id}?wait=60`);
    await del(`/projects/${p.id}`);
  }
}

test.beforeAll(cleanup);
test.afterAll(cleanup);
test.use({ ignoredConsoleErrors: /status of 403/ });

test("an operator runs a VM; an editor also changes it", async ({ browser, hypervisor }) => {
  const me = await api("/auth/whoami");
  const project = (await api("/projects", { method: "POST", body: { name: PROJECT } })).project;
  const vm = await createVm({
    name: `roles-${hypervisor}`,
    project: project.id,
    vcpu_count: 1,
    mem_size_mib: 256,
    kernel_image_path: "/nonexistent/vmlinux",
    rootfs_path: "/nonexistent/disk.img",
  });
  expect(vm.id, JSON.stringify(vm)).toBeTruthy();

  const as = async (role: string) => {
    await api(`/projects/${project.id}/bindings`, {
      method: "POST",
      body: { role, principal: { type: "User", id: me.user.id } },
    });
    const t = await api("/tokens", {
      method: "POST",
      body: { name: `${TOKEN}-${role}`, kind: "TOKEN_KIND_PERSONAL", expires_in_days: 1, roles: [{ role, project: project.id }] },
    });
    return browser.newContext({
      storageState: { cookies: [], origins: [] },
      extraHTTPHeaders: { Authorization: `Bearer ${t.token}` },
    });
  };

  const operator = await as("role.operator");
  try {
    const page = await operator.newPage();
    await page.goto(`/vms/${vm.id}`);
    await expect(page.getByRole("button", { name: "Start" })).toBeVisible();
    await expect(page.getByRole("link", { name: "Open Console" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Change memory" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Scale CPU / Elasticity" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: "Delete" })).toHaveCount(0);
    await expect(page.getByLabel("Disk to attach")).toHaveCount(0);
  } finally {
    await operator.close();
  }

  const editor = await as("role.editor");
  try {
    const page = await editor.newPage();
    await page.goto(`/vms/${vm.id}`);
    await expect(page.getByRole("button", { name: "Start" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Change memory" })).toBeVisible();
    await expect(page.getByRole("button", { name: "Delete" })).toBeVisible();
  } finally {
    await editor.close();
  }
});
