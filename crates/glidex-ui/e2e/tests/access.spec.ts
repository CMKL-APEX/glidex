// The access-control pages (spec/security.md §15 S6) as a system-admin:
// projects with members and quotas, tokens, site policies with validate,
// simulate and save, and the audit log. No VM is booted.
import { api, del, expect, test } from "../fixtures";

test.use({ ignoredConsoleErrors: /status of 4\d\d/ });

const PROJECT = "e2e-proj";

async function cleanup() {
  const p = (await api<any[]>("/projects")).find((x) => x.name === PROJECT);
  if (p) await del(`/projects/${p.id}`);
  for (const t of (await api<any[]>("/tokens")).filter((t) => t.name.startsWith("e2e-"))) await del(`/tokens/${t.id}`);
  const policies = await api("/authz/policies");
  for (const s of policies.site.filter((s: any) => s.id.startsWith("site.e2e"))) {
    await del(`/authz/policies/${s.id}?version=${s.version}`);
  }
}

test.beforeAll(cleanup);
test.afterAll(cleanup);

test("projects: create, members, quotas, delete", async ({ page }) => {
  const me = await api("/auth/whoami");
  await page.goto("/projects");
  await expect(page.getByRole("link", { name: "default", exact: true })).toBeVisible();

  await page.getByRole("button", { name: "+ Create Project" }).click();
  await page.getByLabel("Name").fill(PROJECT);
  await page.getByLabel("Description").fill("made by the e2e suite");
  await page.getByRole("button", { name: "Create Project", exact: true }).click();
  await page.getByRole("link", { name: PROJECT, exact: true }).click();
  await expect(page.getByRole("heading", { name: PROJECT })).toBeVisible();

  // Give the test user the editor role, then take it away.
  const members = page.locator("section", { has: page.getByRole("heading", { name: "Members" }) });
  await expect(members.getByText("No role links.")).toBeVisible();
  await members.locator("select").nth(0).selectOption("role.editor");
  await members.locator("select").nth(2).selectOption(me.user.id);
  await members.getByRole("button", { name: "Add role" }).click();
  await expect(members.locator("tbody").getByText("editor", { exact: true })).toBeVisible();
  await expect(members.getByRole("cell", { name: me.user.display_name })).toBeVisible();
  await members.getByRole("button", { name: "Remove" }).click();
  await expect(members.getByText("No role links.")).toBeVisible();

  await page.getByRole("button", { name: "Edit quotas" }).click();
  await page.locator("#quota-vms").fill("5");
  await page.getByRole("button", { name: "Save quotas" }).click();
  await expect(page.getByText("/ 5")).toBeVisible();
  const p = (await api<any[]>("/projects")).find((x) => x.name === PROJECT);
  expect(p.quotas.vms).toBe(5);

  await page.getByRole("button", { name: "Delete project" }).click();
  await expect(page).toHaveURL(/\/projects$/);
  await expect(page.getByRole("link", { name: PROJECT, exact: true })).toHaveCount(0);
});

test("tokens: the secret is shown once; revoke", async ({ page }) => {
  await page.goto("/tokens");
  await page.getByRole("button", { name: "+ Create Token" }).click();
  await page.getByLabel("Name").fill("e2e-token");
  await page.getByRole("button", { name: "Create Token", exact: true }).click();
  const secret = page.getByLabel("New token");
  await expect(secret).toHaveValue(/^gxt_[A-Za-z0-9]+$/);
  await expect(page.getByText("only once")).toBeVisible();
  await page.getByRole("button", { name: "Done" }).click();
  await expect(page.getByLabel("New token")).toHaveCount(0);

  const row = page.locator("tr", { hasText: "e2e-token" });
  await expect(row).toBeVisible();
  await row.getByRole("button", { name: "Revoke" }).click();
  await expect(page.locator("tr", { hasText: "e2e-token" })).toHaveCount(0);
});

test("policies: validate, simulate, save and delete a site policy", async ({ page }) => {
  const id = "site.e2e-console";
  await page.goto("/policies");
  await expect(page.getByText("base.step-up")).toBeVisible();

  await page.getByRole("button", { name: "+ New site policy" }).click();
  await page.locator("#policy-id").fill(id);
  const text = page.getByLabel("Policy text");

  await text.fill(`@id("${id}")\nforbid (principal, action == Glidex::Action::"noSuchAction", resource);`);
  await page.getByRole("button", { name: "Validate" }).click();
  await expect(page.getByText("Invalid")).toBeVisible();

  // Forbid consoles for everyone: the simulator shows the change.
  await text.fill(`@id("${id}")\nforbid (principal, action == Glidex::Action::"openConsole", resource);`);
  await page.getByRole("button", { name: "Validate" }).click();
  await expect(page.getByText("Valid against the schema.")).toBeVisible();

  const projects: any[] = await api("/projects");
  const dflt = projects.find((p) => p.name === "default");
  const sim = page.locator("section", { has: page.getByRole("heading", { name: "Simulate" }) });
  await sim.locator("input[list=policy-actions]").fill("createVm");
  await sim.getByRole("button", { name: "+ Add request" }).click();
  await sim.locator("input[list=policy-actions]").fill("readProject");
  await sim.getByRole("button", { name: "+ Add request" }).click();
  // The resource id defaults to the selected project.
  await expect(sim.getByText(`Project:${dflt.id.slice(0, 12)}`).first()).toBeVisible();
  await sim.getByRole("button", { name: "Run simulation" }).click();
  await expect(sim.getByText("allow").first()).toBeVisible();

  await page.getByRole("button", { name: "Save" }).click();
  await expect(page.getByText(/Saved as version 1/)).toBeVisible();
  const listing = await api("/authz/policies");
  expect(listing.site.map((s: any) => s.id)).toContain(id);

  // A stale version is refused with a conflict.
  const stale = await page.evaluate(
    async ([pid, csrf]) =>
      (
        await fetch(`/api/authz/policies/${pid}`, {
          method: "PUT",
          headers: { "Content-Type": "application/json", "X-Glidex-CSRF": csrf },
          body: JSON.stringify({ text: `@id("${pid}")\nforbid (principal, action == Glidex::Action::"openConsole", resource);`, version: 0 }),
        })
      ).status,
    [id, (await page.evaluate(() => fetch("/api/auth/whoami").then((r) => r.json()))).csrf] as const,
  );
  expect(stale).toBe(409);

  await page.getByRole("button", { name: "History" }).click();
  await expect(page.getByText("v1", { exact: true })).toBeVisible();
  await page.getByRole("button", { name: "Load into editor" }).click();

  await page.getByRole("button", { name: "Delete" }).click();
  await expect.poll(async () => (await api("/authz/policies")).site.map((s: any) => s.id)).not.toContain(id);
});

test("audit: lists recent writes", async ({ page }) => {
  await page.goto("/audit");
  await expect(page.getByRole("heading", { name: "Audit log" })).toBeVisible();
  // Earlier tests created and deleted things.
  await expect(page.locator("td", { hasText: /^createProject$|^writePolicy$|^manageTeams$/ }).first()).toBeVisible();
});
