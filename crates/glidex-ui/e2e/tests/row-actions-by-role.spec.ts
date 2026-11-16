// The disks, credentials and images pages show a row's actions only to a
// caller its action allows (spec/11-security.md §7.2): a viewer sees the
// disks but no buttons, and no credentials (they are an editor's to read);
// an editor changes and deletes the project's disks and credentials;
// pulling and deleting images is a host role's. Seen
// through personal tokens narrowed to one project role. No VM is booted.
import { api, del, expect, test } from "../fixtures";

const PROJECT = "e2e-row-roles";
const TOKEN = "e2e-row-roles";
const DISK = "e2e-row-roles-disk";
const USER = "e2e-row-roles-user";
const KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOMqqnkVzrm0SdG6UOoqKLsabgH5C9okWi0dh2l9GKJl e2e";

async function cleanup() {
  for (const t of ((await api<{ tokens?: any[] }>("/tokens")).tokens ?? []).filter((t) => t.name.startsWith(TOKEN)))
    await del(`/tokens/${t.id}`);
  const p = ((await api<{ projects?: any[] }>("/projects")).projects ?? []).find((x) => x.name === PROJECT);
  if (p) {
    for (const d of ((await api<{ disks?: any[] }>(`/disks?project=${p.id}`)).disks ?? []).filter((d) => d.project === p.id))
      await del(`/disks/${d.id}`);
    await del(`/credentials/${USER}?project=${p.id}`);
    await del(`/projects/${p.id}`);
  }
}

test.beforeAll(cleanup);
test.afterAll(cleanup);
test.use({ ignoredConsoleErrors: /status of 403/ });

test("a viewer sees no row actions; an editor changes disks and credentials", async ({ browser }) => {
  const me = await api("/auth/whoami");
  const project = (await api("/projects", { method: "POST", body: { name: PROJECT } })).project;
  const made = await api("/disks", { method: "POST", body: { name: DISK, size_bytes: 64 * 1024 * 1024, project: project.id } });
  expect(made?.disk?.id, JSON.stringify(made)).toBeTruthy();
  await api("/credentials", { method: "POST", body: { username: USER, project: project.id, ssh_authorized_keys: [KEY] } });

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
  const row = (page: any, text: string) => page.getByRole("row").filter({ hasText: text });

  const viewer = await as("role.viewer");
  try {
    const page = await viewer.newPage();
    await page.goto("/disks");
    await expect(row(page, DISK)).toBeVisible();
    await expect(row(page, DISK).getByRole("button", { name: "Resize" })).toHaveCount(0);
    await expect(row(page, DISK).getByRole("button", { name: "Delete" })).toHaveCount(0);
    await page.goto("/images");
    await expect(page.getByRole("heading", { name: "Images", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Download from URL" })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^(Pull|Import)$/ })).toHaveCount(0);
    await expect(page.getByRole("button", { name: /^(Delete|Cancel)$/ })).toHaveCount(0);
  } finally {
    await viewer.close();
  }

  const editor = await as("role.editor");
  try {
    const page = await editor.newPage();
    await page.goto("/disks");
    await expect(row(page, DISK).getByRole("button", { name: "Resize" })).toBeVisible();
    await expect(row(page, DISK).getByRole("button", { name: "Delete" })).toBeVisible();
    await page.goto("/credentials");
    await expect(row(page, USER).getByRole("button", { name: "Edit" })).toBeVisible();
    await expect(row(page, USER).getByRole("button", { name: "Delete" })).toBeVisible();
    // Images are a host role's, not a project's.
    await page.goto("/images");
    await expect(page.getByRole("heading", { name: "Images", exact: true })).toBeVisible();
    await expect(page.getByRole("button", { name: "Download from URL" })).toHaveCount(0);
  } finally {
    await editor.close();
  }
});
