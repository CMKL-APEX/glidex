// The project page's Members table for a caller without host rights
// (spec/web-ui.md): principals show their names, from the links
// themselves, although the caller can't list users.
import { api, del, expect, test } from "../fixtures";

const PROJECT = "e2e-members";
const TOKEN = "e2e-members-owner";

async function cleanup() {
  for (const t of (await api<any[]>("/tokens")).filter((t) => t.name === TOKEN)) await del(`/tokens/${t.id}`);
  const p = (await api<any[]>("/projects")).find((x) => x.name === PROJECT);
  if (p) await del(`/projects/${p.id}`);
}

test.beforeAll(cleanup);
test.afterAll(cleanup);
test.use({ ignoredConsoleErrors: /status of 403/ });

test("members show names to a project owner without host rights", async ({ browser }) => {
  const me = await api("/auth/whoami");
  const project = await api("/projects", { method: "POST", body: { name: PROJECT } });
  await api(`/projects/${project.id}/bindings`, {
    method: "POST",
    body: { role: "role.owner", principal: { type: "User", id: me.user.id } },
  });
  const t = await api("/tokens", {
    method: "POST",
    body: { name: TOKEN, kind: "personal", expires_in_days: 1, roles: [{ role: "role.owner", project: project.id }] },
  });
  const context = await browser.newContext({
    storageState: { cookies: [], origins: [] },
    extraHTTPHeaders: { Authorization: `Bearer ${t.token}` },
  });
  const page = await context.newPage();
  try {
    await page.goto(`/projects/${project.id}`);
    const members = page.locator("section").filter({ has: page.getByRole("heading", { name: "Members" }) });
    await expect(members.getByRole("row").filter({ hasText: me.user.display_name })).toContainText("owner");
    await expect(members.getByText(`user ${me.user.id.slice(0, 8)}`)).toHaveCount(0);
    // The narrowed token's own link shows the token's name.
    await expect(members.getByText(`token ${TOKEN}`)).toBeVisible();
  } finally {
    await context.close();
  }
});
